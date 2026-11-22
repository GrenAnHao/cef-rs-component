//! Validation only: actual GPUI confirmation panel, metadata no-job operation.
use super::*;
use gpui::WeakEntity;
use rust_cef_gpui::{
    contracts::SessionState,
    ports::{BrowserCommand, BrowserHandle, FrameSource, WindowBinding},
    print::{PrintId, PrintOutcome, PrintPhase, PrintStatus},
    print_confirmation::{PrintConfirmation, PrintPanelError},
};

pub(super) struct LoadWitness {
    browser: Option<BrowserId>,
    conflicted: bool,
    request: u64,
    start: u64,
    finish: u64,
}

impl LoadWitness {
    pub(super) fn new() -> Self {
        Self {
            browser: None,
            conflicted: false,
            request: 0,
            start: 0,
            finish: 0,
        }
    }

    pub(super) fn bind(&mut self, browser: BrowserId) -> Result<(), &'static str> {
        if self.conflicted || *self.browser.get_or_insert(browser) != browser {
            return Err("print load observer saw a foreign Browser");
        }
        Ok(())
    }

    fn facts(&self, browser: BrowserId) -> Result<(u64, u64, u64), &'static str> {
        if self.conflicted || self.browser != Some(browser) {
            return Err("print load observer is not bound to the original Browser");
        }
        Ok((self.request, self.start, self.finish))
    }

    pub(super) fn observe(&mut self, e: &BrowserEventEnvelope) {
        let value = match e.event {
            BrowserEvent::NavigationRequested {
                is_main_frame: true,
                is_redirect: false,
            } => &mut self.request,
            BrowserEvent::LoadStarted {
                is_main_frame: true,
            } => &mut self.start,
            BrowserEvent::LoadFinished {
                is_main_frame: true,
                ..
            } => &mut self.finish,
            _ => return,
        };
        if *self.browser.get_or_insert(e.browser) != e.browser {
            self.conflicted = true;
            return;
        }
        *value = (*value).max(e.sequence);
    }
}

pub(super) async fn await_initial_load(
    cx: &mut AsyncApp,
    rt: &mut Runtime,
    receiver: &UiWakeReceiver,
    stats: &Stats,
    handle: &BrowserHandle,
) -> Result<u64, String> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let s = handle.snapshot();
        let (_, _, finish) = stats
            .print_load
            .lock()
            .unwrap()
            .as_ref()
            .ok_or("print load observer is missing")?
            .facts(s.browser)?;
        if finish > 0 && s.is_ready() && s.navigation.address == URL && !s.navigation.is_loading {
            return Ok(finish);
        }
        if Instant::now() >= deadline {
            return Err("print fixture initial main load timeout".into());
        }
        turn(cx, rt, receiver, false).await?;
    }
}

pub(super) struct PanelCase {
    id: PrintId,
    window: u64,
    original_view: WeakEntity<CefBrowser>,
    original_source: Arc<dyn FrameSource>,
    panel: Option<WeakEntity<PrintConfirmation>>,
    paints: u64,
    wrong_owner_refused: bool,
    owner_closed: bool,
    reaped: bool,
    reload_proof: Option<String>,
}

impl PanelCase {
    pub(super) fn arm(
        cx: &mut AsyncApp,
        root: WindowHandle<HostRoot>,
        rt: &Runtime,
        handle: &BrowserHandle,
        view: &Entity<CefBrowser>,
    ) -> Result<Self, String> {
        let browser = handle.snapshot().browser;
        let window = update(cx, root, |_, window, _| {
            window.window_handle().window_id().as_u64()
        })?;
        let original_source = rt
            .frame_source(browser)
            .map_err(|e| format!("original print View source: {e:?}"))?;
        let id = rt
            .probe_arm_print_panel(WindowBinding { browser, window })
            .map_err(|e| format!("arm metadata print confirmation: {e:?}"))?;
        Ok(Self::bind_armed(id, window, view, original_source))
    }

    pub(super) fn bind_armed(
        id: PrintId,
        window: u64,
        view: &Entity<CefBrowser>,
        original_source: Arc<dyn FrameSource>,
    ) -> Self {
        Self {
            id,
            window,
            original_view: view.downgrade(),
            original_source,
            panel: None,
            paints: 0,
            wrong_owner_refused: false,
            owner_closed: false,
            reaped: false,
            reload_proof: None,
        }
    }

