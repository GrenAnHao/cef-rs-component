//! Explicit-only windowless managed Find fixture. No menu widget, OS input,
//! new Find product UI, printer, source viewer, or native handle escapes.
use rust_cef_core::{
    contracts::ThreadMode,
    find::*,
    menu::{ContextMenuResponse, MenuLimits},
    menu_context::MenuContextLimits,
    menu_lease::MenuLeaseOwner,
    ports::{BrowserHandle, ContextMenuTarget, EngineError, WindowBinding},
};
use rust_cef_runtime::{
    ContextMenuConfig, ContextMenuRegistration, menu_bridge_fixture::MenuBridgeFixture,
    runtime::Runtime,
};
use std::{
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
type Tick = fn(&mut Runtime, ThreadMode) -> Result<(), String>;
#[derive(Default)]
struct Inbox {
    receipts: Vec<Arc<dyn FindReceipt>>,
    query_bytes: Vec<usize>,
    frame_bytes: Vec<usize>,
    finals: Vec<FindResult>,
    finished: Vec<FindStatus>,
    threads: Vec<u64>,
}
#[derive(Default)]
struct Results(Mutex<Inbox>);
impl FindResultSink for Results {
    fn started(
        &self,
        request: FindRequest,
        receipt: Arc<dyn FindReceipt>,
    ) -> Result<(), EngineError> {
        let status = receipt.status();
        if status.operation != request.operation || status.menu != request.menu {
            return Err(EngineError::InvalidRequest);
        }
        let mut s = self.0.lock().unwrap();
        if s.receipts.len() >= 2 {
            return Err(EngineError::Backpressure);
        }
        s.query_bytes.push(request.context.selection.len());
        s.frame_bytes.push(request.context.frame_id.len());
        s.receipts.push(receipt);
        Ok(())
    }
    fn update(&self, result: FindResult) -> Result<(), EngineError> {
        let tid = cef::get_current_platform_thread_id() as u64;
        let mut s = self.0.lock().unwrap();
        if result.final_update {
            if s.finals.len() >= 2 {
                return Err(EngineError::Backpressure);
            }
            s.finals.push(result);
            s.threads.push(tid);
        }
        Ok(())
    }
    fn finished(&self, status: FindStatus) -> Result<(), EngineError> {
        let mut s = self.0.lock().unwrap();
        if s.finished.len() >= 2 {
            return Err(EngineError::Backpressure);
        }
        s.finished.push(status);
        Ok(())
    }
}
use rust_cef_runtime::find_probe_report::FindProbeReport as Report;
pub struct FindProbe {
    owner: Option<MenuLeaseOwner>,
    binding: Option<ContextMenuRegistration>,
    menu: Option<Arc<MenuBridgeFixture>>,
    results: Arc<Results>,
    report: Report,
}
impl FindProbe {
    pub fn new() -> Self {
        Self {
            owner: None,
            binding: None,
            menu: None,
            results: Arc::new(Results::default()),
            report: Report {
                metadata_owner_only: true,
                ..Default::default()
            },
        }
    }
    pub fn run(
        &mut self,
        rt: &mut Runtime,
        mode: ThreadMode,
        handle: &BrowserHandle,
        tick: Tick,
    ) -> Result<(), String> {
        Self::configure(rt, NonZeroUsize::MIN)?;
        self.run_configured(rt, mode, handle, tick, true)
    }

    pub(super) fn configure(rt: &mut Runtime, capacity: NonZeroUsize) -> Result<(), String> {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        rt.configure_context_menus(ContextMenuConfig {
            capacity,
            model_limits: MenuLimits {
                max_entries: nz(32),
                max_label_bytes: nz(256),
                max_total_label_bytes: nz(2048),
            },
            context_limits: MenuContextLimits {
                max_frame_id_bytes: nz(512),
                max_text_bytes: nz(4096),
                max_suggestions: nz(5),
                max_total_bytes: nz(8192),
            },
        })
        .map_err(|e| format!("Find menu configure: {e:?}"))?;
        rt.configure_finds(FindLimits {
            capacity,
            max_query_bytes: nz(64),
        })
        .map_err(|e| format!("Find configure: {e:?}"))
    }

    /// `repeat` is used on the original Browser only. A peer waiting for its
    /// one real result never treats another Browser's pending row as its own.
    pub(super) fn run_configured(
        &mut self,
        rt: &mut Runtime,
        mode: ThreadMode,
        handle: &BrowserHandle,
        tick: Tick,
        repeat: bool,
    ) -> Result<(), String> {
        let browser = handle.snapshot().browser;
        let baseline = rt.find_diagnostics();
        let owner = MenuLeaseOwner::new(
            WindowBinding {
                browser,
                window: browser.local(),
            },
            NonZeroUsize::MIN,
        )
        .map_err(|e| format!("Find metadata owner: {e:?}"))?;
        let menu = MenuBridgeFixture::new(owner.lease());
        let binding = rt
            .bind_context_menu(ContextMenuTarget {
                lease: owner.lease(),
                delivery: menu.clone(),
            })
            .map_err(|e| format!("Find binding: {e:?}"))?;
        binding
            .set_find_results(self.results.clone())
            .map_err(|e| format!("Find result provider: {e:?}"))?;
        self.owner = Some(owner);
        self.binding = Some(binding);
        self.menu = Some(menu.clone());
        for opening in 1..=if repeat { 3 } else { 2 } {
            self.report.phase = opening;
            let deadline = Instant::now() + Duration::from_secs(20);
            if opening == 1 {
                rt.probe_menu_bridge_open(browser, false)
            } else {
                rt.probe_menu_find_open(browser)
            }
            .map_err(|e| format!("Find CEF menu input: {e:?}"))?;
            let delivery = loop {
                if let Some(e) = self.binding.as_ref().unwrap().status().first_failure {
                    return Err(format!("Find menu capture: {e}"));
                }
                if let Some(d) = menu.take() {
                    break d;
                }
                if Instant::now() >= deadline {
                    return Err("Find menu delivery timeout".into());
                }
                tick(rt, mode)?;
            };
            while menu.snapshot().published != opening as u64 || rt.diagnostics().tasks.pending != 0
            {
                if Instant::now() >= deadline {
                    return Err("Find menu callback return timeout".into());
                }
                tick(rt, mode)?;
            }
            if opening > 1 && delivery.presentation.context.selection.len() != 21 {
                return Err("Find did not capture original actual selection".into());
            }
            delivery
                .response
                .submit(
                    delivery.presentation.request,
                    ContextMenuResponse::Select {
                        command_id: if opening == 1 {
                            cef::MenuId::SELECT_ALL.get_raw()
                        } else {
                            130
                        },
                    },
                )
                .map_err(|e| format!("Find selection admission: {e:?}"))?;
            loop {
                let diag = rt.find_diagnostics();
                let ready = if opening == 1 {
                    self.binding.as_ref().unwrap().status().actions_submitted == 1
                        && handle.snapshot().navigation.title == "menu-bridge-selected"
                } else if opening == 2 {
                    diag.started == baseline.started + 1
                        && diag.finished == baseline.finished + 1
                        && diag.pending == baseline.pending
                        && self.results.0.lock().unwrap().finished.len() == 1
                } else {
                    diag.started == baseline.started + 2
                        && diag.pending == baseline.pending + 1
                        && rt.diagnostics().tasks.pending == 0
                };
                if ready {
                    break;
                }
                if let Some(e) = diag.first_failure {
                    return Err(format!("Find execution failure: {e:?}"));
                }
                if Instant::now() >= deadline {
                    return Err(format!("Find phase {opening} result timeout: {diag:?}"));
                }
                tick(rt, mode)?;
            }
        }
        let receipts = self.receipts();
        if !repeat {
            if receipts.len() != 1
                || !receipts[0].status().last_result.is_some_and(|r| {
                    r.operation.browser == browser && r.final_update && r.matches == 1
                })
                || menu.snapshot().terminal != 2
                || menu.snapshot().outstanding != 0
            {
                return Err(
                    "peer did not obtain one final result and two distinct menu terminals".into(),
                );
            }
            return Ok(());
        }
        if receipts.len() != 2 {
            return Err("Find receipts absent".into());
        }
        let first = receipts[0].status();
        let second = receipts[1].status();
        self.report.local_sequences = vec![first.operation.sequence, second.operation.sequence];
        self.report.menu_terminal_separate = menu.snapshot().terminal == 3
            && menu.snapshot().outstanding == 0
            && rt.find_diagnostics().pending == 1;
        self.report.repeat_pending_before_close = second.issued
            && second.outcome.is_none()
            && second.last_result.is_none()
            && second.first_failure.is_none();
        if !self.report.menu_terminal_separate || !self.report.repeat_pending_before_close {
            return Err("repeat Find was falsely resolved by old result/menu terminal".into());
        }
        receipts[1]
            .cancel()
            .map_err(|e| format!("Find explicit Stop admission: {e:?}"))?;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let q = receipts[1].status();
            if q.stop_submitted && rt.diagnostics().tasks.pending == 0 {
                self.report.stop_return_not_drain =
                    q.stop_attempted && q.outcome.is_none() && rt.find_diagnostics().pending == 1;
                break;
            }
            if Instant::now() >= deadline {
                return Err("Find Stop task timeout".into());
            }
            tick(rt, mode)?;
        }
        if !self.report.stop_return_not_drain {
            return Err("Find Stop incorrectly drained native/unresolved search".into());
        }
        self.report.phase = 4;
        Ok(())
    }
    pub(super) fn receipts(&self) -> Vec<Arc<dyn FindReceipt>> {
        self.results.0.lock().unwrap().receipts.clone()
    }

    pub(super) fn owner_evidence(
        &self,
    ) -> Result<rust_cef_runtime::find_probe_report::FindOwnerEvidence, String> {
        use rust_cef_runtime::find_probe_report::{FindOwnerEvidence, operation_key};
        let menu = self.menu.as_ref().ok_or("Find menu absent")?.snapshot();
        let s = self.results.0.lock().unwrap();
        let mut evidence = FindOwnerEvidence {
            menu,
            query_bytes: s.query_bytes.clone(),
            frame_id_bytes: s.frame_bytes.clone(),
            finished: s
                .finished
                .iter()
                .map(|q| (operation_key(q.operation), format!("{:?}", q.outcome)))
                .collect(),
            native_finals: s
                .finals
                .iter()
                .zip(&s.threads)
                .map(|(result, thread)| {
                    (
                        operation_key(result.operation),
                        result.cef_identifier,
                        result.matches,
                        result.active_ordinal,
                        *thread,
                    )
                })
                .collect(),
            started: Vec::new(),
            all_receipts_retired: false,
        };
        let receipts = s.receipts.clone();
        drop(s);
        let statuses: Vec<_> = receipts.iter().map(|r| r.status()).collect();
        evidence.started = statuses
            .iter()
            .map(|q| operation_key(q.operation))
            .collect();
        evidence.all_receipts_retired = !statuses.is_empty()
            && statuses
                .iter()
                .all(|q| q.phase == FindPhase::Finished && q.first_failure.is_none());
        Ok(evidence)
    }

    pub fn snapshot(&self, rt: &Runtime) -> Report {
        let mut r = self.report.clone();
        let d = rt.find_diagnostics();
        r.started = d.started;
        r.finished = d.finished;
        r.final_pending = d.pending;
        r.faulted = d.faulted;
        r.first_failure = d.first_failure.map(|e| format!("{e:?}"));
        let s = self.results.0.lock().unwrap();
        r.query_bytes = s.query_bytes.clone();
        r.frame_id_bytes = s.frame_bytes.clone();
        r.cef_identifiers = s.finals.iter().map(|v| v.cef_identifier).collect();
        r.matches = s.finals.iter().map(|v| v.matches).collect();
        r.active_ordinals = s.finals.iter().map(|v| v.active_ordinal).collect();
        r.callback_threads = s.threads.clone();
        r.outcomes = s
            .finished
            .iter()
            .map(|q| format!("{:?}", q.outcome))
            .collect();
        let receipts = s.receipts.clone();
        drop(s);
        r.operations_drained_after_close = receipts.len() == 2
            && receipts.iter().all(|c| {
                let q = c.status();
                q.phase == FindPhase::Finished && q.first_failure.is_none()
            });
        r
    }
}
