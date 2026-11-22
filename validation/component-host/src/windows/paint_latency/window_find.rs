//! Metadata-only B Find owner, distinct from A's menu/Source owner. A returned
//! Stop on the second search is not treated as a native final or retirement.
use super::*;
use rust_cef_gpui::{
    ContextMenuRegistration,
    find::{
        FindLimits, FindOperationId, FindOutcome, FindPhase, FindReceipt, FindRequest, FindResult,
        FindResultSink, FindStatus,
    },
    ports::{BrowserHandle, ContextMenuTarget, WindowBinding},
    test_support::{
        ContextMenuResponse, MenuBridgeFixture, MenuLeaseOwner, menu_bridge_fixture_url,
    },
};

const SELECTION: &str = "menu bridge selection";

pub(super) fn url() -> Result<String, String> {
    let original = menu_bridge_fixture_url();
    let peer = original.replace("26%2C76%2C126", "110%2C40%2C180");
    if peer == original {
        return Err("fixed B Find fixture did not change background".into());
    }
    Ok(peer)
}

#[derive(Default)]
struct Inbox {
    receipts: Vec<Arc<dyn FindReceipt>>,
    finals: Vec<FindResult>,
    terminals: Vec<FindStatus>,
}
struct Sink {
    binding: WindowBinding,
    inbox: Mutex<Inbox>,
}
impl FindResultSink for Sink {
    fn started(&self, r: FindRequest, receipt: Arc<dyn FindReceipt>) -> Result<(), EngineError> {
        let q = receipt.status();
        if q.operation != r.operation
            || q.menu != r.menu
            || r.context.binding != self.binding
            || r.operation.browser != self.binding.browser
            || r.context.selection != SELECTION
            || r.context.frame_id.is_empty()
        {
            return Err(EngineError::InvalidRequest);
        }
        let mut i = self.inbox.lock().map_err(|_| EngineError::InvalidRequest)?;
        if i.receipts.len() >= 2 {
            return Err(EngineError::Backpressure);
        }
        i.receipts.push(receipt);
        Ok(())
    }
    fn update(&self, r: FindResult) -> Result<(), EngineError> {
        if r.operation.browser != self.binding.browser {
            return Err(EngineError::InvalidRequest);
        }
        if r.final_update {
            let mut i = self.inbox.lock().map_err(|_| EngineError::InvalidRequest)?;
            if i.finals.len() >= 2 {
                return Err(EngineError::Backpressure);
            }
            i.finals.push(r);
        }
        Ok(())
    }
    fn finished(&self, q: FindStatus) -> Result<(), EngineError> {
        if q.operation.browser != self.binding.browser {
            return Err(EngineError::InvalidRequest);
        }
        let mut i = self.inbox.lock().map_err(|_| EngineError::InvalidRequest)?;
        if i.terminals.len() >= 2 {
            return Err(EngineError::Backpressure);
        }
        i.terminals.push(q);
        Ok(())
    }
}

pub(super) struct Held {
    owner: MenuLeaseOwner,
    registration: ContextMenuRegistration,
    menu: Arc<MenuBridgeFixture>,
    sink: Arc<Sink>,
    first: FindOperationId,
    repeat: Arc<dyn FindReceipt>,
    original: FindOperationId,
    after_a: bool,
    during_b_paint: bool,
}

pub(super) fn key(id: FindOperationId) -> [u64; 3] {
    [
        id.browser.runtime().value(),
        id.browser.local(),
        id.sequence,
    ]
}

impl Held {
    pub(super) async fn start(
        s: &mut Scene<'_>,
        peer: &BrowserHandle,
        window: u64,
        deadline: Instant,
    ) -> Result<Self, String> {
        let id = peer.snapshot().browser;
        let target = WindowBinding {
            browser: id,
            window,
        };
        s.rt.configure_finds(FindLimits {
            capacity: NonZeroUsize::MIN,
            max_query_bytes: NonZeroUsize::new(64).unwrap(),
        })
        .map_err(|e| format!("configure original B Find: {e:?}"))?;
        let owner = MenuLeaseOwner::new(target, NonZeroUsize::MIN)
            .map_err(|e| format!("B menu lease: {e:?}"))?;
        let menu = MenuBridgeFixture::new(owner.lease());
        let registration =
            s.rt.bind_context_menu(ContextMenuTarget {
                lease: owner.lease(),
                delivery: menu.clone(),
            })
            .map_err(|e| format!("bind original B menu: {e:?}"))?;
        let sink = Arc::new(Sink {
            binding: target,
            inbox: Mutex::new(Inbox::default()),
        });
        registration
            .set_find_results(sink.clone())
            .map_err(|e| format!("bind original B Find: {e:?}"))?;
        for n in 1..=3 {
            let (posted, command) = if n == 1 {
                (s.rt.probe_menu_bridge_open(id, false), 117)
            } else {
                (s.rt.probe_menu_find_open(id), 130)
            };
            posted.map_err(|e| format!("CEF-local B menu {n}: {e:?}"))?;
            let delivery = window_menu::Pending::delivery(
                s,
                &registration,
                &menu,
                target,
                n,
                command,
                deadline,
            )
            .await?;
            if n > 1 && delivery.presentation.context.selection != SELECTION {
                return Err("B Find did not use original selected text".into());
            }
            delivery
                .response
                .submit(
                    delivery.presentation.request,
                    ContextMenuResponse::Select {
                        command_id: command,
                    },
                )
                .map_err(|e| format!("B menu reply {n}: {e:?}"))?;
            loop {
                let d = s.rt.find_diagnostics();
                if d.faulted || d.first_failure.is_some() {
                    return Err("native B Find failed".into());
                }
                let done = match n {
                    1 => {
                        registration.status().actions_submitted == 1
                            && peer.snapshot().navigation.title == "menu-bridge-selected"
                    }
                    2 => {
                        d.started == 1
                            && d.finished == 1
                            && d.pending == 0
                            && sink.inbox.lock().unwrap().terminals.len() == 1
                    }
                    _ => {
                        d.started == 2
                            && d.finished == 1
                            && d.pending == 1
                            && s.rt.diagnostics().tasks.pending == 0
                    }
                };
                if done {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(format!("B Find phase {n} timeout"));
                }
                s.service(false, None).await?;
            }
        }
        let receipts = sink.inbox.lock().unwrap().receipts.clone();
        let [first, repeat] = receipts.as_slice() else {
            return Err("expected two original B Find receipts".into());
        };
        let f = first.status();
        let r = repeat.status();
        if f.operation.browser != id
            || r.operation.browser != id
            || f.operation.sequence == 0
            || f.operation.sequence >= r.operation.sequence
            || f.outcome != Some(FindOutcome::Result)
            || f.first_failure.is_some()
            || !f
                .last_result
                .is_some_and(|o| o.operation == f.operation && o.final_update && o.matches == 1)
            || !r.issued
            || r.outcome.is_some()
            || r.last_result.is_some()
            || r.first_failure.is_some()
        {
            return Err(
                "B did not finish first real result and leave original repeat unresolved".into(),
            );
        }
        repeat
            .cancel()
            .map_err(|e| format!("original B Stop admission: {e:?}"))?;
        loop {
            let q = repeat.status();
            if q.stop_submitted && s.rt.diagnostics().tasks.pending == 0 {
                break;
            }
            if q.outcome.is_some() || Instant::now() >= deadline {
                return Err("B resolved or Stop completion timeout".into());
            }
            s.service(false, None).await?;
        }
        let held = Self {
            owner,
            registration,
            menu,
            sink,
            first: f.operation,
            repeat: repeat.clone(),
            original: r.operation,
            after_a: false,
            during_b_paint: false,
        };
        held.pending(s.rt)?;
        Ok(held)
    }

