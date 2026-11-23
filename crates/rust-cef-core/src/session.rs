use std::collections::BTreeSet;

use crate::contracts::{
    BrowserConfig, BrowserId, BrowserSnapshot, BrowserSurfaceMode, CreateError, zoom_level_to_milli,
};
use crate::js_bridge::{MAX_JS_JSON_DEPTH, validate_bounded_json, validate_script};
use crate::ports::{
    BrowserCommand, BrowserEngine, BrowserEvent, CommandEnvelope, CommandId, EngineError,
};

pub use crate::contracts::SessionState;

/// Maximum number of admitted, not-yet-consumed commands per session.
pub const COMMAND_CAPACITY: usize = 256;

/// Pure instance state. It never owns a CEF Browser or a GPUI entity.
pub struct Session {
    config: BrowserConfig,
    snapshot: BrowserSnapshot,
    next_command: u64,
    pending_commands: BTreeSet<u64>,
    pending_replies: BTreeSet<u64>,
    navigation_target: Option<String>,
    input_epoch: u64,
    pending_reset: Option<u64>,
    input_retired: bool,
}

impl Session {
    pub fn begin_create(browser: BrowserId, config: BrowserConfig) -> Result<Self, CreateError> {
        if config
            .profile
            .is_some_and(|profile| profile.runtime() != browser.runtime())
        {
            return Err(CreateError::WrongRuntime);
        }
        config.options.validate()?;
        Ok(Self {
            config,
            snapshot: BrowserSnapshot {
                browser,
                creation_accepted: false,
                state: SessionState::Creating,
                navigation: Default::default(),
                renderer: Default::default(),
            },
            next_command: 1,
            pending_commands: BTreeSet::new(),
            pending_replies: BTreeSet::new(),
            navigation_target: None,
            input_epoch: 1,
            pending_reset: None,
            input_retired: false,
        })
    }

    /// Create a session enforcing runtime-wide `windowless_rendering_enabled`.
    pub fn begin_create_for_runtime(
        browser: BrowserId,
        config: BrowserConfig,
        windowless_rendering_enabled: bool,
    ) -> Result<Self, CreateError> {
        config
            .options
            .surface_mode
            .validate_for_runtime(windowless_rendering_enabled)?;
        Self::begin_create(browser, config)
    }

    pub fn surface_mode(&self) -> BrowserSurfaceMode {
        self.config.options.surface_mode
    }

    /// Acceptance alone is not the native OnAfterCreated notification.
    pub fn accept_create_request(&mut self) {
        self.snapshot.creation_accepted = true;
    }

    pub fn after_created(&mut self) {
        if matches!(
            self.snapshot.state,
            SessionState::Creating | SessionState::Closing
        ) {
            self.snapshot.creation_accepted = true;
            if self.snapshot.state == SessionState::Creating {
                self.snapshot.state = SessionState::Alive;
            }
        }
    }

    pub fn close_requested(&mut self) {
        if matches!(
            self.snapshot.state,
            SessionState::Creating | SessionState::Alive
        ) {
            self.snapshot.state = SessionState::Closing;
        }
    }

    /// Only the native beforeunload denial path restores a created instance.
    /// Runtime drainage is a separate admission gate and is never undone here.
    pub fn close_cancelled(&mut self) {
        if self.snapshot.state == SessionState::Closing && self.snapshot.creation_accepted {
            self.snapshot.state = SessionState::Alive;
        }
    }

    pub fn closed(&mut self) {
        self.snapshot.state = SessionState::Closed;
    }

    pub fn create_rejected(&mut self) {
        if self.snapshot.state == SessionState::Creating {
            self.snapshot.state = SessionState::CreateFailed;
        }
    }