    pub(super) fn owner_is_closed(&self, handle: &BrowserHandle) -> bool {
        let snapshot = handle.snapshot();
        snapshot.browser == self.id.browser
            && snapshot.state == SessionState::Closed
            && self.original_source.snapshot().closed
            && self.original_view.upgrade().is_none()
    }

    fn identity(&self) -> String {
        format!(
            "[{},{},{}]",
            self.id.browser.runtime().value(),
            self.id.browser.local(),
            self.id.sequence
        )
    }

    pub(super) async fn present(
        &mut self,
        cx: &mut AsyncApp,
        root: WindowHandle<HostRoot>,
        rt: &Runtime,
        view: &Entity<CefBrowser>,
        options: &Options,
    ) -> Result<(), String> {
        let status = rt
            .probe_print_status(self.id)
            .ok_or("original confirmation missing")?;
        if rt.diagnostics().pending_requests != 1
            || status.phase != PrintPhase::AwaitingConfirmation
        {
            return Err("original confirmation did not reach Runtime accounting".into());
        }
        self.present_original(cx, root, status, view).await?;
        write_json(
            &options
                .report
                .with_file_name("print-confirmation-present.json"),
            object(vec![
                ("pid", std::process::id().to_string()),
                ("operation", self.identity()),
                ("window_id", self.window.to_string()),
                ("owner_paints", self.paints.to_string()),
                ("pending", rt.diagnostics().pending_requests.to_string()),
                ("wrong_binding_refused", bool_json(self.wrong_owner_refused)),
                ("submission_enabled", bool_json(false)),
            ]),
        )
    }

    pub(super) async fn present_original(
        &mut self,
        cx: &mut AsyncApp,
        root: WindowHandle<HostRoot>,
        status: PrintStatus,
        view: &Entity<CefBrowser>,
    ) -> Result<(), String> {
        if self.panel.is_some()
            || status.operation != self.id
            || self.original_view.upgrade().as_ref() != Some(view)
        {
            return Err("original print panel installation repeated or wrong owner".into());
        }
        let mut installed = None;
        let result = update(cx, root, |host, window, app| {
            if host.print_confirmation.is_some() {
                return Err("duplicate print panel");
            }
            let mut foreign = status;
            foreign.origin.binding.window ^= 1;
            let wrong = matches!(
                PrintConfirmation::new(view, window, app, foreign),
                Err(PrintPanelError::WrongOwner)
            );
            let panel = PrintConfirmation::new(view, window, app, status)
                .map_err(|_| "cannot bind panel to original view")?;
            let panel = app.new(|_| panel);
            installed = Some(panel.downgrade());
            host.print_confirmation = Some(panel);
            app.notify();
            Ok::<_, &str>(wrong)
        });
        // Preserve the exact installed identity even if the outer update fails
        // after running its callback; never infer absence from that error.
        self.panel = installed;
        let rejected = result??;
        self.wrong_owner_refused = rejected;
        rendered(cx, root).await?;
        self.paints = self.original_panel_paints(cx, root, true)?;
        if !rejected || self.paints == 0 {
            return Err(
                "original panel did not paint or accepted mismatched owner metadata".into(),
            );
        }
        Ok(())
    }

    pub(super) fn original_panel_paints(
        &self,
        cx: &mut AsyncApp,
        root: WindowHandle<HostRoot>,
        live: bool,
    ) -> Result<u64, String> {
        update(cx, root, |host, window, app| {
            let panel = self
                .panel
                .as_ref()
                .and_then(|weak| weak.upgrade())
                .ok_or("original panel is absent")?;
            if window.window_handle().window_id().as_u64() != self.window
                || host.print_confirmation.as_ref() != Some(&panel)
            {
                return Err("window no longer owns the exact original panel".into());
            }
            let original = panel.read(app);
            if original.operation() != self.id
                || original.owner_current(window) != live
                || (live && !original.open_in_owner(window))
            {
                return Err(
                    "original panel ownership/operation did not match the expected phase".into(),
                );
            }
            Ok(original.owner_paints())
        })?
    }

    pub(super) fn request_original_panel_paint(
        &self,
        cx: &mut AsyncApp,
        root: WindowHandle<HostRoot>,
    ) -> Result<(), String> {
        update(cx, root, |host, window, app| {
            let panel = self
                .panel
                .as_ref()
                .and_then(|weak| weak.upgrade())
                .ok_or("original panel is absent")?;
            if window.window_handle().window_id().as_u64() != self.window
                || host.print_confirmation.as_ref() != Some(&panel)
                || !panel.read(app).open_in_owner(window)
            {
                return Err(
                    "cannot request a paint for an inactive or replaced original panel".into(),
                );
            }
            panel.update(app, |_, cx| cx.notify());
            Ok(())
        })?
    }

