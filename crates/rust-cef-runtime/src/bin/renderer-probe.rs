//! Isolated native renderer probe. Never used as the CEF helper executable.
//!
//! This probe witnesses the real renderer lifecycle of one same-origin HTTP
//! Browser: a static page that must not be declared hung for producing no new
//! frames, a fixed DevTools `Page.enable` preflight, a controlled
//! `Page.crash`, and the actual CEF `RendererTerminated` callback for the same
//! Browser. Command acceptance, observer detachment, a missing frame, or a PID
//! are never used as termination evidence. After the fault the Browser must
//! remain Alive and no automatic reload may happen; only an explicit `Reload`
//! command may recover the same Browser. Normal Force close and the one native
//! shutdown stay on the same evidence path.
//!
//! The report carries fixed check labels, booleans, counts, actual renderer
//! status/code and thread ids only. Fixture URLs, query strings, titles,
//! bodies and page error strings stay out of it.
//!
//! Renderer recovery supports SoftwareRequired and D3d11Required. This does not
//! claim real hang detection, GUI/IME behavior, GPU faults or utility monitoring.
#![allow(clippy::too_many_arguments)]

#[cfg(all(
    target_os = "windows",
    target_arch = "x86_64",
    feature = "test-support"
))]
mod windows {
    use rust_cef_runtime::{
        contracts::{
            BrowserConfig, BrowserId, ProfileStorage, RenderPreference, RendererState,
            RendererTermination, RuntimeConfig, RuntimeFiles, SessionState, ThreadMode,
            TrustedContentPolicy, Viewport,
        },
        navigation_fixture::{NavigationFixture, PATH_A},
        ports::{
            BrowserCommand, BrowserEvent, BrowserEventEnvelope, BrowserEventSink, CloseMode,
            CloseResult, EngineError, FrameSink, OwnedFrame, PublishResult, SurfaceKind,
            UiDispatcher,
        },
        runtime::{Runtime, RuntimeServices, ShutdownStatus, StartError},
        test_support::{ProbeOptions, ProbeScenario, publish_observer_ready},
    };
    use serde::Serialize;
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::{Arc, Mutex, MutexGuard},
        thread,
        time::{Duration, Instant},
    };

    const FRAME_WIDTH: i32 = 320;
    const FRAME_HEIGHT: i32 = 240;
    const FRAME_WIDTH_U32: u32 = 320;
    const FRAME_HEIGHT_U32: u32 = 240;
    // RGB(26,76,126) fixture background in premultiplied BGRA.
    const CENTER_BGRA: [u8; 4] = [126, 76, 26, 255];
    const PAGE_TITLE: &str = "Nav A";

    const PHASE_TIMEOUT: Duration = Duration::from_secs(20);
    const CRASH_TIMEOUT: Duration = Duration::from_secs(30);
    const OBSERVER_TIMEOUT: Duration = Duration::from_secs(30);
    const SCENARIO_BUDGET: Duration = Duration::from_secs(90);
    const SHUTDOWN_BUDGET: Duration = Duration::from_secs(20);
    const STATIC_WINDOW: Duration = Duration::from_millis(350);
    const QUIESCENCE: Duration = Duration::from_millis(500);
    const FRAME_CAPACITY: usize = 256;

    // Fixed evidence set. A missing, duplicated, reordered or unknown row makes
    // the strict validator fail even when the candidate process exits zero.
    const REQUIRED_CHECKS: [&str; 18] = [
        "runtime_ready",
        "initial_state",
        "helper_observer",
        "static_not_hung",
        "devtools_enabled",
        "crash_submitted",
        "terminated_event",
        "browser_not_closed",
        "terminated_snapshot",
        "no_automatic_reload",
        "explicit_reload_admitted",
        "render_view_ready",
        "fresh_request_after_recovery",
        "fresh_frame_after_recovery",
        "same_browser_recovered",
        "normal_close",
        "native_shutdown",
        "threads",
    ];

    #[derive(Clone, Serialize)]
    struct Check {
        id: &'static str,
        passed: bool,
    }

    #[derive(Serialize)]
    struct RendererProbeReport {
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
        /// Observed renderer state right after the first real fixture frame.
        initial_renderer_state: Option<&'static str>,
        /// Actual DevTools preflight ACK, never the successful post itself.
        renderer_probe_enabled: Option<bool>,
        /// Actual execute returned a positive Page.crash id.
        crash_command_submitted: bool,
        terminated_events: u64,
        /// Actual CEF termination category; no value is assumed.
        terminated_native_status: Option<i32>,
        /// Actual CEF termination error; not a network error or a PID.
        terminated_code: Option<i32>,
        /// Diagnostic counts for the renderer health events. Their absence is
        /// proven by `static_not_hung`; they are never inferred from frames.
        unresponsive_events: u64,
        responsive_events: u64,
        static_window_frames: u64,
        requests_before_crash: u64,
        requests_after_quiescence: u64,
        requests_after_recovery: u64,
        frames_before_crash: u64,
        frames_after_recovery: u64,
        pre_crash_frame_sequence: u64,
        post_recovery_frame_sequence: u64,
        static_quiet_observed: bool,
        no_automatic_reload_observed: bool,
        reload_command_accepted: bool,
        fresh_request_after_recovery: bool,
        fresh_frame_after_recovery: bool,
        same_browser_recovered: bool,
        /// Retained renderer Registration count; must be zero after close.
        renderer_references_retained: u64,
        checks: Vec<Check>,
        error: Option<String>,
        exit_code: Option<i32>,
    }

    impl RendererProbeReport {
        fn started(options: &ProbeOptions, main_thread_id: u64) -> Self {
            Self {
                schema: 1,
                kind: "renderer",
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
                initial_renderer_state: None,
                renderer_probe_enabled: None,
                crash_command_submitted: false,
                terminated_events: 0,
                terminated_native_status: None,
                terminated_code: None,
                unresponsive_events: 0,
                responsive_events: 0,
                static_window_frames: 0,
                requests_before_crash: 0,
                requests_after_quiescence: 0,
                requests_after_recovery: 0,
                frames_before_crash: 0,
                frames_after_recovery: 0,
                pre_crash_frame_sequence: 0,
                post_recovery_frame_sequence: 0,
                static_quiet_observed: false,
                no_automatic_reload_observed: false,
                reload_command_accepted: false,
                fresh_request_after_recovery: false,
                fresh_frame_after_recovery: false,
                same_browser_recovered: false,
                renderer_references_retained: 0,
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
            if self.kind != "renderer" {
                return Err(format!("kind must be renderer, got {}", self.kind));
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
            if !matches!(self.requested_backend, "Software" | "D3d11") {
                return Err("renderer probe requires an explicit supported backend".into());
            }
            if self.actual_backend != Some(self.requested_backend) {
                return Err(format!(
                    "actual backend {:?} does not match requested {}",
                    self.actual_backend, self.requested_backend
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
            if self.frames < 2 {
                return Err(format!(
                    "at least the initial and recovered frames are required, got {}",
                    self.frames
                ));
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
            if !matches!(
                self.initial_renderer_state,
                Some("Starting") | Some("Running")
            ) {
                return Err(format!(
                    "initial renderer state must be healthy, got {:?}",
                    self.initial_renderer_state
                ));
            }
            if self.renderer_probe_enabled != Some(true) {
                return Err(format!(
                    "Page.enable must be acknowledged, got {:?}",
                    self.renderer_probe_enabled
                ));
            }
            if !self.crash_command_submitted {
                return Err("Page.crash was not submitted with a positive id".into());
            }
            if self.terminated_events != 1 {
                return Err(format!(
                    "exactly one RendererTerminated event is required, got {}",
                    self.terminated_events
                ));
            }
            if self.terminated_native_status.is_none() {
                return Err("actual renderer termination status is missing".into());
            }
            if self.terminated_code.is_none() {
                return Err("actual renderer termination code is missing".into());
            }
            if self.requests_before_crash == 0 {
                return Err("the initial fixture page was not requested".into());
            }
            if self.requests_after_quiescence != self.requests_before_crash {
                return Err(format!(
                    "quiescence requests {} != pre-crash {}",
                    self.requests_after_quiescence, self.requests_before_crash
                ));
            }
            if self.requests_before_crash.checked_add(1) != Some(self.requests_after_recovery) {
                return Err(format!(
                    "recovery must issue exactly one fresh request, got {} from {}",
                    self.requests_after_recovery, self.requests_before_crash
                ));
            }
            if self.frames_before_crash == 0 {
                return Err("no valid frame was observed before the crash".into());
            }
            if self.frames_after_recovery == 0 {
                return Err("no valid frame was observed after recovery".into());
            }
            if self.pre_crash_frame_sequence == 0 {
                return Err("pre-crash frame baseline is missing".into());
            }
            if self.post_recovery_frame_sequence <= self.pre_crash_frame_sequence {
                return Err(format!(
                    "recovered frame sequence {} does not exceed baseline {}",
                    self.post_recovery_frame_sequence, self.pre_crash_frame_sequence
                ));
            }
            if !self.static_quiet_observed {
                return Err("static page without new frames was treated as hung".into());
            }
            if !self.no_automatic_reload_observed {
                return Err("an automatic reload was observed".into());
            }
            if !self.reload_command_accepted {
                return Err("the explicit reload command was not admitted".into());
            }
            if !self.fresh_request_after_recovery {
                return Err("recovery did not issue a fresh fixture request".into());
            }
            if !self.fresh_frame_after_recovery {
                return Err("recovery did not deliver a fresh fixture frame".into());
            }
            if !self.same_browser_recovered {
                return Err("the same Browser was not recovered".into());
            }
            if self.renderer_references_retained != 0 {
                return Err("renderer probe references must be released".into());
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

    /// Renderer events are kept as counts. Identity, however, is retained: an
    /// early real event may arrive before `create_browser` returns its handle,
    /// so its full BrowserId is collected as a candidate and checked at bind.
    #[derive(Default)]
    struct EventState {
        bound: Option<BrowserId>,
        candidates: BTreeSet<BrowserId>,
        last_sequence: u64,
        phase: &'static str,
        created: u64,
        closed: u64,
        terminated: u64,
        unresponsive: u64,
        responsive: u64,
        render_view_ready: u64,
        main_load_started: u64,
        termination: Option<(i32, Option<i32>)>,
        error: Option<String>,
    }

    impl EventState {
        fn record_error(&mut self, message: impl Into<String>) {
            if self.error.is_none() {
                self.error = Some(message.into());
            }
        }
    }

    #[derive(Clone, Copy)]
    struct FrameRecord {
        browser: BrowserId,
        sequence: u64,
        valid: bool,
    }

    #[derive(Default)]
    struct FrameState {
        bound: Option<BrowserId>,
        candidates: BTreeSet<BrowserId>,
        last_sequence: BTreeMap<BrowserId, u64>,
        records: Vec<FrameRecord>,
        total: u64,
        first_width: u32,
        first_height: u32,
        error: Option<String>,
    }

    impl FrameState {
        fn record_error(&mut self, message: impl Into<String>) {
            if self.error.is_none() {
                self.error = Some(message.into());
            }
        }
    }

    #[derive(Default)]
    struct Observations {
        events: Mutex<EventState>,
        frames: Mutex<FrameState>,
    }

    impl Observations {
        /// Bind only after the handle exists. Every retained pre-bind candidate
        /// and every frame already recorded must use the same full BrowserId.
        fn bind(&self, browser: BrowserId) {
            {
                let mut state = lock(&self.events);
                let foreign = state
                    .candidates
                    .iter()
                    .copied()
                    .find(|candidate| *candidate != browser);
                if foreign.is_some() {
                    state.record_error(
                        "an early event used a BrowserId that is not the created Browser",
                    );
                }
                state.bound = Some(browser);
            }
            {
                let mut state = lock(&self.frames);
                let foreign = state
                    .candidates
                    .iter()
                    .copied()
                    .chain(state.records.iter().map(|record| record.browser))
                    .find(|candidate| *candidate != browser);
                if foreign.is_some() {
                    state.record_error(
                        "an early frame used a BrowserId that is not the created Browser",
                    );
                }
                state.bound = Some(browser);
            }
        }

        fn begin_phase(&self, phase: &'static str) {
            lock(&self.events).phase = phase;
        }

        fn created(&self) -> u64 {
            lock(&self.events).created
        }

        fn closed(&self) -> u64 {
            lock(&self.events).closed
        }

        fn terminated_count(&self) -> u64 {
            lock(&self.events).terminated
        }

        fn termination(&self) -> Option<(i32, Option<i32>)> {
            lock(&self.events).termination
        }

        fn unresponsive_count(&self) -> u64 {
            lock(&self.events).unresponsive
        }

        fn render_view_ready_count(&self) -> u64 {
            lock(&self.events).render_view_ready
        }

        fn main_load_started_count(&self) -> u64 {
            lock(&self.events).main_load_started
        }

        fn event_error(&self) -> Option<String> {
            lock(&self.events).error.clone()
        }

        fn frame_error(&self) -> Option<String> {
            lock(&self.frames).error.clone()
        }

        fn bound(&self) -> Option<BrowserId> {
            lock(&self.events).bound
        }

        fn frame_total(&self) -> u64 {
            lock(&self.frames).total
        }

        fn valid_frame_count(&self) -> u64 {
            let state = lock(&self.frames);
            state
                .records
                .iter()
                .filter(|record| record.valid && Some(record.browser) == state.bound)
                .count() as u64
        }

        fn valid_frame_after(&self, baseline: u64) -> Option<u64> {
            let state = lock(&self.frames);
            state
                .records
                .iter()
                .filter(|record| {
                    record.valid
                        && record.sequence > baseline
                        && Some(record.browser) == state.bound
                })
                .map(|record| record.sequence)
                .min()
        }

        fn valid_frame_count_after(&self, baseline: u64) -> u64 {
            let state = lock(&self.frames);
            state
                .records
                .iter()
                .filter(|record| {
                    record.valid
                        && record.sequence > baseline
                        && Some(record.browser) == state.bound
                })
                .count() as u64
        }

        fn has_valid_frame(&self) -> bool {
            self.valid_frame_count() > 0
        }

        fn max_frame_sequence(&self) -> u64 {
            let state = lock(&self.frames);
            state
                .records
                .iter()
                .filter(|record| Some(record.browser) == state.bound)
                .map(|record| record.sequence)
                .max()
                .unwrap_or(0)
        }

        fn first_frame_dims(&self) -> (u32, u32) {
            let state = lock(&self.frames);
            (state.first_width, state.first_height)
        }

        fn apply_to(&self, report: &mut RendererProbeReport) {
            {
                let state = lock(&self.events);
                report.created = state.created;
                report.closed = state.closed;
                report.terminated_events = state.terminated;
                report.unresponsive_events = state.unresponsive;
                report.responsive_events = state.responsive;
                if let Some(error) = state.error.clone() {
                    report.error.get_or_insert(error);
                }
            }
            let state = lock(&self.frames);
            report.frames = state.total;
            report.first_frame_width = state.first_width;
            report.first_frame_height = state.first_height;
            let mut valid = BTreeSet::new();
            for record in &state.records {
                if record.valid {
                    valid.insert(record.browser);
                }
            }
            report.valid_frame_browsers = valid.len() as u64;
            if let Some(error) = state.error.clone() {
                report.error.get_or_insert(error);
            }
        }
    }

    impl BrowserEventSink for Observations {
        fn publish(&self, envelope: BrowserEventEnvelope) -> PublishResult {
            let mut state = lock(&self.events);
            match state.bound {
                Some(bound) if bound != envelope.browser => {
                    state.record_error("event BrowserId does not match the bound Browser");
                }
                None => {
                    state.candidates.insert(envelope.browser);
                }
                Some(_) => {}
            }
            if envelope.sequence <= state.last_sequence {
                state.record_error("event sequence is not strictly increasing");
            } else {
                state.last_sequence = envelope.sequence;
            }
            match &envelope.event {
                BrowserEvent::Created => {
                    state.created = state.created.saturating_add(1);
                    if state.created > 1 {
                        state.record_error("duplicate Created event");
                    }
                }
                BrowserEvent::Closed => {
                    state.closed = state.closed.saturating_add(1);
                    if state.closed > 1 {
                        state.record_error("duplicate Closed event");
                    }
                    if state.phase != "shutdown" {
                        state.record_error("Closed outside the shutdown phase");
                    }
                }
                BrowserEvent::RenderViewReady => {
                    state.render_view_ready = state.render_view_ready.saturating_add(1);
                }
                BrowserEvent::RendererTerminated {
                    native_status,
                    code,
                } => {
                    state.terminated = state.terminated.saturating_add(1);
                    if state.terminated > 1 {
                        state.record_error("duplicate RendererTerminated event");
                    }
                    if state.termination.is_none() {
                        state.termination = Some((*native_status, *code));
                    }
                }
                BrowserEvent::Unresponsive => {
                    state.unresponsive = state.unresponsive.saturating_add(1);
                }
                BrowserEvent::Responsive => {
                    state.responsive = state.responsive.saturating_add(1);
                }
                BrowserEvent::LoadStarted { is_main_frame } => {
                    if *is_main_frame {
                        state.main_load_started = state.main_load_started.saturating_add(1);
                    }
                }
                BrowserEvent::CreateFailed(_)
                | BrowserEvent::CaptureFailed(_)
                | BrowserEvent::PolicyDenied { .. }
                | BrowserEvent::CloseCancelled => {
                    state.record_error("unexpected browser failure or denial event");
                }
                BrowserEvent::AddressChanged(_)
                | BrowserEvent::TitleChanged(_)
                | BrowserEvent::NavigationRequested { .. }
                | BrowserEvent::LoadingChanged { .. }
                | BrowserEvent::LoadFinished { .. }
                | BrowserEvent::LoadCancelled { .. }
                | BrowserEvent::LoadFailed { .. }
                | BrowserEvent::BeforeUnloadRequested { .. }
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
                | BrowserEvent::AccessibilityTreeUpdated { .. } => {}
            }
            if state.created.saturating_add(state.closed) > 0 {
                // Lifecycle transitions are counted, never replayed from a log:
                // `created`/`closed` totals are validated by the checks.
            }
            PublishResult::Published
        }
    }

    impl FrameSink for Observations {
        fn publish(&self, frame: OwnedFrame) -> PublishResult {
            if frame.info.surface != SurfaceKind::View {
                return PublishResult::Rejected;
            }
            let mut state = lock(&self.frames);
            match state.bound {
                Some(bound) if bound != frame.info.browser => {
                    state.record_error("frame BrowserId does not match the bound Browser");
                    return PublishResult::Rejected;
                }
                None => {
                    state.candidates.insert(frame.info.browser);
                }
                Some(_) => {}
            }
            let width = frame.info.width;
            let height = frame.info.height;
            let Some(stride) = (width as usize)
                .checked_mul(height as usize)
                .and_then(|value| value.checked_mul(4))
            else {
                state.record_error("frame dimensions overflow");
                return PublishResult::Rejected;
            };
            let Some(pixels) = frame.cpu_pixels() else {
                state.record_error("frame payload is not cpu pixels");
                return PublishResult::Rejected;
            };
            if pixels.len() != stride {
                state.record_error("frame length does not match its dimensions");
                return PublishResult::Rejected;
            }
            let previous = state
                .last_sequence
                .get(&frame.info.browser)
                .copied()
                .unwrap_or(0);
            if frame.info.sequence <= previous {
                state.record_error("frame sequence is not strictly increasing");
                return PublishResult::Rejected;
            }
            state
                .last_sequence
                .insert(frame.info.browser, frame.info.sequence);
            let valid = width == FRAME_WIDTH_U32 && height == FRAME_HEIGHT_U32 && {
                let center = ((height as usize / 2) * width as usize + width as usize / 2) * 4;
                pixels.get(center..center + 4) == Some(&CENTER_BGRA)
            };
            if valid && state.first_width == 0 {
                state.first_width = width;
                state.first_height = height;
            }
            state.total = state.total.saturating_add(1);
            if state.records.len() < FRAME_CAPACITY {
                state.records.push(FrameRecord {
                    browser: frame.info.browser,
                    sequence: frame.info.sequence,
                    valid,
                });
            } else {
                state.record_error("frame record capacity exceeded");
            }
            PublishResult::Published
        }
    }

    impl UiDispatcher for Observations {
        fn wake(&self) -> Result<(), EngineError> {
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

    fn tap_errors(fixture: &NavigationFixture, observations: &Observations) -> Result<(), String> {
        if let Some(error) = observations.event_error() {
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
            tap_errors(fixture, observations)?;
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

    fn record_check(
        report: &mut RendererProbeReport,
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

    fn renderer_label(state: RendererState) -> &'static str {
        match state {
            RendererState::Starting => "Starting",
            RendererState::Running => "Running",
            RendererState::Unresponsive => "Unresponsive",
            RendererState::Terminated => "Terminated",
        }
    }

    fn scenario(
        runtime: &mut Runtime,
        options: &ProbeOptions,
        fixture: &NavigationFixture,
        observations: &Observations,
        report: &mut RendererProbeReport,
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
        // One Browser and one process for the whole scenario: no restart after
        // the renderer fault.
        let handle = runtime
            .create_browser(BrowserConfig {
                initial_url: path_a.clone(),
                profile: None,
                render_preference: options.backend,
                viewport: Viewport {
                    width: FRAME_WIDTH,
                    height: FRAME_HEIGHT,
                    scale: 1.0,
                },
                options: Default::default(),
            })
            .map_err(|error| format!("create browser: {error:?}"))?;
        let browser = handle.snapshot().browser;
        observations.bind(browser);

        wait_for(
            runtime,
            options.mode,
            phase_deadline(case_deadline),
            fixture,
            observations,
            "initial fixture page",
            || {
                let snapshot = handle.snapshot();
                Ok(snapshot.state == SessionState::Alive
                    && observations.created() == 1
                    && observations.has_valid_frame()
                    && snapshot.navigation.address == path_a
                    && snapshot.navigation.title == PAGE_TITLE
                    && !snapshot.navigation.is_loading
                    && snapshot.navigation.main_http_status == Some(200)
                    && matches!(
                        snapshot.renderer.state,
                        RendererState::Starting | RendererState::Running
                    )
                    && snapshot.renderer.last_termination.is_none())
            },
        )?;

        let initial = handle.snapshot();
        let (first_width, first_height) = observations.first_frame_dims();
        report.initial_renderer_state = Some(renderer_label(initial.renderer.state));
        record_check(
            report,
            "initial_state",
            initial.browser == browser
                && initial.state == SessionState::Alive
                && observations.bound() == Some(browser)
                && observations.created() == 1
                && observations.has_valid_frame()
                && first_width == FRAME_WIDTH_U32
                && first_height == FRAME_HEIGHT_U32
                && initial.navigation.address == path_a
                && initial.navigation.title == PAGE_TITLE
                && !initial.navigation.is_loading
                && initial.navigation.main_http_status == Some(200)
                && initial.renderer.last_termination.is_none()
                && !matches!(
                    initial.renderer.state,
                    RendererState::Unresponsive | RendererState::Terminated
                )
                && runtime.is_ready(),
        )?;

        // Existing observer handshake: publish readiness only after a real
        // first frame, then wait for the external launcher's release file.
        let gate = options
            .observer_gate
            .as_ref()
            .ok_or_else(|| "observer gate is required for renderer evidence".to_string())?;
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
        record_check(report, "helper_observer", gate.is_file())?;

        // Static page evidence: a short window without new frames must not be
        // reinterpreted as a hang. There is no frame-frequency watchdog.
        observations.begin_phase("static");
        let static_frames_before = observations.frame_total();
        let static_start = Instant::now();
        while static_start.elapsed() < STATIC_WINDOW {
            if Instant::now() >= case_deadline {
                return Err("static window: scenario budget exhausted".into());
            }
            tap_errors(fixture, observations)?;
            tick(runtime, options.mode)?;
        }
        report.static_window_frames = observations.frame_total() - static_frames_before;
        let after_static = handle.snapshot();
        report.static_quiet_observed = observations.unresponsive_count() == 0
            && after_static.state == SessionState::Alive
            && !matches!(
                after_static.renderer.state,
                RendererState::Unresponsive | RendererState::Terminated
            )
            && !after_static.navigation.is_loading
            && runtime.is_ready();
        record_check(report, "static_not_hung", report.static_quiet_observed)?;

        // Fixed Page.enable preflight. The successful post is not the ACK; the
        // actual method-result reply must be observed for this Browser.
        runtime
            .probe_renderer_enable(browser)
            .map_err(|error| format!("renderer enable post: {error:?}"))?;
        // The ACK is read outside any wait closure: the ACK lookup borrows the
        // Runtime, which the pump loop also needs mutably.
        let enable_deadline = phase_deadline(case_deadline);
        loop {
            tap_errors(fixture, observations)?;
            match runtime.probe_renderer_enabled(browser) {
                Some(true) => break,
                Some(false) => {
                    return Err("Page.enable was refused or the observer detached".into());
                }
                None => {}
            }
            if Instant::now() >= enable_deadline {
                return Err("renderer Page.enable: timeout".into());
            }
            tick(runtime, options.mode)?;
        }
        report.renderer_probe_enabled = runtime.probe_renderer_enabled(browser);
        record_check(
            report,
            "devtools_enabled",
            report.renderer_probe_enabled == Some(true),
        )?;

        let requests_before_crash = fixture.request_count(PATH_A).unwrap_or(u64::MAX);
        report.requests_before_crash = requests_before_crash;
        if requests_before_crash != 1 {
            return Err("initial fixture page request count was not one".into());
        }

        // Controlled Page.crash: a successful post is not a crash ACK and the
        // observer detach is not an exit/Closed proof. Only the real termination
        // callback for this Browser is evidence.
        observations.begin_phase("crash");
        runtime
            .probe_renderer_crash(browser)
            .map_err(|error| format!("renderer crash post: {error:?}"))?;
        let submit_deadline = phase_deadline(case_deadline);
        loop {
            tap_errors(fixture, observations)?;
            if runtime.probe_renderer_crash_submitted(browser) {
                break;
            }
            if Instant::now() >= submit_deadline {
                return Err("renderer crash submission: timeout".into());
            }
            tick(runtime, options.mode)?;
        }
        report.crash_command_submitted = runtime.probe_renderer_crash_submitted(browser);
        record_check(report, "crash_submitted", report.crash_command_submitted)?;

        let crash_deadline = {
            let requested = Instant::now() + CRASH_TIMEOUT;
            if requested < case_deadline {
                requested
            } else {
                case_deadline
            }
        };
        wait_for(
            runtime,
            options.mode,
            crash_deadline,
            fixture,
            observations,
            "renderer termination",
            || Ok(observations.terminated_count() >= 1),
        )?;
        let Some((native_status, code)) = observations.termination() else {
            return Err("renderer termination parameters are missing".into());
        };
        report.terminated_events = observations.terminated_count();
        report.terminated_native_status = Some(native_status);
        report.terminated_code = code;
        record_check(
            report,
            "terminated_event",
            report.terminated_events == 1 && report.terminated_code.is_some(),
        )?;

        let terminated_at = handle.snapshot();
        record_check(
            report,
            "browser_not_closed",
            terminated_at.browser == browser
                && terminated_at.state == SessionState::Alive
                && observations.closed() == 0
                && runtime.is_ready(),
        )?;

        let expected = RendererTermination {
            native_status,
            code,
        };
        record_check(
            report,
            "terminated_snapshot",
            terminated_at.state == SessionState::Alive
                && terminated_at.renderer.state == RendererState::Terminated
                && terminated_at.renderer.last_termination == Some(expected),
        )?;

        // No automatic reload: the same Browser stays Alive but terminated and
        // makes no new fixture request for a 500 ms quiescent window.
        let quiescence_start = Instant::now();
        while quiescence_start.elapsed() < QUIESCENCE {
            if Instant::now() >= case_deadline {
                return Err("quiescence: scenario budget exhausted".into());
            }
            tap_errors(fixture, observations)?;
            tick(runtime, options.mode)?;
        }
        let requests_after_quiescence = fixture.request_count(PATH_A).unwrap_or(u64::MAX);
        report.requests_after_quiescence = requests_after_quiescence;
        let quiet = handle.snapshot();
        report.no_automatic_reload_observed = requests_after_quiescence == requests_before_crash
            && observations.terminated_count() == 1
            && quiet.state == SessionState::Alive
            && quiet.renderer.state == RendererState::Terminated
            && quiet.renderer.last_termination == Some(expected);
        // Post-crash framebuffer baseline, captured after quiescence. A fresh
        // frame must exceed every sequence already delivered, not reuse one.
        report.pre_crash_frame_sequence = observations.max_frame_sequence();
        report.frames_before_crash = observations.valid_frame_count();
        record_check(
            report,
            "no_automatic_reload",
            report.no_automatic_reload_observed,
        )?;

        // Explicit recovery: command admission is its own check, never recovery
        // proof on its own.
        let load_started_before_reload = observations.main_load_started_count();
        let render_view_ready_before_reload = observations.render_view_ready_count();
        let command = handle
            .submit(BrowserCommand::Reload)
            .map_err(|error| format!("reload command: {error:?}"))?;
        report.reload_command_accepted = command.browser == browser && command.sequence != 0;
        record_check(
            report,
            "explicit_reload_admitted",
            report.reload_command_accepted,
        )?;

        wait_for(
            runtime,
            options.mode,
            phase_deadline(case_deadline),
            fixture,
            observations,
            "render view ready after reload",
            || {
                Ok(
                    observations.render_view_ready_count() > render_view_ready_before_reload
                        && handle.snapshot().renderer.state == RendererState::Running,
                )
            },
        )?;
        record_check(
            report,
            "render_view_ready",
            observations.render_view_ready_count() > render_view_ready_before_reload
                && handle.snapshot().renderer.state == RendererState::Running,
        )?;

        wait_for(
            runtime,
            options.mode,
            phase_deadline(case_deadline),
            fixture,
            observations,
            "recovered fixture navigation",
            || {
                let snapshot = handle.snapshot();
                Ok(
                    fixture.request_count(PATH_A) == requests_before_crash.checked_add(1)
                        && observations.main_load_started_count() > load_started_before_reload
                        && snapshot.state == SessionState::Alive
                        && snapshot.navigation.address == path_a
                        && snapshot.navigation.title == PAGE_TITLE
                        && !snapshot.navigation.is_loading
                        && snapshot.navigation.main_http_status == Some(200),
                )
            },
        )?;
        report.requests_after_recovery = fixture.request_count(PATH_A).unwrap_or(u64::MAX);
        report.fresh_request_after_recovery = requests_before_crash.checked_add(1)
            == Some(report.requests_after_recovery)
            && observations.main_load_started_count() > load_started_before_reload;
        record_check(
            report,
            "fresh_request_after_recovery",
            report.fresh_request_after_recovery,
        )?;

        wait_for(
            runtime,
            options.mode,
            phase_deadline(case_deadline),
            fixture,
            observations,
            "recovered fixture frame",
            || {
                Ok(observations
                    .valid_frame_after(report.pre_crash_frame_sequence)
                    .is_some())
            },
        )?;
        let fresh = observations.valid_frame_after(report.pre_crash_frame_sequence);
        report.post_recovery_frame_sequence = fresh.unwrap_or(0);
        report.frames_after_recovery =
            observations.valid_frame_count_after(report.pre_crash_frame_sequence);
        report.fresh_frame_after_recovery = fresh.is_some()
            && report.frames_after_recovery > 0
            && report.post_recovery_frame_sequence > report.pre_crash_frame_sequence;
        record_check(
            report,
            "fresh_frame_after_recovery",
            report.fresh_frame_after_recovery,
        )?;

        let recovered = handle.snapshot();
        report.same_browser_recovered = recovered.browser == browser
            && recovered.state == SessionState::Alive
            && recovered.renderer.state == RendererState::Running
            && recovered.renderer.last_termination == Some(expected)
            && observations.bound() == Some(browser)
            && report.fresh_frame_after_recovery
            && report.fresh_request_after_recovery
            && observations.render_view_ready_count() > render_view_ready_before_reload
            && fixture.request_count(PATH_A) == Some(report.requests_after_recovery);
        record_check(
            report,
            "same_browser_recovered",
            report.same_browser_recovered,
        )?;

        // Normal Force close waits for the real OnBeforeClose path, not just
        // ticket state.
        observations.begin_phase("shutdown");
        let ticket = handle.request_close(CloseMode::Force);
        let close_deadline = phase_deadline(case_deadline);
        loop {
            tap_errors(fixture, observations)?;
            let snapshot = handle.snapshot();
            if ticket.result() == CloseResult::Closed
                && snapshot.state == SessionState::Closed
                && observations.closed() == 1
            {
                record_check(
                    report,
                    "normal_close",
                    snapshot.browser == browser
                        && ticket.result() == CloseResult::Closed
                        && snapshot.state == SessionState::Closed
                        && observations.closed() == 1,
                )?;
                break;
            }
            if Instant::now() >= close_deadline {
                return Err("normal close: timeout".into());
            }
            tick(runtime, options.mode)?;
        }

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

    fn native(options: &ProbeOptions, report: &mut RendererProbeReport) -> i32 {
        if options.scenario != ProbeScenario::Smoke {
            report.error = Some("renderer probe implements the Smoke scenario only".into());
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

        // On any failure still release the fixture and attempt the normal Force
        // drain; an abnormal process exit would not be evidence.
        fixture.release_slow();
        observations.begin_phase("shutdown");
        let finished = drain(&mut runtime, options.mode);
        let fixture_errors = fixture.finish();
        if let Some(first) = fixture_errors.into_iter().next() {
            report
                .error
                .get_or_insert_with(|| format!("fixture protocol error: {first}"));
        }
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
        report.renderer_references_retained = runtime.probe_renderer_references() as u64;
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

        let native_shutdown = finished
            && diagnostics.cef_shutdown_calls == 1
            && !diagnostics.app_reference_retained
            && !diagnostics.root_lease_held
            && diagnostics.tasks.pending == 0
            && diagnostics.pending_requests == 0
            && diagnostics.pending_profiles == 0
            && diagnostics.pending_cookie_flushes == 0
            && diagnostics.profile_references_retained == 0
            && diagnostics.native_calls > 0
            && diagnostics.native_calls_after_shutdown == 0
            && report.renderer_references_retained == 0
            && report.created == 1
            && report.closed == 1;
        if let Err(error) = record_check(report, "native_shutdown", native_shutdown) {
            report.error.get_or_insert(error);
        }

        let threads = report.main_thread_id != 0
            && report.init_thread_id == Some(report.main_thread_id)
            && report.shutdown_thread_id == Some(report.main_thread_id)
            && report.cef_ui_thread_id.is_some_and(|id| {
                id != 0
                    && match options.mode {
                        ThreadMode::Integrated => {
                            id == report.main_thread_id && report.manual_pump_count > 0
                        }
                        ThreadMode::Threaded => {
                            id != report.main_thread_id && report.manual_pump_count == 0
                        }
                    }
            });
        if let Err(error) = record_check(report, "threads", threads) {
            report.error.get_or_insert(error);
        }

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

    fn finalize(report: &mut RendererProbeReport, candidate: i32) -> i32 {
        report.exit_code = Some(candidate);
        if candidate != 0 {
            report.success = false;
            if report.error.is_none() {
                report.error = Some(format!("renderer probe exited with {candidate}"));
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
                    .get_or_insert_with(|| format!("incomplete renderer evidence: {error}"));
                report.exit_code = Some(1);
                1
            }
        }
    }

    pub fn run() -> i32 {
        let options = match ProbeOptions::parse(std::env::args().skip(1)) {
            Ok(options) => options,
            Err(error) => {
                eprintln!("{error}");
                return 2;
            }
        };
        let mut report = RendererProbeReport::started(&options, os_thread_id());
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

        fn started_report() -> RendererProbeReport {
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
                    .join("renderer report.json")
                    .to_string_lossy()
                    .into_owned(),
                "--scenario".into(),
                "Smoke".into(),
            ])
            .unwrap();
            let mut report = RendererProbeReport::started(&options, 1);
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
            report.frames = 3;
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
            report.initial_renderer_state = Some("Running");
            report.renderer_probe_enabled = Some(true);
            report.crash_command_submitted = true;
            report.terminated_events = 1;
            // Arbitrary actual values: the probe never presets a status/code.
            report.terminated_native_status = Some(7);
            report.terminated_code = Some(-9);
            report.static_window_frames = 0;
            report.requests_before_crash = 1;
            report.requests_after_quiescence = 1;
            report.requests_after_recovery = 2;
            report.frames_before_crash = 2;
            report.frames_after_recovery = 1;
            report.pre_crash_frame_sequence = 2;
            report.post_recovery_frame_sequence = 3;
            report.static_quiet_observed = true;
            report.no_automatic_reload_observed = true;
            report.reload_command_accepted = true;
            report.fresh_request_after_recovery = true;
            report.fresh_frame_after_recovery = true;
            report.same_browser_recovered = true;
            report.renderer_references_retained = 0;
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
        fn report_requires_the_exact_ordered_check_set_and_actual_evidence() {
            assert!(started_report().validate().is_ok());

            let mut missing = started_report();
            missing.checks.pop();
            assert!(missing.validate().is_err());

            let mut extra = started_report();
            extra.checks.push(Check {
                id: "unexpected",
                passed: true,
            });
            assert!(extra.validate().is_err());

            let mut reordered = started_report();
            reordered.checks.swap(4, 5);
            assert!(reordered.validate().is_err());

            let mut failed = started_report();
            failed.checks[9].passed = false;
            assert!(failed.validate().is_err());

            // No automatic reload is a count evidence, not a narrative.
            let mut reloaded = started_report();
            reloaded.requests_after_quiescence = 2;
            assert!(reloaded.validate().is_err());

            // A recovered frame may not reuse the pre-crash baseline.
            let mut stale_frame = started_report();
            stale_frame.post_recovery_frame_sequence = stale_frame.pre_crash_frame_sequence;
            assert!(stale_frame.validate().is_err());

            // The successful post alone is never the Page.enable ACK.
            let mut no_ack = started_report();
            no_ack.renderer_probe_enabled = None;
            assert!(no_ack.validate().is_err());

            // Retained renderer Registrations must be zero.
            let mut retained = started_report();
            retained.renderer_references_retained = 1;
            assert!(retained.validate().is_err());
        }
    }
}

fn main() {
    #[cfg(all(
        target_os = "windows",
        target_arch = "x86_64",
        feature = "test-support"
    ))]
    std::process::exit(windows::run());
    #[cfg(not(all(
        target_os = "windows",
        target_arch = "x86_64",
        feature = "test-support"
    )))]
    {
        eprintln!("native renderer probe requires Windows x64 MSVC with test-support");
        std::process::exit(2);
    }
}