    /// Admit one normal command while the session is alive.
    ///
    /// The returned envelope carries a monotonically increasing, non-zero
    /// sequence. Capacity is released by `consume_command` or `cancel_command`.
    pub fn admit_command(
        &mut self,
        command: BrowserCommand,
    ) -> Result<CommandEnvelope, EngineError> {
        match self.snapshot.state {
            SessionState::Alive => {}
            SessionState::Creating | SessionState::CreateFailed => {
                return Err(EngineError::NotReady);
            }
            SessionState::Closing => return Err(EngineError::Closing),
            SessionState::Closed => return Err(EngineError::Closed),
        }
        if let BrowserCommand::SetZoomLevel(level) = &command {
            zoom_level_to_milli(*level)?;
        }
        match &command {
            BrowserCommand::ExecuteJavaScript { script, .. } => {
                validate_script(script).map_err(|_| EngineError::InvalidRequest)?;
            }
            BrowserCommand::EvaluateScript { id, script, .. } => {
                if id.browser != self.snapshot.browser {
                    return Err(EngineError::WrongRuntime);
                }
                validate_script(script).map_err(|_| EngineError::InvalidRequest)?;
            }
            BrowserCommand::PostJsMessage { payload_json } => {
                if !self.config.options.js_bridge.enabled {
                    return Err(EngineError::PolicyDenied);
                }
                validate_bounded_json(
                    payload_json,
                    self.config.options.js_bridge.max_message_bytes,
                    MAX_JS_JSON_DEPTH,
                )
                .map_err(|_| EngineError::InvalidRequest)?;
            }
            BrowserCommand::FinishOutboundDrag(finish) => {
                if finish.drag_id.browser != self.snapshot.browser {
                    return Err(EngineError::WrongRuntime);
                }
            }
            BrowserCommand::TouchEvent(touch) => {
                if self.input_retired {
                    return Err(EngineError::Backend { code: None });
                }
                touch.validate_and_normalize()?;
            }
            BrowserCommand::DragTarget(cmd) => {
                if self.input_retired {
                    return Err(EngineError::Backend { code: None });
                }
                cmd.validate()?;
            }
            BrowserCommand::SetNativeBounds(rect) => {
                if !self.config.options.surface_mode.is_native_child_hwnd() || !rect.is_valid() {
                    return Err(EngineError::InvalidRequest);
                }
            }
            BrowserCommand::NotifyParentMoved => {
                if !self.config.options.surface_mode.is_native_child_hwnd() {
                    return Err(EngineError::InvalidRequest);
                }
            }
            _ => {}
        }
        if self.config.options.surface_mode.is_native_child_hwnd()
            && matches!(
                &command,
                BrowserCommand::SetViewport(_)
                    | BrowserCommand::SetScreenOrigin { .. }
                    | BrowserCommand::MouseClick(_)
                    | BrowserCommand::Input(_)
                    | BrowserCommand::TouchEvent(_)
                    | BrowserCommand::DragTarget(_)
                    | BrowserCommand::FinishOutboundDrag(_)
            )
        {
            return Err(EngineError::InvalidRequest);
        }
        if matches!(&command, BrowserCommand::Input(_)) && self.input_retired {
            return Err(EngineError::Backend { code: None });
        }
        if self.pending_commands.len() >= COMMAND_CAPACITY {
            return Err(EngineError::Backpressure);
        }
        let sequence = self.next_command;
        let Some(next) = sequence.checked_add(1) else {
            self.input_retired = true;
            return Err(EngineError::Backend { code: None });
        };
        self.next_command = next;
        self.pending_commands.insert(sequence);
        Ok(CommandEnvelope {
            id: CommandId {
                browser: self.snapshot.browser,
                sequence,
            },
            command,
        })
    }

    pub fn input_epoch(&self) -> u64 {
        self.input_epoch
    }
    pub fn input_retired(&self) -> bool {
        self.input_retired
    }

    /// Independent coalescing cleanup slot, including Closing and full normal lane.
    /// No recovery from sequence/epoch overflow is possible for this session.
    pub fn admit_input_reset(
        &mut self,
        reason: crate::input::InputResetReason,
    ) -> Result<Option<CommandEnvelope>, EngineError> {
        match self.snapshot.state {
            SessionState::Alive | SessionState::Closing => {}
            SessionState::Closed => return Err(EngineError::Closed),
            SessionState::Creating | SessionState::CreateFailed => {
                return Err(EngineError::NotReady);
            }
        }
        if self.input_retired {
            return Err(EngineError::Backend { code: None });
        }
        if self.pending_reset.is_some() {
            return Ok(None);
        }
        let Some(next_epoch) = self.input_epoch.checked_add(1) else {
            self.input_retired = true;
            return Err(EngineError::Backend { code: None });
        };
        let sequence = self.next_command;
        let Some(next_sequence) = sequence.checked_add(1) else {
            self.input_retired = true;
            return Err(EngineError::Backend { code: None });
        };
        self.next_command = next_sequence;
        self.input_epoch = next_epoch;
        self.pending_reset = Some(sequence);
        Ok(Some(CommandEnvelope {
            id: CommandId {
                browser: self.snapshot.browser,
                sequence,
            },
            command: BrowserCommand::ResetInput(reason),
        }))
    }

    pub fn complete_input_reset(&mut self, id: CommandId) -> bool {
        if id.browser != self.snapshot.browser || self.pending_reset != Some(id.sequence) {
            return false;
        }
        self.pending_reset = None;
        true
    }