    pub(super) async fn reload(
        &mut self,
        cx: &mut AsyncApp,
        root: WindowHandle<HostRoot>,
        rt: &mut Runtime,
        receiver: &UiWakeReceiver,
        stats: &Stats,
        handle: &BrowserHandle,
        baseline: u64,
        fail_after_detach: bool,
    ) -> Result<(), String> {
        let original = rt
            .probe_print_status(self.id)
            .ok_or("print operation lost before reload")?;
        if original.cancel_requested {
            return Err("print already revoked before reload".into());
        }
        let command = handle
            .submit(BrowserCommand::Reload)
            .map_err(|e| format!("print original reload: {e:?}"))?;
        if command.browser != self.id.browser || command.sequence == 0 {
            return Err("wrong reload command owner".into());
        }
        let deadline = Instant::now() + Duration::from_secs(8);
        let (request, start, finish) = loop {
            let (request, start, finish) = stats
                .print_load
                .lock()
                .unwrap()
                .as_ref()
                .ok_or("print reload observer is missing")?
                .facts(self.id.browser)?;
            let s = handle.snapshot();
            if !s.is_ready()
                || s.browser != self.id.browser
                || self.original_source.snapshot().closed
            {
                return Err("print View closed instead of reloading".into());
            }
            if baseline < request
                && request < start
                && start < finish
                && !s.navigation.is_loading
                && s.navigation.address == URL
            {
                break (request, start, finish);
            }
            if Instant::now() >= deadline {
                return Err("original CEF reload callback sequence timeout".into());
            }
            turn(cx, rt, receiver, false).await?;
        };
        let revoked = rt
            .probe_print_status(self.id)
            .ok_or("reload lost pending confirmation")?;
        if revoked.phase != PrintPhase::AwaitingConfirmation
            || !revoked.cancel_requested
            || revoked.origin != original.origin
            || rt.diagnostics().pending_requests != 1
        {
            return Err("reload did not revoke exact original document confirmation".into());
        }
        let before = update(cx, root, |host, window, app| {
            let panel = self
                .panel
                .as_ref()
                .and_then(|p| p.upgrade())
                .ok_or("original panel lost")?;
            if host.print_confirmation.as_ref() != Some(&panel) {
                return Err("different panel after reload");
            }
            panel.update(app, |panel, app| {
                if !panel.owner_current(window) {
                    return Err("panel owner not live at navigation cut");
                }
                let mut wrong = revoked;
                wrong.origin.document_epoch = std::num::NonZeroU64::new(2).unwrap();
                let mut wrong_menu = revoked;
                wrong_menu.menu.sequence = wrong_menu.menu.sequence.saturating_add(1);
                if panel.observe_revocation(wrong, window)
                    || panel.observe_revocation(wrong_menu, window)
                    || !panel.observe_revocation(revoked, window)
                    || panel.observe_revocation(revoked, window)
                    || panel.observe_revocation(original, window)
                    || panel.take_cancel(window).is_some()
                {
                    return Err("revocation accepted a wrong menu/epoch, replay, or response");
                }
                app.notify();
                Ok(panel.owner_paints())
            })
        })??;
        rendered(cx, root).await?;
        let after = update(cx, root, |host, _, app| {
            host.print_confirmation
                .as_ref()
                .map(|p| p.read(app).owner_paints())
        })?;
        if before == 0 || after != Some(before) {
            return Err("revoked panel painted again".into());
        }
        self.record_no_job(rt)?;
        if !self.detach_original_panel(cx, root)? {
            return Err("original print panel missing before reload removal".into());
        }
        if fail_after_detach {
            return Err("test-induced reload failure after original no-job panel detach".into());
        }
        self.prove_panel_released(cx, root).await?;
        self.reap(rt)?;
        if self.original_view.upgrade().is_none()
            || !handle.snapshot().is_ready()
            || self.original_source.snapshot().closed
        {
            return Err("original View not live at print retirement cut".into());
        }
        self.reload_proof = Some(object(vec![
            ("command", command.sequence.to_string()),
            (
                "main_load_sequence",
                format!("[{baseline},{request},{start},{finish}]"),
            ),
            ("original_view_live_at_reap", bool_json(true)),
            ("wrong_epoch_and_replay_refused", bool_json(true)),
            ("wrong_menu_refused", bool_json(true)),
            ("paint_at_revocation", before.to_string()),
            ("paint_after_revocation", before.to_string()),
        ]));
        Ok(())
    }

