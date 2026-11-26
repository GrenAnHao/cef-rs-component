//! Isolated navigation probe. Never used as the CEF helper executable.
//!
//! The probe witnesses real Browser navigation against the same-host
//! `navigation_fixture`: address/title/history, redirect scope, main versus
//! sub-frame load errors, and a real StopLoading cancellation that is not a
//! Runtime failure. Command acceptance and frame delivery are never used as
//! navigation success. The report carries check labels, booleans and raw CEF
//! error codes only; page URLs, queries, titles and bodies stay out of it.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod windows {
    use rust_cef_runtime::{
        contracts::{
            BrowserConfig, BrowserId, ProfileStorage, RenderPreference, RuntimeConfig,
            RuntimeFiles, SessionState, ThreadMode, TrustedContentPolicy, Viewport,
        },
        navigation_fixture::{
            NavigationFixture, PATH_A, PATH_B, PATH_MAIN_ERROR, PATH_REDIRECT, PATH_SLOW,
            PATH_WITH_CHILD,
        },
        ports::{
            BrowserCommand, BrowserEvent, BrowserEventEnvelope, BrowserEventSink, CloseMode,
            EngineError, FrameSink, OwnedFrame, PublishResult, SurfaceKind, UiDispatcher,
        },
        runtime::{Runtime, RuntimeServices, ShutdownStatus, StartError},
        test_support::{ProbeOptions, ProbeScenario, publish_observer_ready},
    };
    use serde::Serialize;
    use std::{
        sync::{
            Arc, Mutex, MutexGuard,
            atomic::{AtomicU32, AtomicU64, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    mod retired_load {
        include!("navigation-probe/retired_load.rs");
    }

    const FRAME_WIDTH: i32 = 320;
    const FRAME_HEIGHT: i32 = 240;
    const FRAME_WIDTH_U32: u32 = 320;
    const FRAME_HEIGHT_U32: u32 = 240;
    // RGB(26,76,126) background in premultiplied BGRA.
    const CENTER_BGRA: [u8; 4] = [126, 76, 26, 255];

    const PHASE_TIMEOUT: Duration = Duration::from_secs(20);
    const OBSERVER_TIMEOUT: Duration = Duration::from_secs(30);
    const SCENARIO_BUDGET: Duration = Duration::from_secs(90);
    const SHUTDOWN_BUDGET: Duration = Duration::from_secs(20);
    const CANCEL_SETTLE: Duration = Duration::from_millis(300);
    const EVENT_CAPACITY: usize = 1024;

    // Fixed evidence set. Missing, duplicated, reordered or unknown rows make
    // the strict validator fail even when the candidate process exits zero.
    const REQUIRED_CHECKS: [&str; 28] = [
        "runtime_ready",
        "browser_created",
        "first_frame",
        "initial_address",
        "initial_title",
        "initial_loading_false",
        "initial_history_no_back",
        "observer_released",
        "redirect_address",
        "redirect_title",
        "redirect_history",
        "redirect_http_requests",
        "redirect_event",
        "back_address",
        "back_can_forward",
        "forward_address",
        "forward_history",
        "reload_request_count",
        "reload_loading_cycle",
        "main_error",
        "main_error_runtime_alive",
        "child_main_clean",
        "child_subframe_error",
        "cancel_event",
        "cancel_no_load_failed",
        "cancel_runtime_alive",
        "recovery_address",
        "recovery_state",
    ];

    #[derive(Clone, Serialize)]
    struct Check {
        id: &'static str,
        passed: bool,
    }

    #[derive(Serialize)]
    struct NavigationProbeReport {
        schema: u32,
        kind: &'static str,
        success: bool,
        requested_mode: &'static str,
        requested_backend: &'static str,
        actual_mode: Option<&'static str>,
        actual_backend: Option<&'static str>,
        pid: u32,
        main_thread_id: u64,
        init_thread_id: Option<u64>,
        cef_ui_thread_id: Option<u64>,
        shutdown_thread_id: Option<u64>,
        manual_pump_count: u64,
        created: u64,
        closed: u64,
        valid_frame_browsers: u64,
        frames: u64,
        first_frame_width: u32,
        first_frame_height: u32,
        tasks_posted: u64,
        tasks_completed: u64,
        task_post_failures: u64,
        pending_tasks: u64,
        pending_requests: u64,
        pending_profiles: u64,
        pending_cookie_flushes: u64,
        profile_references_retained: u64,
        native_calls: u64,
        native_calls_after_shutdown: u64,
        cef_shutdown_calls: u64,
        app_reference_retained: Option<bool>,
        root_lease_held: bool,
        runtime_generation: Option<u64>,
        default_profile_id: Option<u64>,
        default_profile_ready: bool,
        observer_released: bool,
        main_error_code: Option<i32>,
        sub_error_code: Option<i32>,
        cancel_code: Option<i32>,
        checks: Vec<Check>,
        error: Option<String>,
        exit_code: Option<i32>,
    }

    impl NavigationProbeReport {
        fn started(options: &ProbeOptions, main_thread_id: u64) -> Self {
            Self {
                schema: 1,
                kind: "navigation",
                success: false,
                requested_mode: options.mode_name(),
                requested_backend: options.backend_name(),
                actual_mode: None,
                actual_backend: None,
                pid: std::process::id(),
                main_thread_id,
                init_thread_id: None,
                cef_ui_thread_id: None,
                shutdown_thread_id: None,
                manual_pump_count: 0,
                created: 0,
                closed: 0,
                valid_frame_browsers: 0,
                frames: 0,
                first_frame_width: 0,
                first_frame_height: 0,
                tasks_posted: 0,
                tasks_completed: 0,
                task_post_failures: 0,
                pending_tasks: 0,
                pending_requests: 0,
                pending_profiles: 0,
                pending_cookie_flushes: 0,
                profile_references_retained: 0,
                native_calls: 0,
                native_calls_after_shutdown: 0,
                cef_shutdown_calls: 0,
                app_reference_retained: None,
                root_lease_held: true,
                runtime_generation: None,
                default_profile_id: None,
                default_profile_ready: false,
                observer_released: false,
                main_error_code: None,
                sub_error_code: None,
                cancel_code: None,
                checks: Vec::new(),
                error: None,
                exit_code: None,
            }
        }

        /// Strict evidence check. `success` is deliberately not consulted:
        /// only a complete fixed check set may become success after exit zero.
        fn validate(&self) -> Result<(), String> {
            if self.schema != 1 {
                return Err(format!("schema must be 1, got {}", self.schema));
            }
            if self.kind != "navigation" {
                return Err(format!("kind must be navigation, got {}", self.kind));
            }
            if self.error.is_some() {
                return Err("error must be absent".into());
            }
            if self.exit_code != Some(0) {
                return Err(format!(
                    "candidate exit code must be 0, got {:?}",
                    self.exit_code
                ));
            }
            if self.pid == 0 {
                return Err("pid must be nonzero".into());
            }
            if self.requested_backend != "Software" {
                return Err("only SoftwareRequired is implemented".into());
            }
            if self.actual_backend != Some("Software") {
                return Err(format!(
                    "actual backend must be Software, got {:?}",
                    self.actual_backend
                ));
            }
            if self.actual_mode != Some(self.requested_mode) {
                return Err(format!(
                    "actual mode {:?} does not match requested {}",
                    self.actual_mode, self.requested_mode
                ));
            }
            if self.main_thread_id == 0 {
                return Err("main thread id must be nonzero".into());
            }
            if self.init_thread_id != Some(self.main_thread_id) {
                return Err("init thread must be the main thread".into());
            }
            if self.shutdown_thread_id != Some(self.main_thread_id) {
                return Err("shutdown thread must be the main thread".into());
            }
            let cef_ui = self
                .cef_ui_thread_id
                .ok_or_else(|| "cef UI thread id is missing".to_string())?;
            if cef_ui == 0 {
                return Err("cef UI thread id must be nonzero".into());
            }
            match self.requested_mode {
                "Integrated" => {
                    if cef_ui != self.main_thread_id {
                        return Err("Integrated CEF UI must be the main thread".into());
                    }
                    if self.manual_pump_count == 0 {
                        return Err("Integrated mode must record manual pumps".into());
                    }
                }
                "Threaded" => {
                    if cef_ui == self.main_thread_id {
                        return Err("Threaded CEF UI must not be the main thread".into());
                    }
                    if self.manual_pump_count != 0 {
                        return Err("Threaded mode must never manually pump".into());
                    }
                }
                other => return Err(format!("unknown requested mode: {other}")),
            }
            if self.created != 1 {
                return Err(format!(
                    "exactly one browser must be created, got {}",
                    self.created
                ));
            }
            if self.closed != 1 {
                return Err(format!(
                    "exactly one browser must be closed, got {}",
                    self.closed
                ));
            }
            if self.valid_frame_browsers != 1 {
                return Err(format!(
                    "valid frame browsers must be 1, got {}",
                    self.valid_frame_browsers
                ));
            }
            if self.frames == 0 {
                return Err("no frames were delivered".into());
            }
            if self.first_frame_width != FRAME_WIDTH_U32
                || self.first_frame_height != FRAME_HEIGHT_U32
            {
                return Err(format!(
                    "first valid frame must be {FRAME_WIDTH_U32}x{FRAME_HEIGHT_U32}, got {}x{}",
                    self.first_frame_width, self.first_frame_height
                ));
            }
            if self.tasks_posted == 0 {
                return Err("no tasks were posted".into());
            }
            if self.pending_tasks != 0 {
                return Err("pending tasks must be zero after shutdown".into());
            }
            if self.tasks_completed.checked_add(self.task_post_failures) != Some(self.tasks_posted)
            {
                return Err("task completed/failure counters do not add up".into());
            }
            if self.cef_shutdown_calls != 1 {
                return Err(format!(
                    "cef shutdown calls must be 1, got {}",
                    self.cef_shutdown_calls
                ));
            }
            if self.native_calls == 0 {
                return Err("no native calls were recorded".into());
            }
            if self.native_calls_after_shutdown != 0 {
                return Err("native calls occurred after shutdown".into());
            }
            if self.pending_requests != 0 {
                return Err("pending requests must be zero".into());
            }
            if self.pending_profiles != 0 {
                return Err("pending profiles must be zero".into());
            }
            if self.pending_cookie_flushes != 0 {
                return Err("pending cookie flushes must be zero".into());
            }
            if self.profile_references_retained != 0 {
                return Err("profile references must be released".into());
            }
            if self.app_reference_retained != Some(false) {
                return Err("App reference must be released".into());
            }
            if self.root_lease_held {
                return Err("root lease must be released".into());
            }
            if self.runtime_generation.is_none_or(|value| value == 0) {
                return Err("runtime generation must be nonzero".into());
            }
            if self.default_profile_id != Some(0) {
                return Err("default profile id must be 0".into());
            }
            if !self.default_profile_ready {
                return Err("default profile was not Ready".into());
            }
            if !self.observer_released {
                return Err("external observer release was not observed".into());
            }
            if self
                .main_error_code
                .is_none_or(|code| code == 0 || code == -3)
            {
                return Err(format!(
                    "main frame error must be real and not aborted, got {:?}",
                    self.main_error_code
                ));
            }
            if self
                .sub_error_code
                .is_none_or(|code| code == 0 || code == -3)
            {
                return Err(format!(
                    "sub frame error must be real and not aborted, got {:?}",
                    self.sub_error_code
                ));
            }
            if self.cancel_code != Some(-3) {
                return Err(format!(
                    "cancellation must be ERR_ABORTED(-3), got {:?}",
                    self.cancel_code
                ));
            }
            if self.checks.len() != REQUIRED_CHECKS.len() {
                return Err(format!(
                    "checks must be exactly {}, got {}",
                    REQUIRED_CHECKS.len(),
                    self.checks.len()
                ));
            }
            for (index, required) in REQUIRED_CHECKS.iter().enumerate() {
                let check = &self.checks[index];
                if check.id != *required {
                    return Err(format!(
                        "check {} must be {}, got {}",
                        index, required, check.id
                    ));
                }
                if !check.passed {
                    return Err(format!("check did not pass: {}", check.id));
                }
            }
            Ok(())
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Kind {
        Created,
        Closed,
        AddressChanged,
        TitleChanged,
        LoadingChanged {
            loading: bool,
        },
        NavigationRequested {
            is_main_frame: bool,
            is_redirect: bool,
        },
        LoadStarted {
            is_main_frame: bool,
        },
        LoadFinished {
            is_main_frame: bool,
            http_status_code: i32,
        },
        LoadCancelled {
            is_main_frame: bool,
            code: i32,
        },
        LoadFailed {
            is_main_frame: bool,
            code: i32,
        },
    }

    #[derive(Clone)]
    struct Recorded {
        browser: BrowserId,
        kind: Kind,
    }

    struct EventState {
        expected_browser: Option<BrowserId>,
        last_sequence: u64,
        phase: &'static str,
        events: Vec<Recorded>,
        created: u64,
        closed: u64,
        error: Option<String>,
    }

    impl Default for EventState {
        fn default() -> Self {
            Self {
                expected_browser: None,
                last_sequence: 0,
                phase: "startup",
                events: Vec::new(),
                created: 0,
                closed: 0,
                error: None,
            }
        }
    }

    impl EventState {
        fn record_error(&mut self, message: impl Into<String>) {
            if self.error.is_none() {
                self.error = Some(message.into());
            }
        }

        /// Phase-scoped error policy. Expected main/sub frame errors are only
        /// admitted in their own phase; a late old failure cannot be absorbed
        /// into a later phase count or cleared by a later success.
        fn admit(&mut self, kind: Kind) {
            match kind {
                Kind::Created => {
                    if self.created != 0 {
                        self.record_error("duplicate Created lifecycle event");
                    }
                    self.created = self.created.saturating_add(1);
                }
                Kind::Closed => {
                    self.closed = self.closed.saturating_add(1);
                    if self.phase != "shutdown" {
                        self.record_error("Closed outside the shutdown phase");
                    }
                }
                Kind::LoadFailed {
                    is_main_frame: true,
                    code,
                } => {
                    if self.phase != "main-error" || code == 0 || code == -3 {
                        self.record_error(format!(
                            "unexpected main LoadFailed code {code} in phase {}",
                            self.phase
                        ));
                    }
                }
                Kind::LoadFailed {
                    is_main_frame: false,
                    code,
                } => {
                    if self.phase != "child" || code == 0 || code == -3 {
                        self.record_error(format!(
                            "unexpected subframe LoadFailed code {code} in phase {}",
                            self.phase
                        ));
                    }
                }
                Kind::LoadCancelled {
                    is_main_frame: true,
                    code,
                } => {
                    // ERR_ABORTED is an expected cancellation, including when
                    // CEF aborts an in-flight main load as the host starts a
                    // different navigation. It is never a Runtime failure. Any
                    // other main cancellation code is unexpected.
                    if code != -3 {
                        self.record_error(format!(
                            "unexpected main LoadCancelled code {code} in phase {}",
                            self.phase
                        ));
                    }
                }
                Kind::LoadCancelled {
                    is_main_frame: false,
                    code: _,
                } => {
                    // A sub-frame teardown can legitimately abort while the main
                    // document navigates away. It is never main cancellation
                    // evidence and never a Runtime failure.
                }
                Kind::AddressChanged
                | Kind::TitleChanged
                | Kind::LoadingChanged { .. }
                | Kind::NavigationRequested { .. }
                | Kind::LoadStarted { .. }
                | Kind::LoadFinished { .. } => {}
            }
        }
    }

    #[derive(Default)]
    struct Observations {
        events: Mutex<EventState>,
        frames: AtomicU64,
        width: AtomicU32,
        height: AtomicU32,
        wakes: AtomicU64,
        valid_browser: Mutex<Option<BrowserId>>,
        frame_error: Mutex<Option<String>>,
    }

    impl Observations {
        fn set_browser(&self, browser: BrowserId) {
            let mut state = lock(&self.events);
            match state.expected_browser {
                None => {
                    if let Some(foreign) = state
                        .events
                        .iter()
                        .map(|record| record.browser)
                        .find(|id| *id != browser)
                    {
                        state.record_error(format!(
                            "early event used browser {} not {}",
                            foreign.local(),
                            browser.local()
                        ));
                    }
                    state.expected_browser = Some(browser);
                }
                Some(current) if current != browser => {
                    state.record_error("only one Browser may be observed by this probe");
                }
                Some(_) => {}
            }
            if lock(&self.valid_browser).is_some_and(|seen| seen != browser) {
                lock(&self.frame_error).get_or_insert_with(|| {
                    "early valid frame BrowserId does not match the handle".into()
                });
            }
        }

        fn begin_phase(&self, phase: &'static str) -> usize {
            let mut state = lock(&self.events);
            state.phase = phase;
            state.events.len()
        }

        fn events_since(&self, mark: usize) -> Vec<Recorded> {
            let state = lock(&self.events);
            state
                .events
                .get(mark.min(state.events.len())..)
                .unwrap_or(&[])
                .to_vec()
        }

        fn events_error(&self) -> Option<String> {
            lock(&self.events).error.clone()
        }

        fn created(&self) -> u64 {
            lock(&self.events).created
        }

        fn frame_error(&self) -> Option<String> {
            lock(&self.frame_error).clone()
        }

        fn valid_frame(&self) -> bool {
            let expected = lock(&self.events).expected_browser;
            expected.is_some()
                && *lock(&self.valid_browser) == expected
                && self.width.load(Ordering::Acquire) == FRAME_WIDTH_U32
                && self.height.load(Ordering::Acquire) == FRAME_HEIGHT_U32
        }

        fn apply_to(&self, report: &mut NavigationProbeReport) {
            let state = lock(&self.events);
            report.created = state.created;
            report.closed = state.closed;
            if let Some(error) = state.error.clone() {
                report.error.get_or_insert(error);
            }
            report.frames = self.frames.load(Ordering::Relaxed);
            report.first_frame_width = self.width.load(Ordering::Acquire);
            report.first_frame_height = self.height.load(Ordering::Acquire);
            report.valid_frame_browsers = u64::from(
                state.expected_browser.is_some()
                    && *lock(&self.valid_browser) == state.expected_browser,
            );
            if let Some(error) = self.frame_error() {
                report.error.get_or_insert(error);
            }
        }
    }

    impl BrowserEventSink for Observations {
        fn publish(&self, envelope: BrowserEventEnvelope) -> PublishResult {
            let mut state = lock(&self.events);
            if let Some(expected) = state.expected_browser {
                if envelope.browser != expected {
                    state.record_error("event BrowserId does not match the observed Browser");
                }
            }
            if envelope.sequence <= state.last_sequence {
                state.record_error("event sequence is not strictly increasing");
            } else {
                state.last_sequence = envelope.sequence;
            }
            let kind = match &envelope.event {
                BrowserEvent::Created => Some(Kind::Created),
                BrowserEvent::Closed => Some(Kind::Closed),
                BrowserEvent::AddressChanged(_) => Some(Kind::AddressChanged),
                BrowserEvent::TitleChanged(_) => Some(Kind::TitleChanged),
                BrowserEvent::LoadingChanged {
                    loading,
                    can_go_back: _,
                    can_go_forward: _,
                } => Some(Kind::LoadingChanged { loading: *loading }),
                BrowserEvent::NavigationRequested {
                    is_main_frame,
                    is_redirect,
                } => Some(Kind::NavigationRequested {
                    is_main_frame: *is_main_frame,
                    is_redirect: *is_redirect,
                }),
                BrowserEvent::LoadStarted { is_main_frame } => Some(Kind::LoadStarted {
                    is_main_frame: *is_main_frame,
                }),
                BrowserEvent::LoadFinished {
                    is_main_frame,
                    http_status_code,
                } => Some(Kind::LoadFinished {
                    is_main_frame: *is_main_frame,
                    http_status_code: *http_status_code,
                }),
                BrowserEvent::LoadCancelled {
                    is_main_frame,
                    code,
                } => Some(Kind::LoadCancelled {
                    is_main_frame: *is_main_frame,
                    code: *code,
                }),
                BrowserEvent::LoadFailed {
                    is_main_frame,
                    code,
                } => Some(Kind::LoadFailed {
                    is_main_frame: *is_main_frame,
                    code: *code,
                }),
                BrowserEvent::CreateFailed(_) => {
                    state.record_error("unexpected CreateFailed event");
                    None
                }
                BrowserEvent::CaptureFailed(_) => {
                    state.record_error("unexpected CaptureFailed event");
                    None
                }
                BrowserEvent::RendererTerminated { .. } => {
                    state.record_error("unexpected RendererTerminated event");
                    None
                }
                BrowserEvent::PolicyDenied { .. } => {
                    state.record_error("unexpected PolicyDenied event");
                    None
                }
                BrowserEvent::BeforeUnloadRequested { .. } => {
                    state.record_error("unexpected BeforeUnloadRequested event");
                    None
                }
                BrowserEvent::Unresponsive => {
                    state.record_error("unexpected Unresponsive event");
                    None
                }
                BrowserEvent::RenderViewReady
                | BrowserEvent::Responsive
                | BrowserEvent::CursorChanged(_)
                | BrowserEvent::TooltipChanged(_)
                | BrowserEvent::StatusMessageChanged(_)
                | BrowserEvent::FaviconUrlsChanged(_)
                | BrowserEvent::FullscreenModeChanged(_)
                | BrowserEvent::LoadingProgressChanged(_)
                | BrowserEvent::ConsoleMessage { .. }
                | BrowserEvent::ZoomLevelChanged(_)
                | BrowserEvent::AudioMutedChanged(_)
                | BrowserEvent::FocusTaken { .. }
                | BrowserEvent::GotFocus
                | BrowserEvent::HostShortcutTriggered { .. }
                | BrowserEvent::JsMessageReceived { .. }
                | BrowserEvent::EvalCompleted { .. }
                | BrowserEvent::StartOutboundDrag(_)
                | BrowserEvent::OutboundDragCursorChanged(_)
                | BrowserEvent::AccessibilityTreeUpdated { .. }
                | BrowserEvent::PopupRequested(_)
                | BrowserEvent::DraggableRegionsChanged(_)
                | BrowserEvent::AutoResized { .. }
                | BrowserEvent::MediaAccessChanged { .. }
                | BrowserEvent::DocumentAvailableInMainFrame
                | BrowserEvent::FrameCreated(_)
                | BrowserEvent::FrameAttached { .. }
                | BrowserEvent::FrameDetached { .. }
                | BrowserEvent::MainFrameChanged { .. }
                | BrowserEvent::ContextCreated { .. }
                | BrowserEvent::ContextReleased { .. }
                | BrowserEvent::FocusedNodeChanged(_)
                | BrowserEvent::UncaughtJsException(_)
                | BrowserEvent::ResourceResponseObserved(_)
                | BrowserEvent::ResourceLoadCompleted(_) => None,
                BrowserEvent::CloseCancelled => {
                    state.record_error("unexpected CloseCancelled event");
                    None
                }
            };
            if let Some(kind) = kind {
                state.admit(kind);
                if state.events.len() < EVENT_CAPACITY {
                    state.events.push(Recorded {
                        browser: envelope.browser,
                        kind,
                    });
                } else {
                    state.record_error("navigation event capacity exceeded");
                }
            }
            PublishResult::Published
        }
    }

    impl FrameSink for Observations {
        fn publish(&self, frame: OwnedFrame) -> PublishResult {
            if frame.info.surface != SurfaceKind::View {
                return PublishResult::Rejected;
            }
            self.frames.fetch_add(1, Ordering::Relaxed);
            let expected = lock(&self.events).expected_browser;
            if expected.is_some_and(|id| id != frame.info.browser) {
                lock(&self.frame_error)
                    .get_or_insert_with(|| "frame BrowserId does not match the handle".into());
                return PublishResult::Rejected;
            }
            // A blank initial paint is not navigation evidence. Require the
            // fixture page's actual opaque BGRA center pixel and dimensions.
            let width = frame.info.width;
            let height = frame.info.height;
            let center = ((height as usize / 2) * width as usize + width as usize / 2) * 4;
            if width == FRAME_WIDTH_U32
                && height == FRAME_HEIGHT_U32
                && frame.cpu_pixels().and_then(|px| px.get(center..center + 4))
                    == Some(&CENTER_BGRA)
            {
                let mut valid = lock(&self.valid_browser);
                match *valid {
                    None => {
                        *valid = Some(frame.info.browser);
                        self.width.store(width, Ordering::Release);
                        self.height.store(height, Ordering::Release);
                    }
                    Some(browser) if browser == frame.info.browser => {}
                    Some(_) => {
                        let mut error = lock(&self.frame_error);
                        if error.is_none() {
                            *error = Some("valid frame BrowserId does not match".into());
                        }
                    }
                }
            }
            PublishResult::Published
        }
    }

    impl UiDispatcher for Observations {
        fn wake(&self) -> Result<(), EngineError> {
            self.wakes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    fn os_thread_id() -> u64 {
        // SAFETY: no arguments, always succeeds on Windows, touches no CEF state.
        unsafe { GetCurrentThreadId() as u64 }
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }

    fn tick(runtime: &mut Runtime, mode: ThreadMode) -> Result<(), String> {
        if mode == ThreadMode::Integrated {
            runtime.pump().map_err(|error| format!("pump: {error:?}"))?;
        }
        thread::sleep(Duration::from_millis(3));
        Ok(())
    }

    fn phase_deadline(case_deadline: Instant) -> Instant {
        let phase = Instant::now() + PHASE_TIMEOUT;
        if phase < case_deadline {
            phase
        } else {
            case_deadline
        }
    }

    fn first_fixture_error(fixture: &NavigationFixture) -> Option<String> {
        fixture.errors().into_iter().next()
    }

    #[allow(clippy::too_many_arguments)]
    fn wait_for<F>(
        runtime: &mut Runtime,
        mode: ThreadMode,
        deadline: Instant,
        fixture: &NavigationFixture,
        observations: &Observations,
        label: &str,
        mut condition: F,
    ) -> Result<(), String>
    where
        F: FnMut() -> Result<bool, String>,
    {
        loop {
            if let Some(error) = observations.events_error() {
                return Err(format!("{label}: {error}"));
            }
            if let Some(error) = observations.frame_error() {
                return Err(format!("{label}: {error}"));
            }
            if let Some(error) = first_fixture_error(fixture) {
                return Err(format!("{label}: fixture protocol error: {error}"));
            }
            if condition()? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("{label}: timeout"));
            }
            tick(runtime, mode)?;
        }
    }

    fn wait_runtime_ready(
        runtime: &mut Runtime,
        mode: ThreadMode,
        deadline: Instant,
    ) -> Result<(), String> {
        loop {
            if runtime.is_ready() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("RuntimeReady timeout".into());
            }
            tick(runtime, mode)?;
        }
    }

    fn tap_errors(fixture: &NavigationFixture, observations: &Observations) -> Result<(), String> {
        if let Some(error) = observations.events_error() {
            return Err(error);
        }
        if let Some(error) = observations.frame_error() {
            return Err(error);
        }
        if let Some(error) = first_fixture_error(fixture) {
            return Err(format!("fixture protocol error: {error}"));
        }
        Ok(())
    }

    fn record_check(
        report: &mut NavigationProbeReport,
        id: &'static str,
        passed: bool,
    ) -> Result<(), String> {
        if report.checks.iter().any(|check| check.id == id) {
            return Err(format!("duplicate check: {id}"));
        }
        report.checks.push(Check { id, passed });
        if passed {
            Ok(())
        } else {
            Err(format!("check failed: {id}"))
        }
    }

    fn submit(
        handle: &rust_cef_runtime::ports::BrowserHandle,
        label: &str,
        command: BrowserCommand,
    ) -> Result<(), String> {
        handle
            .submit(command)
            .map(|_| ())
            .map_err(|error| format!("{label}: {error:?}"))
    }

    fn main_failed_codes(records: &[Recorded]) -> Vec<i32> {
        records
            .iter()
            .filter_map(|record| match record.kind {
                Kind::LoadFailed {
                    is_main_frame: true,
                    code,
                } => Some(code),
                _ => None,
            })
            .collect()
    }

    fn sub_failed_codes(records: &[Recorded]) -> Vec<i32> {
        records
            .iter()
            .filter_map(|record| match record.kind {
                Kind::LoadFailed {
                    is_main_frame: false,
                    code,
                } => Some(code),
                _ => None,
            })
            .collect()
    }

    fn main_cancelled_codes(records: &[Recorded]) -> Vec<i32> {
        records
            .iter()
            .filter_map(|record| match record.kind {
                Kind::LoadCancelled {
                    is_main_frame: true,
                    code,
                } => Some(code),
                _ => None,
            })
            .collect()
    }

    fn has_main_redirect(records: &[Recorded]) -> bool {
        records.iter().any(|record| {
            matches!(
                record.kind,
                Kind::NavigationRequested {
                    is_main_frame: true,
                    is_redirect: true,
                }
            )
        })
    }

    fn has_main_request(records: &[Recorded]) -> bool {
        records.iter().any(|record| {
            matches!(
                record.kind,
                Kind::NavigationRequested {
                    is_main_frame: true,
                    is_redirect: false,
                }
            )
        })
    }

    fn has_main_finished(records: &[Recorded], status: i32) -> bool {
        records.iter().any(|record| {
            matches!(
                record.kind,
                Kind::LoadFinished {
                    is_main_frame: true,
                    http_status_code,
                } if http_status_code == status
            )
        })
    }

    fn has_main_started(records: &[Recorded]) -> bool {
        records.iter().any(|record| {
            matches!(
                record.kind,
                Kind::LoadStarted {
                    is_main_frame: true
                }
            )
        })
    }

    fn has_loading_cycle(records: &[Recorded]) -> bool {
        let mut started = false;
        for record in records {
            match record.kind {
                Kind::LoadingChanged { loading: true } => started = true,
                Kind::LoadingChanged { loading: false } if started => return true,
                _ => {}
            }
        }
        false
    }

    fn scenario(
        runtime: &mut Runtime,
        options: &ProbeOptions,
        fixture: &NavigationFixture,
        observations: &Observations,
        report: &mut NavigationProbeReport,
        case_deadline: Instant,
    ) -> Result<(), String> {
        wait_runtime_ready(runtime, options.mode, phase_deadline(case_deadline))?;
        report.default_profile_ready = runtime
            .profile_state(runtime.default_profile_handle())
            .map(|state| state == rust_cef_runtime::runtime::ProfileState::Ready)
            .unwrap_or(false);
        if !report.default_profile_ready {
            return Err("default profile is not Ready after RuntimeReady".into());
        }
        record_check(report, "runtime_ready", true)?;

        let path_a = fixture.url(PATH_A)?;
        let path_b = fixture.url(PATH_B)?;
        let path_redirect = fixture.url(PATH_REDIRECT)?;
        let path_main_error = fixture.url(PATH_MAIN_ERROR)?;
        let path_with_child = fixture.url(PATH_WITH_CHILD)?;
        let path_slow = fixture.url(PATH_SLOW)?;

        // Initial navigation is part of the create task; no second process and
        // no Browser restart for the whole scenario.
        let initial_mark = observations.begin_phase("initial");
        let handle = runtime
            .create_browser(BrowserConfig {
                initial_url: path_a.clone(),
                profile: None,
                render_preference: RenderPreference::SoftwareRequired,
                viewport: Viewport {
                    width: FRAME_WIDTH,
                    height: FRAME_HEIGHT,
                    scale: 1.0,
                },
                options: Default::default(),
            })
            .map_err(|error| format!("create browser: {error:?}"))?;
        observations.set_browser(handle.snapshot().browser);

        wait_for(
            runtime,
            options.mode,
            phase_deadline(case_deadline),
            fixture,
            observations,
            "initial page",
            || {
                let snapshot = handle.snapshot();
                Ok(snapshot.state == SessionState::Alive
                    && observations.created() == 1
                    && observations.valid_frame()
                    && has_main_started(&observations.events_since(initial_mark))
                    && snapshot.navigation.address == path_a
                    && snapshot.navigation.title == "Nav A"
                    && !snapshot.navigation.is_loading
                    && !snapshot.navigation.can_go_back
                    && !snapshot.navigation.can_go_forward)
            },
        )?;

        let initial = handle.snapshot();
        record_check(
            report,
            "browser_created",
            observations.created() == 1 && initial.state == SessionState::Alive,
        )?;
        record_check(report, "first_frame", observations.valid_frame())?;
        record_check(
            report,
            "initial_address",
            initial.navigation.address == path_a && fixture.request_count(PATH_A) == Some(1),
        )?;
        record_check(report, "initial_title", initial.navigation.title == "Nav A")?;
        record_check(
            report,
            "initial_loading_false",
            !initial.navigation.is_loading,
        )?;
        record_check(
            report,
            "initial_history_no_back",
            !initial.navigation.can_go_back && !initial.navigation.can_go_forward,
        )?;

        // Existing observer handshake: publish readiness only for a real live
        // Browser, then wait for the external launcher's release file.
        let gate = options
            .observer_gate
            .as_ref()
            .ok_or_else(|| "observer gate is required for navigation evidence".to_string())?;
        observations.begin_phase("observer");
        publish_observer_ready(gate, std::process::id())
            .map_err(|error| format!("observer ready: {error}"))?;
        let observer_deadline = {
            let requested = Instant::now() + OBSERVER_TIMEOUT;
            if requested < case_deadline {
                requested
            } else {
                case_deadline
            }
        };
        while !gate.is_file() {
            if Instant::now() >= observer_deadline {
                return Err("external observer release timeout".into());
            }
            tap_errors(fixture, observations)?;
            tick(runtime, options.mode)?;
        }
        report.observer_released = true;
        record_check(report, "observer_released", true)?;

        // Redirect: the initial /redirect request and its redirected /b request
        // are both real HTTP requests. is_redirect is not inferred from the
        // resulting address.
        let redirect_mark = observations.begin_phase("redirect");
        submit(
            &handle,
            "navigate redirect",
            BrowserCommand::Navigate(path_redirect),
        )?;
        wait_for(
            runtime,
            options.mode,
            phase_deadline(case_deadline),
            fixture,
            observations,
            "redirect page",
            || {
                let snapshot = handle.snapshot();
                Ok(snapshot.navigation.address == path_b
                    && snapshot.navigation.title == "Nav B"
                    && !snapshot.navigation.is_loading
                    && snapshot.navigation.can_go_back
                    && snapshot.navigation.main_http_status == Some(200))
            },
        )?;
        let redirect = handle.snapshot();
        let redirect_records = observations.events_since(redirect_mark);
        record_check(
            report,
            "redirect_address",
            redirect.navigation.address == path_b,
        )?;
        record_check(
            report,
            "redirect_title",
            redirect.navigation.title == "Nav B",
        )?;
        record_check(
            report,
            "redirect_history",
            redirect.navigation.can_go_back
                && !redirect.navigation.is_loading
                && redirect.navigation.main_http_status == Some(200),
        )?;
        record_check(
            report,
            "redirect_http_requests",
            fixture.request_count(PATH_REDIRECT) == Some(1)
                && fixture.request_count(PATH_B) == Some(1),
        )?;
        record_check(
            report,
            "redirect_event",
            has_main_redirect(&redirect_records),
        )?;

        // Back to /a; forward history must become available.
        observations.begin_phase("back");
        submit(&handle, "back", BrowserCommand::Back)?;
        wait_for(
            runtime,
            options.mode,
            phase_deadline(case_deadline),
            fixture,
            observations,
            "back page",
            || {
                let snapshot = handle.snapshot();
                Ok(snapshot.navigation.address == path_a
                    && snapshot.navigation.title == "Nav A"
                    && !snapshot.navigation.is_loading
                    && snapshot.navigation.can_go_forward)
            },
        )?;
        let back = handle.snapshot();
        record_check(report, "back_address", back.navigation.address == path_a)?;
        record_check(
            report,
            "back_can_forward",
            back.navigation.can_go_forward && !back.navigation.is_loading,
        )?;

        // Forward to /b.
        observations.begin_phase("forward");
        submit(&handle, "forward", BrowserCommand::Forward)?;
        wait_for(
            runtime,
            options.mode,
            phase_deadline(case_deadline),
            fixture,
            observations,
            "forward page",
            || {
                let snapshot = handle.snapshot();
                Ok(snapshot.navigation.address == path_b
                    && snapshot.navigation.title == "Nav B"
                    && !snapshot.navigation.is_loading
                    && snapshot.navigation.can_go_back
                    && !snapshot.navigation.can_go_forward)
            },
        )?;
        let forward = handle.snapshot();
        record_check(
            report,
            "forward_address",
            forward.navigation.address == path_b,
        )?;
        record_check(
            report,
            "forward_history",
            forward.navigation.can_go_back
                && !forward.navigation.can_go_forward
                && !forward.navigation.is_loading,
        )?;

        // Reload must cause a genuinely new /b request and a full loading
        // cycle, not replay the cached snapshot.
        let reload_before = fixture.request_count(PATH_B).unwrap_or(0);
        let reload_mark = observations.begin_phase("reload");
        submit(&handle, "reload", BrowserCommand::Reload)?;
        wait_for(
            runtime,
            options.mode,
            phase_deadline(case_deadline),
            fixture,
            observations,
            "reload page",
            || {
                let snapshot = handle.snapshot();
                Ok(snapshot.navigation.address == path_b
                    && snapshot.navigation.title == "Nav B"
                    && !snapshot.navigation.is_loading
                    && snapshot.navigation.main_http_status == Some(200)
                    && fixture
                        .request_count(PATH_B)
                        .is_some_and(|count| count >= reload_before + 1)
                    && has_loading_cycle(&observations.events_since(reload_mark)))
            },
        )?;
        let reload = handle.snapshot();
        let reload_records = observations.events_since(reload_mark);
        record_check(
            report,
            "reload_request_count",
            fixture.request_count(PATH_B) == Some(reload_before + 1),
        )?;
        record_check(
            report,
            "reload_loading_cycle",
            has_loading_cycle(&reload_records) && reload.navigation.main_http_status == Some(200),
        )?;

        // Main-frame network failure: a real closed connection, non-aborted
        // error code, while the Runtime and Browser remain usable.
        let main_error_mark = observations.begin_phase("main-error");
        submit(
            &handle,
            "navigate main-error",
            BrowserCommand::Navigate(path_main_error),
        )?;
        wait_for(
            runtime,
            options.mode,
            phase_deadline(case_deadline),
            fixture,
            observations,
            "main error page",
            || {
                let snapshot = handle.snapshot();
                Ok(snapshot.navigation.last_main_frame_error.is_some()
                    && !snapshot.navigation.is_loading)
            },
        )?;
        let main_error = handle.snapshot();
        let main_error_code = main_error
            .navigation
            .last_main_frame_error
            .filter(|code| *code != 0 && *code != -3);
        let main_error_records = observations.events_since(main_error_mark);
        report.main_error_code = main_error_code;
        record_check(
            report,
            "main_error",
            main_error_code
                .is_some_and(|code| main_failed_codes(&main_error_records).contains(&code))
                && main_error.navigation.main_http_status.is_none()
                && sub_failed_codes(&main_error_records).is_empty()
                && main_cancelled_codes(&main_error_records).is_empty()
                && fixture
                    .request_count(PATH_MAIN_ERROR)
                    .is_some_and(|count| count >= 1),
        )?;
        record_check(
            report,
            "main_error_runtime_alive",
            runtime.is_ready() && handle.snapshot().state == SessionState::Alive,
        )?;

        // /with-child: the parent is a real HTTP 200, the sub-frame fails on
        // its own closed connection, and the previous main error is cleared.
        let child_mark = observations.begin_phase("child");
        submit(
            &handle,
            "navigate with-child",
            BrowserCommand::Navigate(path_with_child.clone()),
        )?;
        wait_for(
            runtime,
            options.mode,
            phase_deadline(case_deadline),
            fixture,
            observations,
            "child error page",
            || {
                let snapshot = handle.snapshot();
                Ok(snapshot.navigation.address == path_with_child
                    && snapshot.navigation.title == "Nav Parent"
                    && !snapshot.navigation.is_loading
                    && snapshot.navigation.main_http_status == Some(200)
                    && snapshot.navigation.last_main_frame_error.is_none()
                    && snapshot.navigation.last_sub_frame_error.is_some())
            },
        )?;
        let child = handle.snapshot();
        let child_records = observations.events_since(child_mark);
        let sub_error_code = child
            .navigation
            .last_sub_frame_error
            .filter(|code| *code != 0 && *code != -3);
        report.sub_error_code = sub_error_code;
        record_check(
            report,
            "child_main_clean",
            child.navigation.address == path_with_child
                && child.navigation.title == "Nav Parent"
                && child.navigation.main_http_status == Some(200)
                && child.navigation.last_main_frame_error.is_none()
                && has_main_request(&child_records)
                && has_main_finished(&child_records, 200)
                && main_failed_codes(&child_records).is_empty(),
        )?;
        record_check(
            report,
            "child_subframe_error",
            sub_error_code.is_some_and(|code| sub_failed_codes(&child_records).contains(&code))
                && main_failed_codes(&child_records).is_empty(),
        )?;

        // /slow: the server has accepted the request and sent no headers. Stop
        // only after loading is genuinely true, then expect ERR_ABORTED(-3).
        let slow_mark = observations.begin_phase("slow-load");
        submit(
            &handle,
            "navigate slow",
            BrowserCommand::Navigate(path_slow),
        )?;
        wait_for(
            runtime,
            options.mode,
            phase_deadline(case_deadline),
            fixture,
            observations,
            "slow request accepted",
            || {
                let snapshot = handle.snapshot();
                Ok(fixture.slow_request_count() >= 1
                    && snapshot.navigation.is_loading
                    && has_main_request(&observations.events_since(slow_mark)))
            },
        )?;
        let cancel_mark = observations.begin_phase("cancel");
        submit(&handle, "stop loading", BrowserCommand::StopLoading)?;
        wait_for(
            runtime,
            options.mode,
            phase_deadline(case_deadline),
            fixture,
            observations,
            "cancellation",
            || {
                let snapshot = handle.snapshot();
                let records = observations.events_since(cancel_mark);
                Ok(!snapshot.navigation.is_loading && main_cancelled_codes(&records).contains(&-3))
            },
        )?;
        // Bounded settle: a late old failure cannot be hidden by advancing to
        // the recovery phase immediately.
        let settle_deadline = {
            let requested = Instant::now() + CANCEL_SETTLE;
            if requested < case_deadline {
                requested
            } else {
                case_deadline
            }
        };
        while Instant::now() < settle_deadline {
            tap_errors(fixture, observations)?;
            tick(runtime, options.mode)?;
        }
        let cancel = handle.snapshot();
        let cancel_records = observations.events_since(cancel_mark);
        let cancelled = main_cancelled_codes(&cancel_records);
        let cancel_code = cancelled.iter().copied().find(|code| *code == -3);
        report.cancel_code = cancel_code;
        record_check(
            report,
            "cancel_event",
            cancel_code == Some(-3) && !cancel.navigation.is_loading,
        )?;
        record_check(
            report,
            "cancel_no_load_failed",
            main_failed_codes(&cancel_records).is_empty()
                && sub_failed_codes(&cancel_records).is_empty()
                && cancel.navigation.last_main_frame_error.is_none(),
        )?;
        record_check(
            report,
            "cancel_runtime_alive",
            runtime.is_ready() && handle.snapshot().state == SessionState::Alive,
        )?;
        // Release the held response only after the cancellation evidence; the
        // real abort means its write may fail with BrokenPipe/ConnectionReset.
        fixture.release_slow();

        // Recovery: a fresh navigation after both network failure and
        // cancellation must succeed and clear both error scopes.
        let recovery_before = fixture.request_count(PATH_A).unwrap_or(0);
        let recovery_mark = observations.begin_phase("recovery");
        submit(
            &handle,
            "navigate recovery",
            BrowserCommand::Navigate(path_a.clone()),
        )?;
        wait_for(
            runtime,
            options.mode,
            phase_deadline(case_deadline),
            fixture,
            observations,
            "recovery page",
            || {
                let snapshot = handle.snapshot();
                Ok(snapshot.navigation.address == path_a
                    && snapshot.navigation.title == "Nav A"
                    && !snapshot.navigation.is_loading
                    && snapshot.navigation.main_http_status == Some(200)
                    && snapshot.navigation.last_main_frame_error.is_none()
                    && snapshot.navigation.last_sub_frame_error.is_none()
                    && fixture
                        .request_count(PATH_A)
                        .is_some_and(|count| count > recovery_before))
            },
        )?;
        let recovery = handle.snapshot();
        let recovery_records = observations.events_since(recovery_mark);
        record_check(
            report,
            "recovery_address",
            recovery.navigation.address == path_a,
        )?;
        record_check(
            report,
            "recovery_state",
            recovery.navigation.title == "Nav A"
                && !recovery.navigation.is_loading
                && recovery.navigation.main_http_status == Some(200)
                && recovery.navigation.last_main_frame_error.is_none()
                && recovery.navigation.last_sub_frame_error.is_none()
                && has_main_request(&recovery_records)
                && has_main_finished(&recovery_records, 200)
                && runtime.is_ready()
                && recovery.state == SessionState::Alive,
        )?;

        Ok(())
    }

    fn drain(runtime: &mut Runtime, mode: ThreadMode) -> bool {
        let deadline = Instant::now() + SHUTDOWN_BUDGET;
        let ticket = runtime.request_shutdown(CloseMode::Force, deadline);
        let mut finished = false;
        loop {
            match runtime.poll_shutdown(&ticket) {
                ShutdownStatus::Ready => match runtime.finish_shutdown() {
                    Ok(_) => {
                        finished = true;
                        break;
                    }
                    Err(_) => break,
                },
                ShutdownStatus::Stopped => break,
                ShutdownStatus::ProcessExitRequired { .. } => break,
                ShutdownStatus::NotRequested
                | ShutdownStatus::Draining { .. }
                | ShutdownStatus::Blocked { .. } => {}
            }
            if Instant::now() >= deadline {
                break;
            }
            if tick(runtime, mode).is_err() {
                break;
            }
        }
        finished
    }

    fn native(options: &ProbeOptions, report: &mut NavigationProbeReport) -> i32 {
        if options.backend != RenderPreference::SoftwareRequired {
            report.error =
                Some("D3d11Required is not implemented; refusing to fall back to Software".into());
            return 1;
        }
        if options.scenario != ProbeScenario::Smoke {
            report.error = Some("navigation probe implements the Smoke scenario only".into());
            return 1;
        }
        let host = match std::env::current_exe() {
            Ok(host) => host,
            Err(error) => {
                report.error = Some(error.to_string());
                return 1;
            }
        };
        let mut fixture = match NavigationFixture::new() {
            Ok(fixture) => fixture,
            Err(error) => {
                report.error = Some(format!("navigation fixture: {error}"));
                return 1;
            }
        };
        let config = RuntimeConfig {
            thread_mode: options.mode,
            files: RuntimeFiles {
                directory: host.parent().unwrap().to_path_buf(),
                helper_executable: options.helper.clone(),
            },
            root_cache_path: options.report.parent().unwrap().join("cache"),
            default_profile: ProfileStorage::Memory,
            content_policy: TrustedContentPolicy {
                allowed_origins: vec![fixture.origin().to_string()],
                allow_embedded_data: true,
            },
            settings: Default::default(),
        };
        let observations = Arc::new(Observations::default());
        let services = RuntimeServices {
            events: observations.clone(),
            frames: observations.clone(),
            ui: observations.clone(),
        };
        let mut runtime = match Runtime::start(config, services) {
            Ok(runtime) => runtime,
            Err(error) => {
                report.error = Some(format!("start: {error:?}"));
                return match error {
                    StartError::ProcessExitRequired { exit_code }
                    | StartError::InitializeFailed { exit_code } => exit_code.unwrap_or(1),
                    _ => 1,
                };
            }
        };
        let default = runtime.default_profile_handle();
        report.runtime_generation = Some(default.runtime().value());
        report.default_profile_id = Some(default.local());

        let case_deadline = Instant::now() + SCENARIO_BUDGET;
        let scenario_result = scenario(
            &mut runtime,
            options,
            &fixture,
            observations.as_ref(),
            report,
            case_deadline,
        );
        let scenario_failed = scenario_result.is_err();
        if let Err(error) = scenario_result {
            report.error.get_or_insert(error);
        }

        // On any failure still release the slow holder and attempt the normal
        // Force drain; an abnormal process exit would not be evidence. The
        // shutdown phase is the only place a Closed event is admissible.
        fixture.release_slow();
        observations.begin_phase("shutdown");
        let finished = drain(&mut runtime, options.mode);
        retain_fixture_failure(report, fixture.finish());
        if !finished {
            report
                .error
                .get_or_insert_with(|| "shutdown did not complete".into());
        }

        let diagnostics = runtime.diagnostics();
        if diagnostics.main_thread_id != report.main_thread_id {
            report.error.get_or_insert_with(|| {
                "Runtime main thread differs from the OS thread that started it".into()
            });
        }
        report.main_thread_id = diagnostics.main_thread_id;
        report.init_thread_id = Some(diagnostics.init_thread_id);
        report.cef_ui_thread_id = diagnostics
            .cef_ui_thread_id
            .or(diagnostics.context_thread_id);
        report.shutdown_thread_id = diagnostics.shutdown_thread_id;
        report.manual_pump_count = diagnostics.manual_pump_count;
        report.tasks_posted = diagnostics.tasks.posted;
        report.tasks_completed = diagnostics.tasks.completed;
        report.task_post_failures = diagnostics.tasks.post_failed;
        report.pending_tasks = diagnostics.tasks.pending;
        report.pending_requests = diagnostics.pending_requests;
        report.pending_profiles = diagnostics.pending_profiles;
        report.pending_cookie_flushes = diagnostics.pending_cookie_flushes;
        report.profile_references_retained = diagnostics.profile_references_retained;
        report.native_calls = diagnostics.native_calls;
        report.native_calls_after_shutdown = diagnostics.native_calls_after_shutdown;
        report.cef_shutdown_calls = diagnostics.cef_shutdown_calls;
        report.app_reference_retained = Some(diagnostics.app_reference_retained);
        report.root_lease_held = diagnostics.root_lease_held;
        if diagnostics
            .context_thread_id
            .is_some_and(|id| match options.mode {
                ThreadMode::Integrated => id == diagnostics.main_thread_id,
                ThreadMode::Threaded => id != diagnostics.main_thread_id,
            })
        {
            report.actual_mode = Some(options.mode_name());
        }
        report.actual_backend = match diagnostics.observed_backend {
            Some(RenderPreference::SoftwareRequired) => Some("Software"),
            Some(RenderPreference::D3d11Required) => Some("D3d11"),
            _ => None,
        };
        observations.apply_to(report);
        if !finished {
            // Test-host failure only: retain native owners until main exits this
            // process, rather than destruct an undrained registry on this thread.
            std::mem::forget(runtime);
        }
        if finished && !scenario_failed && report.error.is_none() {
            0
        } else {
            1
        }
    }

    fn retain_fixture_failure(report: &mut NavigationProbeReport, errors: Vec<String>) {
        if let Some(first) = errors.into_iter().next() {
            report
                .error
                .get_or_insert_with(|| format!("fixture protocol error: {first}"));
        }
    }

    fn finalize(report: &mut NavigationProbeReport, candidate: i32) -> i32 {
        report.exit_code = Some(candidate);
        if candidate != 0 {
            report.success = false;
            if report.error.is_none() {
                report.error = Some(format!("navigation probe exited with {candidate}"));
            }
            return candidate;
        }
        match report.validate() {
            Ok(()) => {
                report.success = true;
                0
            }
            Err(error) => {
                report.success = false;
                report
                    .error
                    .get_or_insert_with(|| format!("incomplete navigation evidence: {error}"));
                report.exit_code = Some(1);
                1
            }
        }
    }

    pub fn run() -> i32 {
        let mut args = std::env::args().skip(1).peekable();
        let retired_load_only = args
            .peek()
            .is_some_and(|arg| arg == "--retired-load-isolation");
        if retired_load_only {
            let _ = args.next();
        }
        let options = match ProbeOptions::parse(args) {
            Ok(options) => options,
            Err(error) => {
                eprintln!("{error}");
                return 2;
            }
        };
        if retired_load_only {
            return retired_load::run(&options);
        }
        let mut report = NavigationProbeReport::started(&options, os_thread_id());
        let candidate = native(&options, &mut report);
        let code = finalize(&mut report, candidate);
        let written = serde_json::to_vec_pretty(&report)
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                std::fs::write(&options.report, bytes).map_err(|error| error.to_string())
            });
        if let Err(error) = written {
            eprintln!("report: {error}");
            return 1;
        }
        code
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn started() -> NavigationProbeReport {
            let options = ProbeOptions::parse(vec![
                "--mode".into(),
                "Threaded".into(),
                "--backend".into(),
                "Software".into(),
                "--helper".into(),
                std::env::temp_dir()
                    .join("helper.exe")
                    .to_string_lossy()
                    .into_owned(),
                "--report".into(),
                std::env::temp_dir()
                    .join("navigation report.json")
                    .to_string_lossy()
                    .into_owned(),
                "--scenario".into(),
                "Smoke".into(),
            ])
            .unwrap();
            let mut report = NavigationProbeReport::started(&options, 1);
            report.requested_mode = "Threaded";
            report.actual_mode = Some("Threaded");
            report.actual_backend = Some("Software");
            report.pid = 1234;
            report.main_thread_id = 1;
            report.init_thread_id = Some(1);
            report.cef_ui_thread_id = Some(2);
            report.shutdown_thread_id = Some(1);
            report.manual_pump_count = 0;
            report.created = 1;
            report.closed = 1;
            report.valid_frame_browsers = 1;
            report.frames = 1;
            report.first_frame_width = FRAME_WIDTH_U32;
            report.first_frame_height = FRAME_HEIGHT_U32;
            report.tasks_posted = 4;
            report.tasks_completed = 4;
            report.pending_tasks = 0;
            report.pending_requests = 0;
            report.pending_profiles = 0;
            report.pending_cookie_flushes = 0;
            report.profile_references_retained = 0;
            report.native_calls = 10;
            report.native_calls_after_shutdown = 0;
            report.cef_shutdown_calls = 1;
            report.app_reference_retained = Some(false);
            report.root_lease_held = false;
            report.runtime_generation = Some(1);
            report.default_profile_id = Some(0);
            report.default_profile_ready = true;
            report.observer_released = true;
            report.main_error_code = Some(-324);
            report.sub_error_code = Some(-324);
            report.cancel_code = Some(-3);
            report.checks = REQUIRED_CHECKS
                .iter()
                .map(|id| Check {
                    id: *id,
                    passed: true,
                })
                .collect();
            report.exit_code = Some(0);
            report
        }

        #[test]
        fn foreign_valid_frames_are_rejected_before_and_after_handle_binding() {
            let expected =
                BrowserId::new(rust_cef_runtime::contracts::RuntimeGeneration::new(1), 1);
            let foreign = BrowserId::new(rust_cef_runtime::contracts::RuntimeGeneration::new(2), 1);
            let frame = || {
                let mut buffer = rust_cef_core::frame_budget::FrameBudget::default()
                    .allocate((FRAME_WIDTH_U32 * FRAME_HEIGHT_U32 * 4) as usize)
                    .unwrap();
                for pixel in buffer.as_mut_slice().chunks_exact_mut(4) {
                    pixel.copy_from_slice(&CENTER_BGRA);
                }
                OwnedFrame::from_cpu_pixels(
                    rust_cef_runtime::ports::FrameInfo {
                        browser: foreign,
                        surface: SurfaceKind::View,
                        surface_generation: 1,
                        sequence: 1,
                        width: FRAME_WIDTH_U32,
                        height: FRAME_HEIGHT_U32,
                        format: rust_cef_runtime::ports::PixelFormat::PremultipliedBgra8,
                    },
                    buffer.freeze(),
                )
            };
            let early = Observations::default();
            FrameSink::publish(&early, frame());
            early.set_browser(expected);
            assert!(!early.valid_frame());
            assert!(early.frame_error().is_some());
            let bound = Observations::default();
            bound.set_browser(expected);
            assert_eq!(FrameSink::publish(&bound, frame()), PublishResult::Rejected);
            assert!(!bound.valid_frame());
            assert!(bound.frame_error().is_some());
        }

        #[test]
        fn report_requires_the_exact_fixed_check_set() {
            let complete = started();
            assert!(complete.validate().is_ok());

            let mut missing = started();
            missing.checks.pop();
            assert!(missing.validate().is_err());

            let mut extra = started();
            extra.checks.push(Check {
                id: "unexpected",
                passed: true,
            });
            assert!(extra.validate().is_err());

            let mut reordered = started();
            reordered.checks.swap(0, 1);
            assert!(reordered.validate().is_err());

            let mut failed = started();
            failed.checks[12].passed = false;
            assert!(failed.validate().is_err());
        }

        #[test]
        fn report_requires_real_main_sub_and_cancel_codes() {
            let mut aborted_main = started();
            aborted_main.main_error_code = Some(-3);
            assert!(aborted_main.validate().is_err());

            let mut missing_sub = started();
            missing_sub.sub_error_code = None;
            assert!(missing_sub.validate().is_err());

            let mut wrong_cancel = started();
            wrong_cancel.cancel_code = Some(-2);
            assert!(wrong_cancel.validate().is_err());
        }
    }
}

fn main() {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    std::process::exit(windows::run());
    #[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
    {
        eprintln!("native navigation probe requires Windows x64 MSVC");
        std::process::exit(2);
    }
}