    pub fn input_reset_pending(&self) -> bool {
        self.pending_reset.is_some()
    }
    pub fn input_reset_id(&self) -> Option<CommandId> {
        self.pending_reset.map(|sequence| CommandId {
            browser: self.snapshot.browser,
            sequence,
        })
    }

    /// Reserve the independent reply lane after the adapter has verified the
    /// one-shot native request. It shares the command sequence, not its capacity.
    pub fn admit_dialog_response(
        &mut self,
        request: crate::ports::RequestId,
        allow: bool,
    ) -> Result<CommandEnvelope, EngineError> {
        if request.browser != self.snapshot.browser {
            return Err(EngineError::WrongRuntime);
        }
        match self.snapshot.state {
            SessionState::Alive | SessionState::Closing => {}
            SessionState::Closed => return Err(EngineError::Closed),
            _ => return Err(EngineError::NotReady),
        }
        if self.pending_replies.len() >= COMMAND_CAPACITY {
            return Err(EngineError::Backpressure);
        }
        let sequence = self.next_command;
        let Some(next) = sequence.checked_add(1) else {
            self.input_retired = true;
            return Err(EngineError::Backend { code: None });
        };
        self.next_command = next;
        self.pending_replies.insert(sequence);
        Ok(CommandEnvelope {
            id: CommandId {
                browser: self.snapshot.browser,
                sequence,
            },
            command: BrowserCommand::ResolveBeforeUnload { request, allow },
        })
    }

    /// Reserve the independent reply lane for finishing or cancelling an in-flight
    /// outbound drag even if the session is already draining or the normal queue is full.
    pub fn admit_outbound_drag_finish(
        &mut self,
        finish: crate::outbound_drag::OutboundDragFinish,
    ) -> Result<CommandEnvelope, EngineError> {
        if self.config.options.surface_mode.is_native_child_hwnd() {
            return Err(EngineError::InvalidRequest);
        }
        if finish.drag_id.browser != self.snapshot.browser {
            return Err(EngineError::WrongRuntime);
        }
        match self.snapshot.state {
            SessionState::Alive | SessionState::Closing => {}
            SessionState::Closed => return Err(EngineError::Closed),
            _ => return Err(EngineError::NotReady),
        }
        if self.pending_replies.len() >= COMMAND_CAPACITY {
            return Err(EngineError::Backpressure);
        }
        let sequence = self.next_command;
        let Some(next) = sequence.checked_add(1) else {
            self.input_retired = true;
            return Err(EngineError::Backend { code: None });
        };
        self.next_command = next;
        self.pending_replies.insert(sequence);
        Ok(CommandEnvelope {
            id: CommandId {
                browser: self.snapshot.browser,
                sequence,
            },
            command: BrowserCommand::FinishOutboundDrag(finish),
        })
    }

    /// Release an admitted command that native code rejected before execution.
    ///
    /// The full browser identity is checked so a foreign envelope with the
    /// same local id cannot release this session's capacity.
    pub fn cancel_command(&mut self, id: CommandId) -> bool {
        if id.browser != self.snapshot.browser {
            return false;
        }
        self.pending_commands.remove(&id.sequence) || self.pending_replies.remove(&id.sequence)
    }

    /// Take ownership of a previously admitted command.
    ///
    /// Commands admitted before a close request may still execute while the
    /// session is closing. Terminal sessions, duplicates, and foreign
    /// envelopes are rejected.
    pub fn consume_command(&mut self, envelope: CommandEnvelope) -> Option<AcceptedCommand> {
        if envelope.id.browser != self.snapshot.browser {
            return None;
        }
        if !self.pending_commands.remove(&envelope.id.sequence)
            && !self.pending_replies.remove(&envelope.id.sequence)
        {
            return None;
        }
        match self.snapshot.state {
            SessionState::Alive | SessionState::Closing => Some(AcceptedCommand { envelope }),
            SessionState::Creating | SessionState::Closed | SessionState::CreateFailed => None,
        }
    }

    /// Number of admitted commands that have not been consumed or cancelled.
    pub fn pending_commands(&self) -> usize {
        self.pending_commands.len()
    }

    /// For the UI adapter's ordered, reentrant input gate only.
    pub fn has_pending_command(&self, id: CommandId) -> bool {
        id.browser == self.snapshot.browser
            && (self.pending_commands.contains(&id.sequence)
                || self.pending_replies.contains(&id.sequence))
    }

    /// Apply an owned CEF page observation without changing native lifecycle.
    /// The registry validates the full BrowserId before calling this reducer.
    pub fn apply_navigation_event(&mut self, event: &BrowserEvent) -> bool {
        self.apply_navigation_observation(event, None)
    }

