//! Validation owner for the original A menu. This deliberately does not attach
//! a presenter, activate a desktop window, or manufacture a native continuation.
use super::*;
use rust_cef_gpui::{
    ContextMenuConfig, ContextMenuRegistration,
    ports::{ContextMenuTarget, WindowBinding},
    test_support::{
        BridgeDelivery, CommandPressure, ContextMenuRequestId, ContextMenuResponse,
        MenuBridgeFixture, MenuContextLimits, MenuLeaseOwner, MenuLimits, WindowMenuCut,
        WindowPeerFlush,
    },
};

pub(super) struct Pending {
    owner: MenuLeaseOwner,
    binding: ContextMenuRegistration,
    sink: Arc<MenuBridgeFixture>,
    delivery: BridgeDelivery,
    window: u64,
    pub(super) source: Option<rust_cef_gpui::source::SourceOperationId>,
    pub(super) find: Option<rust_cef_gpui::find::FindOperationId>,
}

pub(super) struct Retired {
    pub(super) cut: WindowMenuCut,
    pub(super) json: String,
}

const SELECT_ALL: i32 = 117;

impl Pending {
    pub(super) fn configure(rt: &Runtime, with_find: bool) -> Result<(), String> {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        rt.configure_context_menus(ContextMenuConfig {
            capacity: nz(if with_find { 2 } else { 1 }),
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
        .map_err(|e| format!("window pressure menu config: {e:?}"))
    }

    pub(super) async fn open(
        s: &mut Scene<'_>,
        browser: BrowserId,
        window: u64,
        find: Option<rust_cef_gpui::find::FindOperationId>,
        deadline: Instant,
    ) -> Result<Self, String> {
        let target = WindowBinding { browser, window };
        let owner = MenuLeaseOwner::new(target, NonZeroUsize::MIN)
            .map_err(|e| format!("window menu lease: {e:?}"))?;
        let sink = MenuBridgeFixture::new(owner.lease());
        let binding =
            s.rt.bind_context_menu(ContextMenuTarget {
                lease: owner.lease(),
                delivery: sink.clone(),
            })
            .map_err(|e| format!("bind original window menu: {e:?}"))?;
        let source = if s.out.window_source_enabled {
            let held = window_source::Held::arm(s.rt, target)?;
            binding
                .set_source_viewer(held.sink())
                .map_err(|e| format!("attach original A Source: {e:?}"))?;
            s.out.window_source = Some(held);
            s.rt.probe_menu_source_open(browser)
                .map_err(|e| format!("CEF-local A Source stimulus: {e:?}"))?;
            let initial = Self::delivery(s, &binding, &sink, target, 1, 132, deadline).await?;
            s.out
                .window_source
                .as_ref()
                .unwrap()
                .arm_menu(initial.presentation.request)?;
            initial
                .response
                .submit(
                    initial.presentation.request,
                    ContextMenuResponse::Select { command_id: 132 },
                )
                .map_err(|e| format!("admit original A Source: {e:?}"))?;
            loop {
                if let Some(id) = s.out.window_source.as_mut().unwrap().observe_queued(s.rt)? {
                    break Some(id);
                }
                if Instant::now() >= deadline {
                    return Err("original window Source callback timed out".into());
                }
                s.service(false, None).await?;
            }
        } else {
            None
        };
        s.rt.probe_menu_bridge_open(browser, false)
            .map_err(|e| format!("CEF-local original menu stimulus: {e:?}"))?;
        let delivery = Self::delivery(
            s,
            &binding,
            &sink,
            target,
            1 + u64::from(source.is_some()),
            SELECT_ALL,
            deadline,
        )
        .await?;
        Ok(Self {
            owner,
            binding,
            sink,
            delivery,
            window,
            source,
            find,
        })
    }

    pub(super) async fn delivery(
        s: &mut Scene<'_>,
        binding: &ContextMenuRegistration,
        sink: &MenuBridgeFixture,
        target: WindowBinding,
        published: u64,
        command: i32,
        deadline: Instant,
    ) -> Result<BridgeDelivery, String> {
        loop {
            let status = binding.status();
            if let Some(e) = status.first_failure {
                return Err(format!("window menu callback: {e}"));
            }
            if status.published == published
                && s.rt.diagnostics().tasks.pending == 0
                && let Some(d) = sink.take()
            {
                let ledger = sink.snapshot();
                if d.presentation.request.browser == target.browser
                    && d.presentation.request.sequence > 0
                    && d.presentation.context.binding == target
                    && d.presentation.model.allows_selection(command)
                    && ledger.published == published
                    && ledger.terminal == published - 1
                    && ledger.outstanding == 1
                    && ledger.first_error.is_none()
                    && status.cancelled == published - 1
                    && status.actions_submitted == published - 1
                {
                    return Ok(d);
                }
                return Err("original menu did not remain unanswered".into());
            }
            if Instant::now() >= deadline {
                return Err("waiting for original window menu delivery".into());
            }
            s.service(false, None).await?;
        }
    }

    pub(super) fn request(&self) -> ContextMenuRequestId {
        self.delivery.presentation.request
    }

    pub(super) fn cut(&self) -> WindowMenuCut {
        let id = self.request();
        WindowMenuCut {
            menu: [
                id.browser.runtime().value(),
                id.browser.local(),
                id.sequence,
            ],
            gpui_window: self.window,
            source: self.source.map(window_source::key),
            find: self.find.map(window_find::key),
        }
    }

    pub(super) fn retire(
        mut self,
        rt: &Runtime,
        command: &CommandPressure,
        peer_flush: Option<WindowPeerFlush>,
    ) -> Result<Retired, String> {
        let cut = self.cut();
        let id = self.request();
        let status = self.binding.status();
        let m = self.sink.snapshot();
        let prior = u64::from(self.source.is_some());
        let ordered = m.terminal_ids.last() == Some(&id.sequence)
            && m.terminal_ids.len() as u64 == prior + 1
            && (prior == 0 || m.terminal_ids[0] > 0 && m.terminal_ids[0] < id.sequence);
        if !command.complete_with_window_menu(cut, peer_flush)
            || !status.retired
            || status.opened != 1 + prior
            || status.published != 1 + prior
            || status.cancelled != 1 + prior
            || status.native_dismissed != 1 + prior
            || status.actions_submitted != prior
            || status.first_failure.is_some()
            || status.post_failures != 0
            || m.published != 1 + prior
            || m.terminal != 1 + prior
            || !ordered
            || m.outstanding != 0
            || m.first_error.is_some()
        {
            return Err("original window menu did not settle once in its own Close task".into());
        }
        // Test old host delivery, not a late native callback. The original
        // endpoint and original active Select All are retained unchanged.
        let before = rt.diagnostics();
        let select = self.delivery.response.submit(
            id,
            ContextMenuResponse::Select {
                command_id: SELECT_ALL,
            },
        );
        let cancel = self
            .delivery
            .response
            .submit(id, ContextMenuResponse::Cancel);
        let after = rt.diagnostics();
        let terminal = self.sink.snapshot();
        let final_status = self.binding.status();
        if select != Err(EngineError::InvalidRequest)
            || cancel != Err(EngineError::InvalidRequest)
            || before.tasks.posted != after.tasks.posted
            || before.native_calls != after.native_calls
            || terminal.terminal_ids != m.terminal_ids
            || terminal.terminal != m.terminal
            || terminal.first_error.is_some()
            || final_status.first_failure.is_some()
            || final_status.cancelled != 1 + prior
            || final_status.actions_submitted != prior
        {
            return Err(format!(
                "old window menu reply after Closed: {select:?}/{cancel:?}"
            ));
        }
        self.owner
            .retire()
            .map_err(|e| format!("retire original window menu host lease: {e:?}"))?;
        if !self
            .owner
            .lease()
            .snapshot()
            .is_ok_and(|state| state.is_drained())
        {
            return Err("original menu lease did not drain".into());
        }
        let task = command.close_task.ok_or("window menu Close task missing")?;
        Ok(Retired {
            cut,
            json: object(vec![
                ("menu", format!("{:?}", cut.menu)),
                ("gpui_window", cut.gpui_window.to_string()),
                ("close_task", format!("[{},{}]", task.0, task.1)),
                ("published", m.published.to_string()),
                ("terminal_ids", format!("{:?}", m.terminal_ids)),
                ("cancellations", status.cancelled.to_string()),
                ("native_dismissals", status.native_dismissed.to_string()),
                ("actions_submitted", status.actions_submitted.to_string()),
                ("outstanding", m.outstanding.to_string()),
                ("lease_drained", "true".into()),
                ("late_select_command", SELECT_ALL.to_string()),
                ("late_select_error", quote("InvalidRequest")),
                ("late_cancel_error", quote("InvalidRequest")),
                (
                    "tasks_posted",
                    format!("[{},{}]", before.tasks.posted, after.tasks.posted),
                ),
                (
                    "native_calls",
                    format!("[{},{}]", before.native_calls, after.native_calls),
                ),
            ]),
        })
    }
}