    async fn await_owner_close(
        &mut self,
        cx: &mut AsyncApp,
        rt: &mut Runtime,
        receiver: &UiWakeReceiver,
        handle: &BrowserHandle,
    ) -> Result<(), String> {
        if handle.snapshot().browser != self.id.browser {
            return Err("print retirement handle no longer matches the original Browser".into());
        }
        if self.owner_closed {
            return Ok(());
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while handle.snapshot().state != SessionState::Closed
            || !self.original_source.snapshot().closed
        {
            if Instant::now() >= deadline {
                return Err("original Print View/Browser did not retire".into());
            }
            turn(cx, rt, receiver, false).await?;
        }
        if self.original_view.upgrade().is_some() {
            return Err("original print View was not released".into());
        }
        self.owner_closed = true;
        Ok(())
    }

    pub(super) fn owner_close_observed(&self) -> bool {
        self.owner_closed
    }

    pub(super) async fn retire(
        &mut self,
        cx: &mut AsyncApp,
        root: WindowHandle<HostRoot>,
        rt: &mut Runtime,
        receiver: &UiWakeReceiver,
        handle: &BrowserHandle,
        fail_after_detach: bool,
    ) -> Result<String, String> {
        self.await_owner_close(cx, rt, receiver, handle).await?;
        if self.reload_proof.is_some() {
            if !self.reaped
                || rt.probe_print_status(self.id).is_some()
                || rt.diagnostics().pending_requests != 0
            {
                return Err("closed View recreated a reaped confirmation".into());
            }
            return Ok(self.report());
        }
        let s = rt
            .probe_print_status(self.id)
            .ok_or("original print operation retired prematurely")?;
        if s.operation != self.id
            || s.phase != PrintPhase::AwaitingConfirmation
            || !s.cancel_requested
            || rt.diagnostics().pending_requests != 1
        {
            return Err("original View retirement lost the pending print operation".into());
        }
        let stale = update(cx, root, |host, window, app| {
            let Some(original) = self.panel.as_ref().and_then(|panel| panel.upgrade()) else {
                return false;
            };
            window.window_handle().window_id().as_u64() == self.window
                && host.print_confirmation.as_ref() == Some(&original)
                && !original.read(app).owner_current(window)
                && original.read(app).operation() == self.id
        })?;
        if !stale {
            return Err("confirmation panel was not invalidated by original View release".into());
        }
        self.record_no_job(rt)?;
        if !self.detach_original_panel(cx, root)? {
            return Err("original print panel missing before close removal".into());
        }
        if fail_after_detach {
            return Err("test-induced close failure after original no-job panel detach".into());
        }
        self.prove_panel_released(cx, root).await?;
        self.reap(rt)?;
        Ok(self.report())
    }

    /// This path does not publish a passing panel report or retry the operation.
    /// Advance only the original metadata row after the primary failure.
    pub(super) async fn cleanup_after_failure(
        mut self,
        cx: &mut AsyncApp,
        root: WindowHandle<HostRoot>,
        rt: &mut Runtime,
        receiver: &UiWakeReceiver,
        handle: &BrowserHandle,
    ) -> Result<String, String> {
        self.await_owner_close(cx, rt, receiver, handle).await?;
        let mut already_cancelled = self.reaped;
        let mut detached_in_cleanup = false;
        let panel_never_installed = self.panel.is_none();
        if !self.reaped {
            let status = rt
                .probe_print_status(self.id)
                .ok_or("missing original failed print row")?;
            if status.operation != self.id
                || !status.cancel_requested
                || status.accepted_job.is_some()
                || status.resources_retired
            {
                return Err("cannot qualify the original no-job row during failure cleanup".into());
            }
            match (status.phase, status.outcome) {
                (PrintPhase::AwaitingConfirmation, None) => self.record_no_job(rt)?,
                (PrintPhase::Terminal, Some(PrintOutcome::Cancelled)) => already_cancelled = true,
                _ => return Err("failed print cleanup must not replace an unknown result".into()),
            }
            detached_in_cleanup = self.release_panel_resources(cx, root).await?;
            self.reap(rt)?;
        }
        if !self.reaped
            || rt.probe_print_status(self.id).is_some()
            || rt.diagnostics().pending_requests != 0
        {
            return Err("failure cleanup did not retire the original print row".into());
        }
        Ok(object(vec![
            ("operation", self.identity()),
            ("already_cancelled", bool_json(already_cancelled)),
            ("detached_in_cleanup", bool_json(detached_in_cleanup)),
            ("panel_never_installed", bool_json(panel_never_installed)),
            ("panel_entity_released", bool_json(!panel_never_installed)),
            ("original_browser_closed", bool_json(true)),
            ("print_operation_reaped", bool_json(true)),
            ("pending_after_cleanup", "0".into()),
        ]))
    }

    fn record_no_job(&self, rt: &Runtime) -> Result<(), String> {
        rt.probe_print_cancelled(self.id)
            .map_err(|e| format!("metadata no-job result: {e:?}"))?;
        if rt.diagnostics().pending_requests != 1 {
            return Err("cancel result prematurely released print gate".into());
        }
        Ok(())
    }

    pub(super) async fn release_panel_resources(
        &self,
        cx: &mut AsyncApp,
        root: WindowHandle<HostRoot>,
    ) -> Result<bool, String> {
        if self.panel.is_none() {
            let vacant = update(cx, root, |host, window, _| {
                window.window_handle().window_id().as_u64() == self.window
                    && host.print_confirmation.is_none()
            })?;
            if !vacant {
                return Err("cannot attest absence of an uninstalled original panel".into());
            }
            return Ok(false);
        }
        let detached = self.detach_original_panel(cx, root)?;
        self.prove_panel_released(cx, root).await?;
        Ok(detached)
    }

    fn detach_original_panel(
        &self,
        cx: &mut AsyncApp,
        root: WindowHandle<HostRoot>,
    ) -> Result<bool, String> {
        let expected = self
            .panel
            .as_ref()
            .ok_or("missing original panel identity")?;
        let removed = update(cx, root, |host, window, app| {
            if window.window_handle().window_id().as_u64() != self.window {
                return Err("wrong original host for panel removal");
            }
            if let Some(present) = host.print_confirmation.as_ref() {
                if expected.upgrade().as_ref() != Some(present) {
                    return Err("refuse to remove a different panel");
                }
                let panel = host.print_confirmation.take();
                app.notify();
                Ok(panel)
            } else {
                Ok(None)
            }
        })??;
        let detached = removed.is_some();
        drop(removed);
        Ok(detached)
    }

    async fn prove_panel_released(
        &self,
        cx: &mut AsyncApp,
        root: WindowHandle<HostRoot>,
    ) -> Result<(), String> {
        rendered(cx, root).await?;
        if !self.panel.as_ref().is_some_and(|p| p.upgrade().is_none()) {
            return Err("print panel entity still retained after removal".into());
        }
        Ok(())
    }

    fn reap(&mut self, rt: &Runtime) -> Result<(), String> {
        let receipt = rt
            .probe_print_retired(self.id)
            .map_err(|e| format!("metadata panel retirement acknowledgement: {e:?}"))?;
        if receipt.operation != self.id
            || receipt.outcome != Some(PrintOutcome::Cancelled)
            || !receipt.resources_retired
            || receipt.accepted_job.is_some()
            || rt.probe_print_status(self.id).is_some()
            || rt.diagnostics().pending_requests != 0
        {
            return Err("original print panel receipt did not drain".into());
        }
        self.reaped = true;
        Ok(())
    }

    fn report(&self) -> String {
        let mut fields = vec![
            ("operation", self.identity()),
            ("window_id", self.window.to_string()),
            ("owner_paints", self.paints.to_string()),
            ("wrong_binding_refused", bool_json(self.wrong_owner_refused)),
            ("original_browser_closed", bool_json(true)),
            ("original_view_released", bool_json(true)),
            ("stale_panel_rejected", bool_json(true)),
            ("same_panel_at_invalidation", bool_json(true)),
            ("panel_entity_released", bool_json(true)),
            ("print_operation_reaped", bool_json(true)),
            ("pending_by_cut", "[1,1,0]".into()),
            ("outcome", quote("Cancelled")),
            ("submission_enabled", bool_json(false)),
            ("physical_input", bool_json(false)),
            ("user_confirmation_observed", bool_json(false)),
        ];
        if let Some(proof) = &self.reload_proof {
            fields.push(("reload", proof.clone()));
        }
        object(fields)
    }
}