    /// A request URL or callback URL is owned before crossing the native boundary.
    /// Reject known previous-URL main-frame errors/status/cancellation after a new target.
    /// CEF does not expose a navigation ID here; equal-URL reloads cannot be
    /// distinguished by inventing an epoch at callback arrival time.
    pub fn apply_navigation_observation(
        &mut self,
        event: &BrowserEvent,
        source_url: Option<&str>,
    ) -> bool {
        if matches!(
            self.snapshot.state,
            SessionState::Closed | SessionState::CreateFailed
        ) {
            return false;
        }
        if matches!(
            event,
            BrowserEvent::LoadFailed {
                is_main_frame: true,
                ..
            } | BrowserEvent::LoadFinished {
                is_main_frame: true,
                ..
            } | BrowserEvent::LoadCancelled {
                is_main_frame: true,
                ..
            }
        ) && self
            .navigation_target
            .as_deref()
            .is_some_and(|target| source_url != Some(target))
        {
            return false;
        }
        if matches!(
            event,
            BrowserEvent::NavigationRequested {
                is_main_frame: true,
                ..
            }
        ) {
            self.navigation_target = source_url.map(str::to_owned);
        }
        let navigation = &mut self.snapshot.navigation;
        match event {
            BrowserEvent::AddressChanged(address) => navigation.address.clone_from(address),
            BrowserEvent::TitleChanged(title) => navigation.title.clone_from(title),
            BrowserEvent::CursorChanged(cursor) => navigation.cursor = *cursor,
            BrowserEvent::TooltipChanged(tooltip) => navigation.tooltip.clone_from(tooltip),
            BrowserEvent::StatusMessageChanged(status) => {
                navigation.status_message.clone_from(status)
            }
            BrowserEvent::FaviconUrlsChanged(urls) => navigation.favicon_urls.clone_from(urls),
            BrowserEvent::FullscreenModeChanged(fullscreen) => navigation.fullscreen = *fullscreen,
            BrowserEvent::LoadingProgressChanged(progress) => {
                navigation.loading_progress_per_mille = (*progress).min(1000)
            }
            BrowserEvent::ZoomLevelChanged(zoom_milli) => navigation.zoom_level_milli = *zoom_milli,
            BrowserEvent::AudioMutedChanged(muted) => navigation.audio_muted = *muted,
            BrowserEvent::ConsoleMessage { .. }
            | BrowserEvent::FocusTaken { .. }
            | BrowserEvent::GotFocus
            | BrowserEvent::HostShortcutTriggered { .. }
            | BrowserEvent::StartOutboundDrag(_)
            | BrowserEvent::OutboundDragCursorChanged(_)
            | BrowserEvent::AccessibilityTreeUpdated { .. } => {}
            BrowserEvent::LoadingChanged {
                loading,
                can_go_back,
                can_go_forward,
            } => {
                navigation.is_loading = *loading;
                navigation.can_go_back = *can_go_back;
                navigation.can_go_forward = *can_go_forward;
            }
            BrowserEvent::NavigationRequested {
                is_main_frame: true,
                is_redirect: false,
            } => {
                navigation.last_main_frame_error = None;
                navigation.last_sub_frame_error = None;
                navigation.main_http_status = None;
            }
            BrowserEvent::LoadFailed {
                is_main_frame: true,
                code,
            } => {
                navigation.last_main_frame_error = Some(*code);
                navigation.main_http_status = None;
            }
            BrowserEvent::LoadFailed {
                is_main_frame: false,
                code,
            } => {
                navigation.last_sub_frame_error = Some(*code);
            }
            BrowserEvent::LoadFinished {
                is_main_frame: true,
                http_status_code,
            } => {
                // CEF may deliver an end after an error (including its error
                // document). Do not reinterpret that as origin-page success.
                if navigation.last_main_frame_error.is_none() {
                    navigation.main_http_status = (100..=599)
                        .contains(http_status_code)
                        .then_some(*http_status_code);
                }
            }
            BrowserEvent::NavigationRequested { .. }
            | BrowserEvent::LoadStarted { .. }
            | BrowserEvent::LoadFinished { .. }
            | BrowserEvent::LoadCancelled { .. } => {}
            _ => return false,
        }
        true
    }