    pub(super) fn pending(&self, rt: &Runtime) -> Result<FindOperationId, String> {
        let q = self.repeat.status();
        let d = rt.find_diagnostics();
        if q.operation != self.original
            || !q.issued
            || !q.stop_attempted
            || !q.stop_submitted
            || q.last_result.is_some()
            || q.outcome.is_some()
            || q.first_failure.is_some()
            || d.faulted
            || d.first_failure.is_some()
            || d.started != 2
            || d.finished != 1
            || d.pending != 1
            || !matches!(d.active.as_slice(),[r] if r.operation==self.original)
        {
            return Err("original B Find did not stay unresolved across A close/paint".into());
        }
        Ok(self.original)
    }

    pub(super) fn sample(&mut self, rt: &Runtime, paint: bool) -> Result<(), String> {
        self.pending(rt)?;
        if paint {
            if !self.after_a {
                return Err("B repaint sample preceded actual A Closed".into());
            }
            self.during_b_paint = true;
        } else {
            self.after_a = true;
        }
        Ok(())
    }

    pub(super) fn retire(mut self, rt: &Runtime) -> Result<String, String> {
        let q = self.repeat.status();
        let d = rt.find_diagnostics();
        let m = self.menu.snapshot();
        let s = self.registration.status();
        if !self.after_a
            || !self.during_b_paint
            || q.operation != self.original
            || q.phase != FindPhase::Finished
            || q.outcome != Some(FindOutcome::Cancelled)
            || q.last_result.is_some()
            || q.first_failure.is_some()
            || d.started != 2
            || d.finished != 2
            || d.pending != 0
            || !d.active.is_empty()
            || d.faulted
            || d.first_failure.is_some()
            || !s.retired
            || s.published != 3
            || s.cancelled != 3
            || s.actions_submitted != 3
            || s.first_failure.is_some()
            || m.published != 3
            || m.terminal != 3
            || m.outstanding != 0
            || m.first_error.is_some()
            || !matches!(m.terminal_ids.as_slice(),[x,y,z] if 0<*x && x<y && y<z && q.menu.sequence==*z)
        {
            return Err("B's original unresolved receipt did not cancel on its own close".into());
        }
        {
            let i = self.sink.inbox.lock().unwrap();
            if !matches!(i.finals.as_slice(),[f] if f.operation==self.first && f.final_update && f.matches==1)
                || !matches!(i.terminals.as_slice(),[a,b] if a.operation==self.first && a.outcome==Some(FindOutcome::Result)
                    && b.operation==self.original && b.outcome==Some(FindOutcome::Cancelled))
            {
                return Err("B host result/termination delivery did not correlate".into());
            }
        }
        self.owner
            .retire()
            .map_err(|e| format!("retire original B Find menu lease: {e:?}"))?;
        if !self.owner.lease().snapshot().is_ok_and(|q| q.is_drained()) {
            return Err("B Find menu lease did not drain".into());
        }
        Ok(object(vec![
            ("first_final", format!("{:?}", key(self.first))),
            ("first_match_count", "1".into()),
            ("original_repeat", format!("{:?}", key(self.original))),
            (
                "configured_gpui_window",
                self.sink.binding.window.to_string(),
            ),
            ("unresolved_after_a_closed", "true".into()),
            ("unresolved_after_b_paint", "true".into()),
            ("cancelled_after_b_closed", "true".into()),
            ("final_find_pending", "0".into()),
            ("late_results_observed", d.late_results.to_string()),
            ("menu_terminal_ids", format!("{:?}", m.terminal_ids)),
            ("menu_actions", s.actions_submitted.to_string()),
            ("host_lease_drained", "true".into()),
        ]))
    }
}
