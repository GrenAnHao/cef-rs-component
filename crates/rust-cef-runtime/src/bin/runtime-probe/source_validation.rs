//! Windowless, metadata-only host adapter: actual CEF source acquisition but
//! NOT a GPUI source window or full ViewSource capability acceptance.
use rust_cef_core::{
    menu::{ContextMenuResponse, MenuLimits},
    menu_context::MenuContextLimits,
    menu_lease::MenuLeaseOwner,
    source_view::*,
};
use rust_cef_runtime::{
    ContextMenuConfig, ContextMenuRegistration,
    contracts::{SessionState, ThreadMode},
    menu_bridge_fixture::{BridgeDelivery, MenuBridgeFixture},
    ports::{BrowserHandle, ContextMenuTarget, EngineError, WindowBinding},
    runtime::Runtime,
    test_support::{LateManagedMenuReply, ManagedPressureDrain, ProbeBrowserEvent},
};
use std::{
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
type Tick = fn(&mut Runtime, ThreadMode) -> Result<(), String>;
use rust_cef_runtime::source_probe_report::SourceProbeReport;
#[derive(Default)]
struct Inbox {
    controls: Vec<Arc<dyn SourceViewControl>>,
    pending: Option<(SourceViewRequest, Arc<dyn SourceViewControl>)>,
    closing: Vec<Arc<dyn SourceViewControl>>,
    open: Option<SourceViewRequest>,
    publications: u64,
    close_requests: u64,
    close_acks: u64,
    error: Option<String>,
}
struct Viewer(Mutex<Inbox>);
impl SourceViewSink for Viewer {
    fn started(
        &self,
        id: SourceOperationId,
        menu: rust_cef_core::menu::ContextMenuRequestId,
        c: Arc<dyn SourceViewControl>,
    ) -> Result<(), EngineError> {
        let mut s = self.0.lock().unwrap();
        if s.controls.len() >= 2 || id.browser != menu.browser || c.status().operation != id {
            s.error
                .get_or_insert("operation start correlation/capacity".into());
            return Err(EngineError::Backpressure);
        }
        s.controls.push(c);
        Ok(())
    }
    fn publish(
        &self,
        request: SourceViewRequest,
        control: Arc<dyn SourceViewControl>,
    ) -> Result<(), EngineError> {
        let mut s = self.0.lock().unwrap();
        if s.pending.is_some()
            || s.publications >= 2
            || !s
                .controls
                .iter()
                .any(|c| c.status().operation == request.operation)
        {
            s.error
                .get_or_insert("source publication correlation/capacity".into());
            return Err(EngineError::Backpressure);
        }
        s.publications += 1;
        s.pending = Some((request, control));
        Ok(())
    }
    fn close(
        &self,
        id: SourceOperationId,
        c: Arc<dyn SourceViewControl>,
    ) -> Result<(), EngineError> {
        let mut s = self.0.lock().unwrap();
        if s.closing.len() >= 1 || c.status().operation != id {
            s.error
                .get_or_insert("source close correlation/capacity".into());
            return Err(EngineError::Backpressure);
        }
        s.close_requests += 1;
        s.closing.push(c);
        Ok(())
    }
}
pub struct SourceProbe {
    owner: Option<MenuLeaseOwner>,
    binding: Option<ContextMenuRegistration>,
    menu: Option<Arc<MenuBridgeFixture>>,
    viewer: Arc<Viewer>,
    report: SourceProbeReport,
    hold_visitor: bool,
}
impl SourceProbe {
    pub fn new(budget: bool) -> Self {
        Self {
            owner: None,
            binding: None,
            menu: None,
            viewer: Arc::new(Viewer(Mutex::new(Inbox::default()))),
            report: SourceProbeReport {
                metadata_owner_only: true,
                budget_case: budget,
                ..Default::default()
            },
            hold_visitor: false,
        }
    }
    pub fn with_held_visitor() -> Self {
        Self {
            hold_visitor: true,
            ..Self::new(false)
        }
    }
    pub fn run(
        &mut self,
        rt: &mut Runtime,
        mode: ThreadMode,
        handle: &BrowserHandle,
        tick: Tick,
    ) -> Result<(), String> {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        rt.configure_context_menus(ContextMenuConfig {
            capacity: nz(1),
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
        .map_err(|e| format!("source menu config: {e:?}"))?;
        self.run_configured(rt, mode, handle, tick)
    }

    /// Joint fixture uses its own already configured two-Browser menu table.
    pub(super) fn run_configured(
        &mut self,
        rt: &mut Runtime,
        mode: ThreadMode,
        handle: &BrowserHandle,
        tick: Tick,
    ) -> Result<(), String> {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        let browser = handle.snapshot().browser;
        rt.configure_source_views(SourceViewLimits {
            capacity: nz(1),
            max_source_bytes: nz(if self.report.budget_case { 32 } else { 8192 }),
        })
        .map_err(|e| format!("source limits: {e:?}"))?;
        let owner = MenuLeaseOwner::new(
            WindowBinding {
                browser,
                window: browser.local(),
            },
            nz(1),
        )
        .map_err(|e| format!("source metadata owner: {e:?}"))?;
        let menu = MenuBridgeFixture::new(owner.lease());
        let registration = rt
            .bind_context_menu(ContextMenuTarget {
                lease: owner.lease(),
                delivery: menu.clone(),
            })
            .map_err(|e| format!("source binding: {e:?}"))?;
        registration
            .set_source_viewer(self.viewer.clone())
            .map_err(|e| format!("source provider: {e:?}"))?;
        self.owner = Some(owner);
        self.binding = Some(registration);
        self.menu = Some(menu.clone());
        if self.hold_visitor {
            rt.probe_hold_source_visitor(browser)
                .map_err(|e| format!("arm source visitor retention: {e:?}"))?;
        }
        for opening in 1..=if self.report.budget_case || self.hold_visitor {
            1
        } else {
            2
        } {
            self.report.phase = opening;
            let deadline = Instant::now() + Duration::from_secs(20);
            rt.probe_menu_source_open(browser)
                .map_err(|e| format!("source input: {e:?}"))?;
            let delivery = loop {
                if let Some(e) = self.binding.as_ref().unwrap().status().first_failure {
                    return Err(format!("source menu capture: {e}"));
                }
                if let Some(d) = menu.take() {
                    break d;
                }
                if Instant::now() >= deadline {
                    return Err("source menu delivery timeout".into());
                }
                tick(rt, mode)?;
            };
            self.report.menu_offered.push(
                delivery
                    .presentation
                    .model
                    .entries
                    .iter()
                    .filter_map(|e| match e {
                        rust_cef_core::menu_model::MenuItem::Command(c) => Some(c.command_id),
                        _ => None,
                    })
                    .collect(),
            );
            while rt.diagnostics().pending_requests != 1 {
                if Instant::now() >= deadline {
                    return Err("source menu callback return timeout".into());
                }
                tick(rt, mode)?;
            }
            delivery
                .response
                .submit(
                    delivery.presentation.request,
                    ContextMenuResponse::Select { command_id: 132 },
                )
                .map_err(|e| format!("source selection admission: {e:?}"))?;
            if self.report.budget_case {
                while rt.source_view_diagnostics().finished != 1
                    || rt.source_view_diagnostics().pending != 0
                {
                    if Instant::now() >= deadline {
                        return Err("source budget retirement timeout".into());
                    }
                    tick(rt, mode)?;
                }
                let s = self.viewer.0.lock().unwrap();
                let receipt = s
                    .controls
                    .first()
                    .ok_or("missing independent source failure receipt")?;
                let q = receipt.status();
                self.report.budget_rejected = q.first_failure == Some(SourceFailure::Budget)
                    && q.outcome == Some(SourceOutcome::Failed)
                    && !q.native_live
                    && !q.host_live
                    && s.publications == 0;
                if !self.report.budget_rejected {
                    return Err(format!("source budget negative wrong outcome: {:?}", q));
                }
                continue;
            }
            let (request, control) = loop {
                if let Some(d) = self.viewer.0.lock().unwrap().pending.take() {
                    break d;
                }
                if let Some((_, e)) = rt.source_view_diagnostics().first_failure {
                    return Err(format!("source extraction: {e:?}"));
                }
                if Instant::now() >= deadline {
                    return Err("source visitor/document timeout".into());
                }
                tick(rt, mode)?;
            };
            if request.context.binding.browser != browser
                || request.menu != delivery.presentation.request
                || !request
                    .source
                    .as_str()
                    .contains("<span>menu bridge selection</span>")
                || !control.is_current()
            {
                return Err("source document identity/content/scope mismatch".into());
            }
            self.report.source_bytes.push(request.source.len());
            self.report
                .source_hashes
                .push(hash(request.source.as_str().as_bytes()));
            self.report
                .frame_id_bytes
                .push(request.context.frame_id.len());
            self.report.operation_ids.push([
                request.operation.browser.runtime().value(),
                request.operation.browser.local(),
                request.operation.sequence,
            ]);
            control
                .presented()
                .map_err(|e| format!("metadata-only viewer admission: {e:?}"))?;
            if self.hold_visitor {
                let q = control.status();
                if rt.probe_held_source_visitor() != Some(request.operation)
                    || !q.native_live
                    || !q.host_live
                    || !q.callback_received
                    || self.binding.as_ref().unwrap().status().cancelled != 1
                {
                    return Err(
                        "missing real source result and retained visitor before close".into(),
                    );
                }
                self.viewer.0.lock().unwrap().open = Some(request);
                self.report.barrier_after_menu_terminal = true;
                self.report.phase = 3;
                return Ok(());
            }
            while rt.source_view_diagnostics().native_live != 0 {
                if Instant::now() >= deadline {
                    return Err("source final native field release fence timeout".into());
                }
                tick(rt, mode)?;
            }
            let q = control.status();
            if !q.host_live
                || q.native_live
                || rt.diagnostics().pending_requests != 1
                || self.binding.as_ref().unwrap().status().cancelled != opening as u64
            {
                return Err(
                    "source operation incorrectly equated with menu/native completion".into(),
                );
            }
            self.viewer.0.lock().unwrap().open = Some(request);
            if opening == 1 {
                self.report.barrier_after_menu_terminal = true;
                let old = self.viewer.0.lock().unwrap().open.take();
                drop(old);
                control
                    .closed()
                    .map_err(|e| format!("metadata viewer absence: {e:?}"))?;
                if rt.source_view_diagnostics().pending != 0 {
                    return Err("first source operation did not finish".into());
                }
            } else {
                self.report.viewer_pending_at_shutdown = true;
            }
        }
        self.report.phase = 3;
        Ok(())
    }
    /// A second actual CEF opening on the original A registration. Consume
    /// only its host delivery, never submit a response, cancel, or rebind.
    pub(super) fn hold_unanswered_menu(
        &self,
        rt: &mut Runtime,
        mode: ThreadMode,
        handle: &BrowserHandle,
        tick: Tick,
    ) -> Result<BridgeDelivery, String> {
        if !self.hold_visitor || self.report.phase != 3 {
            return Err("unanswered menu requires the original held source fixture".into());
        }
        let menu = self.menu.as_ref().ok_or("original menu owner missing")?;
        let binding = self
            .binding
            .as_ref()
            .ok_or("original menu registration missing")?;
        let browser = handle.snapshot().browser;
        rt.probe_menu_bridge_open(browser, false)
            .map_err(|e| format!("open managed pressure menu: {e:?}"))?;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let status = binding.status();
            if let Some(e) = status.first_failure {
                return Err(format!("managed pressure menu: {e}"));
            }
            if status.published == 2
                && rt.diagnostics().tasks.pending == 0
                && let Some(delivery) = menu.take()
            {
                let id = delivery.presentation.request;
                let d = menu.snapshot();
                if id.browser == browser
                    && id.sequence == d.last_sequence
                    && d.published == 2
                    && d.terminal == 1
                    && d.outstanding == 1
                    && status.cancelled == 1
                    && status.actions_submitted == 1
                {
                    return Ok(delivery);
                }
                return Err("second managed menu is not uniquely unanswered".into());
            }
            if Instant::now() >= deadline {
                return Err("original unanswered menu was not delivered".into());
            }
            tick(rt, mode)?;
        }
    }

    pub(super) fn managed_close(
        &self,
        rt: &Runtime,
        first: &BrowserHandle,
        closed_events: &[ProbeBrowserEvent],
        delivery: BridgeDelivery,
    ) -> Result<ManagedPressureDrain, String> {
        let [closed] = closed_events else {
            return Err("old response requires exactly one observed original Closed".into());
        };
        let id = delivery.presentation.request;
        let command = rt
            .probe_command_pressure_status(id.browser)
            .ok_or("pressured Close task missing")?;
        let menu_delivery = self
            .menu
            .as_ref()
            .ok_or("original menu owner missing")?
            .snapshot();
        let menu_status = self
            .binding
            .as_ref()
            .ok_or("original menu registration missing")?
            .status();
        let original_menu = [
            id.browser.runtime().value(),
            id.browser.local(),
            id.sequence,
        ];
        if !command.complete()
            || !command.cancelled_exact_menu(original_menu)
            || !command
                .held_managed_at_close
                .as_ref()
                .is_some_and(|r| r.menu == original_menu)
            || menu_delivery.published != 2
            || menu_delivery.terminal != 2
            || menu_delivery.outstanding != 0
            || !matches!(menu_delivery.terminal_ids.as_slice(), [old, pending] if *old > 0 && *old < *pending && *pending == id.sequence)
            || menu_delivery.first_error.is_some()
            || menu_status.cancelled != 2
            || menu_status.actions_submitted != 1
            || menu_status.first_failure.is_some()
        {
            return Err(format!(
                "original managed menu did not retire exactly once at pressure close: {command:?}, {menu_status:?}"
            ));
        }
        let first_state = first.snapshot();
        let owner = (id.browser.runtime().value(), id.browser.local());
        // This fixture offered Select All, not a disabled or invented action.
        let command_id = 117;
        if first_state.browser != id.browser
            || first_state.state != SessionState::Closed
            || closed.0 != owner
            || closed.1 == 0
            || !menu_status.retired
            || !delivery.presentation.model.allows_selection(command_id)
        {
            return Err("old response endpoint not qualified after original Closed".into());
        }
        let before = rt.diagnostics();
        let select = delivery
            .response
            .submit(id, ContextMenuResponse::Select { command_id });
        let cancel = delivery.response.submit(id, ContextMenuResponse::Cancel);
        let after = rt.diagnostics();
        let current = self.binding.as_ref().unwrap().status();
        let ledger = self.menu.as_ref().unwrap().snapshot();
        if select != Err(EngineError::InvalidRequest)
            || cancel != Err(EngineError::InvalidRequest)
            || before.tasks.posted != after.tasks.posted
            || before.native_calls != after.native_calls
            || current.actions_submitted != menu_status.actions_submitted
            || current.cancelled != menu_status.cancelled
            || current.first_failure != menu_status.first_failure
            || ledger.terminal_ids != menu_delivery.terminal_ids
            || ledger.terminal != menu_delivery.terminal
            || ledger.first_error != menu_delivery.first_error
            || ledger.outstanding != 0
        {
            return Err(format!(
                "old managed menu response changed state: Select={select:?}, Cancel={cancel:?}"
            ));
        }
        let late_reply = LateManagedMenuReply {
            menu: original_menu,
            owner_closed: *closed,
            command_id,
            select_error: "InvalidRequest",
            cancel_error: "InvalidRequest",
            tasks_posted: [before.tasks.posted, after.tasks.posted],
            native_calls: [before.native_calls, after.native_calls],
        };
        Ok(ManagedPressureDrain {
            command,
            menu_delivery,
            menu_status,
            late_reply,
        })
    }

    /// Explicit test-host dispatch, not a Runtime consumer or automatic retry.
    pub fn flush_close(&self) -> Result<(), String> {
        let (closing, pending, open) = {
            let mut s = self.viewer.0.lock().unwrap();
            let c = std::mem::take(&mut s.closing);
            let p = if c.is_empty() { None } else { s.pending.take() };
            let open = if c.is_empty() { None } else { s.open.take() };
            (c, p, open)
        };
        drop(pending);
        drop(open);
        for c in closing {
            c.closed()
                .map_err(|e| format!("source metadata owner close acknowledgement: {e:?}"))?;
            self.viewer.0.lock().unwrap().close_acks += 1;
        }
        Ok(())
    }
    pub fn snapshot(&self, rt: &Runtime) -> SourceProbeReport {
        let mut r = self.report.clone();
        let d = rt.source_view_diagnostics();
        r.started = d.started;
        r.finished = d.finished;
        r.final_pending = d.pending;
        r.final_native_live = d.native_live;
        r.final_host_live = d.host_live;
        r.faulted = d.faulted;
        r.first_failure = d.first_failure.map(|(_, e)| format!("{e:?}"));
        let s = self.viewer.0.lock().unwrap();
        r.publications = s.publications;
        r.close_requests = s.close_requests;
        r.close_acks = s.close_acks;
        r.sink_error = s.error.clone();
        r.outcomes = s
            .controls
            .iter()
            .map(|c| format!("{:?}", c.status().outcome))
            .collect();
        r
    }
}
fn hash(b: &[u8]) -> u64 {
    b.iter().fold(14695981039346656037u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(1099511628211)
    })
}