    /// Renderer health is independent from native Browser lifecycle and load.
    /// No clock or frame observation may synthesize these events.
    pub fn apply_renderer_event(&mut self, event: &BrowserEvent) -> bool {
        use crate::contracts::{RendererState, RendererTermination};
        if matches!(
            self.snapshot.state,
            SessionState::Closed | SessionState::CreateFailed
        ) {
            return false;
        }
        let renderer = &mut self.snapshot.renderer;
        match event {
            BrowserEvent::RenderViewReady => renderer.state = RendererState::Running,
            BrowserEvent::Unresponsive if renderer.state != RendererState::Terminated => {
                renderer.state = RendererState::Unresponsive
            }
            BrowserEvent::Responsive if renderer.state == RendererState::Unresponsive => {
                renderer.state = RendererState::Running
            }
            BrowserEvent::RendererTerminated {
                native_status,
                code,
            } => {
                renderer.state = RendererState::Terminated;
                renderer.last_termination = Some(RendererTermination {
                    native_status: *native_status,
                    code: *code,
                });
            }
            _ => return false,
        }
        true
    }

    pub fn configuration(&self) -> &BrowserConfig {
        &self.config
    }

    pub fn snapshot(&self) -> BrowserSnapshot {
        self.snapshot.clone()
    }
}

/// Owned token for exactly one admitted command. It is not cloneable.
pub struct AcceptedCommand {
    envelope: CommandEnvelope,
}

impl AcceptedCommand {
    pub fn id(&self) -> CommandId {
        self.envelope.id
    }

    /// Hand the command to the injected engine. Call this outside registry locks.
    pub fn execute(self, engine: &mut dyn BrowserEngine) -> Result<(), EngineError> {
        engine.execute(self.envelope)
    }
}

#[cfg(test)]
mod input_tests {
    use super::*;
    use crate::{
        contracts::{RenderPreference, RuntimeGeneration, Viewport},
        input::InputResetReason,
    };

    #[test]
    fn reset_slot_survives_full_normal_lane_and_close() {
        let browser = BrowserId::new(RuntimeGeneration::new(1), 2);
        let config = BrowserConfig {
            initial_url: String::new(),
            profile: None,
            render_preference: RenderPreference::SoftwareRequired,
            viewport: Viewport {
                width: 200,
                height: 100,
                scale: 1.0,
            },
            options: Default::default(),
        };
        let mut session = Session::begin_create(browser, config).unwrap();
        session.after_created();
        for _ in 0..COMMAND_CAPACITY {
            session.admit_command(BrowserCommand::Back).unwrap();
        }
        assert_eq!(
            session.admit_command(BrowserCommand::Back).err(),
            Some(EngineError::Backpressure)
        );
        session.close_requested();
        let reset = session
            .admit_input_reset(InputResetReason::Closing)
            .unwrap()
            .unwrap();
        assert_eq!(reset.id.sequence, (COMMAND_CAPACITY + 1) as u64);
        assert!(
            session
                .admit_input_reset(InputResetReason::Rejected)
                .unwrap()
                .is_none()
        );
        assert_eq!(session.pending_commands(), COMMAND_CAPACITY);
        assert!(session.complete_input_reset(reset.id));
        assert!(!session.input_reset_pending());
        session.closed();
        assert_eq!(
            session.admit_input_reset(InputResetReason::Closing).err(),
            Some(EngineError::Closed)
        );
    }

    #[test]
    fn overflow_retires_only_input_without_reusing_epoch() {
        let browser = BrowserId::new(RuntimeGeneration::new(1), 9);
        let config = BrowserConfig {
            initial_url: String::new(),
            profile: None,
            render_preference: RenderPreference::SoftwareRequired,
            viewport: Viewport {
                width: 200,
                height: 100,
                scale: 1.0,
            },
            options: Default::default(),
        };
        let mut session = Session::begin_create(browser, config).unwrap();
        session.after_created();
        session.input_epoch = u64::MAX;
        assert_eq!(
            session.admit_input_reset(InputResetReason::Rejected).err(),
            Some(EngineError::Backend { code: None })
        );
        assert!(session.input_retired());
        assert_eq!(
            session
                .admit_command(BrowserCommand::Input(crate::input::InputEvent {
                    epoch: u64::MAX,
                    kind: crate::input::InputKind::Text(String::new()),
                }))
                .err(),
            Some(EngineError::Backend { code: None })
        );
        assert!(session.admit_command(BrowserCommand::Back).is_ok());
        let browser2 = BrowserId::new(RuntimeGeneration::new(1), 10);
        let mut exhausted =
            Session::begin_create(browser2, session.configuration().clone()).unwrap();
        exhausted.after_created();
        exhausted.next_command = u64::MAX;
        assert_eq!(
            exhausted
                .admit_command(BrowserCommand::Input(crate::input::InputEvent {
                    epoch: 1,
                    kind: crate::input::InputKind::Text(String::new()),
                }))
                .err(),
            Some(EngineError::Backend { code: None })
        );
        assert!(exhausted.input_retired());
    }
}
