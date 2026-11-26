//! Isolated native profile probe. Never used as the CEF helper executable.
//!
//! The probe proves profile sharing/isolation with a real same-origin HTTP
//! fixture and real Browsers. A storage row is only recorded after the page's
//! own same-origin POST arrived; frame delivery or command acceptance alone is
//! never treated as a successful read.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod windows {
    use rust_cef_core::{
        contracts::{
            BrowserConfig, ProfileConfig, ProfileHandle, ProfileStorage, RenderPreference,
            RuntimeConfig, RuntimeFiles, SessionState, ThreadMode, TrustedContentPolicy, Viewport,
        },
        profiles::ProfileState,
    };
    use rust_cef_runtime::{
        ports::{
            BrowserCommand, BrowserEvent, BrowserEventEnvelope, BrowserEventSink, CloseMode,
            EngineError, FrameSink, OwnedFrame, PublishResult, SurfaceKind, UiDispatcher,
        },
        profile_fixture::{Observation, ProfileFixture},
        runtime::{ProfileComparison, Runtime, RuntimeServices, ShutdownStatus, StartError},
        test_support::{ProbeOptions, ProbeScenario, publish_observer_ready},
    };
    use serde::Serialize;
    use std::{
        collections::HashSet,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU32, AtomicU64, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    const FRAME_WIDTH: i32 = 320;
    const FRAME_HEIGHT: i32 = 240;
    const FRAME_WIDTH_U32: u32 = 320;
    const FRAME_HEIGHT_U32: u32 = 240;
    const CENTER_BGRA: [u8; 4] = [126, 76, 26, 255];

    const VALUE_D: &str = "D";
    const VALUE_P1: &str = "P1";
    const VALUE_P2: &str = "P2";

    const PHASE_TIMEOUT: Duration = Duration::from_secs(20);
    const OBSERVER_TIMEOUT: Duration = Duration::from_secs(60);
    const SCENARIO_BUDGET: Duration = Duration::from_secs(90);
    const SHUTDOWN_BUDGET: Duration = Duration::from_secs(20);

    // Fixed evidence set. A missing, duplicated or unknown row makes the
    // strict validator fail even when the candidate process exited zero.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum ProfileSlot {
        Default,
        P1,
        P2,
    }

    const REQUIRED_STORAGE: [(&str, ProfileSlot); 16] = [
        ("default-a-write-d", ProfileSlot::Default),
        ("default-b-read-d", ProfileSlot::Default),
        ("p1a-initial-none", ProfileSlot::P1),
        ("p1b-initial-none", ProfileSlot::P1),
        ("p2a-initial-none", ProfileSlot::P2),
        ("p2b-initial-none", ProfileSlot::P2),
        ("p1a-write-p1", ProfileSlot::P1),
        ("p1b-read-p1", ProfileSlot::P1),
        ("default-read-d-after-p1", ProfileSlot::Default),
        ("p2a-read-none-after-p1", ProfileSlot::P2),
        ("p2a-write-p2", ProfileSlot::P2),
        ("p2b-read-p2", ProfileSlot::P2),
        ("p1b-read-p1-after-p2", ProfileSlot::P1),
        ("default-read-d-after-p2", ProfileSlot::Default),
        ("p1b-read-p1-after-close", ProfileSlot::P1),
        ("p1c-reuse-read-p1", ProfileSlot::P1),
    ];

    const REQUIRED_IDENTITY: [(&str, &str, bool, bool); 5] = [
        ("default", "default-explicit", true, true),
        ("P1", "P1", true, true),
        ("default", "P1", false, false),
        ("P1", "P2", false, false),
        ("default", "P2", false, false),
    ];

    #[derive(Clone, Serialize)]
    struct StorageCheck {
        token: String,
        profile_id: u64,
        cookie_matches: bool,
        local_storage_matches: bool,
        passed: bool,
    }

    #[derive(Clone, Serialize)]
    struct IdentityCheck {
        left: String,
        right: String,
        same: bool,
        sharing: bool,
        passed: bool,
    }

    #[derive(Serialize)]
    struct ProfileProbeReport {
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
        profile_references_retained: u64,
        native_calls: u64,
        native_calls_after_shutdown: u64,
        cef_shutdown_calls: u64,
        app_reference_retained: Option<bool>,
        /// Raw diagnostic: `Some(false)` means the real CEF command line was
        /// observed and `--single-process` was absent.
        single_process_enabled: Option<bool>,
        /// Strict probe view derived from the raw observation above.
        single_process_disabled: Option<bool>,
        runtime_generation: Option<u64>,
        default_profile_id: Option<u64>,
        p1_profile_id: Option<u64>,
        p2_profile_id: Option<u64>,
        default_profile_ready: bool,
        same_handle_reused: bool,
        other_browser_survived: bool,
        observer_released: bool,
        storage_checks: Vec<StorageCheck>,
        identity_checks: Vec<IdentityCheck>,
        error: Option<String>,
        exit_code: Option<i32>,
    }

    impl ProfileProbeReport {
        fn started(options: &ProbeOptions, main_thread_id: u64) -> Self {
            Self {
                schema: 1,
                kind: "profiles",
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
                profile_references_retained: 0,
                native_calls: 0,
                native_calls_after_shutdown: 0,
                cef_shutdown_calls: 0,
                app_reference_retained: None,
                single_process_enabled: None,
                single_process_disabled: None,
                runtime_generation: None,
                default_profile_id: None,
                p1_profile_id: None,
                p2_profile_id: None,
                default_profile_ready: false,
                same_handle_reused: false,
                other_browser_survived: false,
                observer_released: false,
                storage_checks: Vec::new(),
                identity_checks: Vec::new(),
                error: None,
                exit_code: None,
            }
        }

        fn profile_id(&self, slot: ProfileSlot) -> Option<u64> {
            match slot {
                ProfileSlot::Default => self.default_profile_id,
                ProfileSlot::P1 => self.p1_profile_id,
                ProfileSlot::P2 => self.p2_profile_id,
            }
        }

        /// Strict evidence check. `success` is deliberately not consulted here:
        /// it may only be set after this function accepted the evidence and
        /// after the candidate already exited zero.
        fn validate(&self) -> Result<(), String> {
            if self.schema != 1 {
                return Err(format!("schema must be 1, got {}", self.schema));
            }
            if self.kind != "profiles" {
                return Err(format!("kind must be profiles, got {}", self.kind));
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
            if !matches!(self.requested_backend, "Software" | "D3d11")
                || self.actual_backend != Some(self.requested_backend)
            {
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
            if self.created < 6 {
                return Err(format!("created must be at least 6, got {}", self.created));
            }
            if self.closed != self.created {
                return Err(format!(
                    "closed {} != created {}",
                    self.closed, self.created
                ));
            }
            if self.valid_frame_browsers != self.created {
                return Err(format!(
                    "valid frame browsers {} != created {}",
                    self.valid_frame_browsers, self.created
                ));
            }
            if self.frames == 0 {
                return Err("no frames were delivered".into());
            }
            if self.first_frame_width != FRAME_WIDTH_U32
                || self.first_frame_height != FRAME_HEIGHT_U32
            {
                return Err(format!(
                    "first frame must be {FRAME_WIDTH_U32}x{FRAME_HEIGHT_U32}, got {}x{}",
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
            if self.native_calls_after_shutdown != 0 {
                return Err("native calls occurred after shutdown".into());
            }
            if self.pending_requests != 0 {
                return Err("pending requests must be zero".into());
            }
            if self.pending_profiles != 0 {
                return Err("pending profiles must be zero".into());
            }
            if self.profile_references_retained != 0 {
                return Err("profile references must be released".into());
            }
            if self.app_reference_retained != Some(false) {
                return Err("App reference must be released".into());
            }
            if self.single_process_enabled != Some(false) {
                return Err(format!(
                    "single-process must be observed disabled, got {:?}",
                    self.single_process_enabled
                ));
            }
            if self.single_process_disabled != Some(true) {
                return Err("single_process_disabled must be Some(true)".into());
            }
            let generation = self
                .runtime_generation
                .ok_or_else(|| "runtime generation is missing".to_string())?;
            if generation == 0 {
                return Err("runtime generation must be nonzero".into());
            }
            if self.default_profile_id != Some(0) {
                return Err("default profile id must be 0".into());
            }
            let p1 = self
                .p1_profile_id
                .ok_or_else(|| "P1 profile id is missing".to_string())?;
            let p2 = self
                .p2_profile_id
                .ok_or_else(|| "P2 profile id is missing".to_string())?;
            if p1 == 0 || p2 == 0 {
                return Err("explicit profile ids must be nonzero".into());
            }
            if p1 == p2 {
                return Err("explicit profiles must have distinct ids".into());
            }
            if !self.default_profile_ready {
                return Err(
                    "default profile was not Ready from its required initialization source".into(),
                );
            }
            if !self.same_handle_reused {
                return Err("same profile handle was not reused".into());
            }
            if !self.other_browser_survived {
                return Err("the other same-profile browser did not survive".into());
            }
            if !self.observer_released {
                return Err("external observer release was not observed".into());
            }

            if self.storage_checks.len() != REQUIRED_STORAGE.len() {
                return Err(format!(
                    "storage checks must be {}, got {}",
                    REQUIRED_STORAGE.len(),
                    self.storage_checks.len()
                ));
            }
            for (token, slot) in REQUIRED_STORAGE {
                let expected_id = self
                    .profile_id(slot)
                    .ok_or_else(|| format!("profile id for {token} is missing"))?;
                let mut matches = self
                    .storage_checks
                    .iter()
                    .filter(|check| check.token.as_str() == token);
                let Some(check) = matches.next() else {
                    return Err(format!("missing storage check: {token}"));
                };
                if matches.next().is_some() {
                    return Err(format!("duplicate storage check: {token}"));
                }
                if check.profile_id != expected_id {
                    return Err(format!(
                        "storage check {token} used profile {} not {expected_id}",
                        check.profile_id
                    ));
                }
                if !check.cookie_matches || !check.local_storage_matches || !check.passed {
                    return Err(format!("storage check {token} did not match"));
                }
            }
            for check in &self.storage_checks {
                if !REQUIRED_STORAGE
                    .iter()
                    .any(|(token, _)| *token == check.token.as_str())
                {
                    return Err(format!("unknown storage check token: {}", check.token));
                }
            }

            if self.identity_checks.len() != REQUIRED_IDENTITY.len() {
                return Err(format!(
                    "identity checks must be {}, got {}",
                    REQUIRED_IDENTITY.len(),
                    self.identity_checks.len()
                ));
            }
            for (left, right, same, sharing) in REQUIRED_IDENTITY {
                let mut matches = self
                    .identity_checks
                    .iter()
                    .filter(|check| check.left.as_str() == left && check.right.as_str() == right);
                let Some(check) = matches.next() else {
                    return Err(format!("missing identity check: {left}/{right}"));
                };
                if matches.next().is_some() {
                    return Err(format!("duplicate identity check: {left}/{right}"));
                }
                if check.same != same || check.sharing != sharing || !check.passed {
                    return Err(format!(
                        "identity check {left}/{right} expected same={same} sharing={sharing}"
                    ));
                }
            }
            for check in &self.identity_checks {
                if !REQUIRED_IDENTITY.iter().any(|(left, right, _, _)| {
                    *left == check.left.as_str() && *right == check.right.as_str()
                }) {
                    return Err(format!(
                        "unknown identity check: {}/{}",
                        check.left, check.right
                    ));
                }
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct Observations {
        created: AtomicU64,
        closed: AtomicU64,
        frames: AtomicU64,
        width: AtomicU32,
        height: AtomicU32,
        valid_browsers: Mutex<HashSet<rust_cef_runtime::contracts::BrowserId>>,
        error: Mutex<Option<String>>,
    }

    impl Observations {
        fn record_error(&self, message: String) {
            let mut error = self.error.lock().unwrap();
            if error.is_none() {
                *error = Some(message);
            }
        }

        fn apply_to(&self, report: &mut ProfileProbeReport) {
            report.created = self.created.load(Ordering::Relaxed);
            report.closed = self.closed.load(Ordering::Relaxed);
            report.frames = self.frames.load(Ordering::Relaxed);
            report.first_frame_width = self.width.load(Ordering::Acquire);
            report.first_frame_height = self.height.load(Ordering::Acquire);
            report.valid_frame_browsers = self.valid_browsers.lock().unwrap().len() as u64;
            if let Some(error) = self.error.lock().unwrap().clone() {
                report.error.get_or_insert(error);
            }
        }
    }

    impl BrowserEventSink for Observations {
        fn publish(&self, envelope: BrowserEventEnvelope) -> PublishResult {
            // A valid first frame cannot erase a later failure; the first
            // failure is retained until the report is finalized.
            match &envelope.event {
                BrowserEvent::Created => {
                    self.created.fetch_add(1, Ordering::Relaxed);
                }
                BrowserEvent::Closed => {
                    self.closed.fetch_add(1, Ordering::Relaxed);
                }
                BrowserEvent::CreateFailed(error) | BrowserEvent::CaptureFailed(error) => {
                    self.record_error(format!(
                        "browser {} failure: {error:?}",
                        envelope.browser.local()
                    ));
                }
                BrowserEvent::LoadFailed {
                    is_main_frame: true,
                    code,
                } => {
                    self.record_error(format!(
                        "browser {} main frame load failed: {code}",
                        envelope.browser.local()
                    ));
                }
                BrowserEvent::RendererTerminated { code, .. } => {
                    self.record_error(format!(
                        "browser {} renderer terminated: {code:?}",
                        envelope.browser.local()
                    ));
                }
                _ => {}
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
            let width = frame.info.width;
            let height = frame.info.height;
            let center = ((height as usize / 2) * width as usize + width as usize / 2) * 4;
            if width == FRAME_WIDTH_U32
                && height == FRAME_HEIGHT_U32
                && frame.cpu_pixels().and_then(|px| px.get(center..center + 4))
                    == Some(&CENTER_BGRA)
            {
                self.width.store(width, Ordering::Release);
                self.height.store(height, Ordering::Release);
                self.valid_browsers
                    .lock()
                    .unwrap()
                    .insert(frame.info.browser);
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
                return Err("ContextReady timeout".into());
            }
            tick(runtime, mode)?;
        }
    }

    fn wait_profile_ready(
        runtime: &mut Runtime,
        handle: ProfileHandle,
        mode: ThreadMode,
        deadline: Instant,
        label: &str,
    ) -> Result<(), String> {
        loop {
            match runtime
                .profile_state(handle)
                .map_err(|error| format!("profile state {label}: {error}"))?
            {
                ProfileState::Ready => return Ok(()),
                ProfileState::Pending => {}
                ProfileState::Failed => return Err(format!("profile {label} failed")),
            }
            if Instant::now() >= deadline {
                return Err(format!("profile {label} readiness timeout"));
            }
            tick(runtime, mode)?;
        }
    }

    fn fixture_protocol_error(errors: Vec<String>) -> Result<(), String> {
        if errors.is_empty() {
            Ok(())
        } else {
            Err(format!("fixture protocol error: {}", errors.join("; ")))
        }
    }

    fn next_observation(
        fixture: &ProfileFixture,
        token: &str,
    ) -> Result<Option<Observation>, String> {
        fixture_protocol_error(fixture.errors())?;
        let observation = fixture.take_result(token);
        fixture_protocol_error(fixture.errors())?;
        Ok(observation)
    }

    fn retain_fixture_failure(report: &mut ProfileProbeReport, errors: Vec<String>) {
        if let Err(error) = fixture_protocol_error(errors) {
            report.error.get_or_insert(error);
        }
    }

    fn wait_observation(
        fixture: &ProfileFixture,
        runtime: &mut Runtime,
        mode: ThreadMode,
        token: &str,
        case_deadline: Instant,
    ) -> Result<Observation, String> {
        let deadline = phase_deadline(case_deadline);
        loop {
            if let Some(observation) = next_observation(fixture, token)? {
                return Ok(observation);
            }
            if Instant::now() >= deadline {
                return Err(format!("page result timeout: {token}"));
            }
            tick(runtime, mode)?;
        }
    }

    #[derive(Clone, Copy)]
    enum Expected {
        Value(&'static str),
        None,
    }

    fn record_check(
        fixture: &ProfileFixture,
        runtime: &mut Runtime,
        mode: ThreadMode,
        report: &mut ProfileProbeReport,
        token: &'static str,
        profile_id: u64,
        expected: Expected,
        case_deadline: Instant,
    ) -> Result<(), String> {
        let observation = wait_observation(fixture, runtime, mode, token, case_deadline)?;
        let (cookie_matches, local_storage_matches) = match expected {
            Expected::Value(value) => (
                observation.cookie.as_deref() == Some(value),
                observation.local_storage.as_deref() == Some(value),
            ),
            Expected::None => (
                observation.cookie.is_none(),
                observation.local_storage.is_none(),
            ),
        };
        let passed = cookie_matches && local_storage_matches;
        report.storage_checks.push(StorageCheck {
            token: token.to_string(),
            profile_id,
            cookie_matches,
            local_storage_matches,
            passed,
        });
        if !passed {
            return Err(format!("storage observation did not match: {token}"));
        }
        Ok(())
    }

    fn create_browser(
        runtime: &mut Runtime,
        label: &str,
        profile: Option<ProfileHandle>,
        url: String,
        backend: RenderPreference,
    ) -> Result<rust_cef_runtime::ports::BrowserHandle, String> {
        runtime
            .create_browser(BrowserConfig {
                initial_url: url,
                profile,
                render_preference: backend,
                viewport: Viewport {
                    width: FRAME_WIDTH,
                    height: FRAME_HEIGHT,
                    scale: 1.0,
                },
                options: Default::default(),
            })
            .map_err(|error| format!("create browser {label}: {error:?}"))
    }

    fn navigate(
        handle: &rust_cef_runtime::ports::BrowserHandle,
        label: &str,
        url: String,
    ) -> Result<(), String> {
        handle
            .submit(BrowserCommand::Navigate(url))
            .map(|_| ())
            .map_err(|error| format!("navigate {label}: {error:?}"))
    }

    fn ensure_alive(
        handle: &rust_cef_runtime::ports::BrowserHandle,
        label: &str,
    ) -> Result<(), String> {
        if handle.snapshot().state == SessionState::Alive {
            Ok(())
        } else {
            Err(format!(
                "browser {label} is not Alive: {:?}",
                handle.snapshot().state
            ))
        }
    }

    fn wait_closed(
        handle: &rust_cef_runtime::ports::BrowserHandle,
        runtime: &mut Runtime,
        mode: ThreadMode,
        case_deadline: Instant,
        label: &str,
    ) -> Result<(), String> {
        let deadline = phase_deadline(case_deadline);
        loop {
            match handle.snapshot().state {
                SessionState::Closed => return Ok(()),
                SessionState::CreateFailed => {
                    return Err(format!("browser {label} failed to be created"));
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(format!("browser {label} close timeout"));
            }
            tick(runtime, mode)?;
        }
    }

    fn observation_error(observations: &Observations) -> Option<String> {
        observations.error.lock().unwrap().clone()
    }

    fn wait_browsers_alive_with_frames(
        handles: &[rust_cef_runtime::ports::BrowserHandle],
        observations: &Observations,
        runtime: &mut Runtime,
        mode: ThreadMode,
        case_deadline: Instant,
        label: &str,
    ) -> Result<(), String> {
        let deadline = phase_deadline(case_deadline);
        loop {
            let all_alive = handles
                .iter()
                .all(|handle| handle.snapshot().state == SessionState::Alive);
            let frames = {
                let valid = observations.valid_browsers.lock().unwrap();
                handles
                    .iter()
                    .all(|handle| valid.contains(&handle.snapshot().browser))
            };
            if all_alive
                && frames
                && observations.created.load(Ordering::Acquire) >= handles.len() as u64
            {
                return Ok(());
            }
            if let Some(error) = observation_error(observations) {
                return Err(error);
            }
            if Instant::now() >= deadline {
                return Err(format!("{label}: alive/frame timeout"));
            }
            tick(runtime, mode)?;
        }
    }

    fn wait_browser_frame(
        handle: &rust_cef_runtime::ports::BrowserHandle,
        observations: &Observations,
        runtime: &mut Runtime,
        mode: ThreadMode,
        case_deadline: Instant,
        label: &str,
    ) -> Result<(), String> {
        let deadline = phase_deadline(case_deadline);
        loop {
            let alive = handle.snapshot().state == SessionState::Alive;
            let frame = observations
                .valid_browsers
                .lock()
                .unwrap()
                .contains(&handle.snapshot().browser);
            if alive && frame {
                return Ok(());
            }
            if let Some(error) = observation_error(observations) {
                return Err(error);
            }
            if Instant::now() >= deadline {
                return Err(format!("{label}: alive/frame timeout"));
            }
            tick(runtime, mode)?;
        }
    }

    fn compare_profiles(
        runtime: &mut Runtime,
        mode: ThreadMode,
        left: ProfileHandle,
        right: ProfileHandle,
        case_deadline: Instant,
        label: &str,
    ) -> Result<ProfileComparison, String> {
        let deadline = phase_deadline(case_deadline);
        let ticket = runtime
            .compare_profiles(left, right)
            .map_err(|error| format!("compare {label}: {error}"))?;
        loop {
            match ticket.poll() {
                None => {}
                Some(Ok(comparison)) => return Ok(comparison),
                Some(Err(error)) => {
                    return Err(format!("compare {label}: {error}"));
                }
            }
            if Instant::now() >= deadline {
                return Err(format!("compare {label}: timeout"));
            }
            tick(runtime, mode)?;
        }
    }

    fn identity_check(
        runtime: &mut Runtime,
        mode: ThreadMode,
        report: &mut ProfileProbeReport,
        case_deadline: Instant,
        left_label: &'static str,
        right_label: &'static str,
        left: ProfileHandle,
        right: ProfileHandle,
        expected_same: bool,
        expected_sharing: bool,
    ) -> Result<(), String> {
        let comparison = compare_profiles(runtime, mode, left, right, case_deadline, left_label)?;
        let passed =
            comparison.is_same == expected_same && comparison.is_sharing == expected_sharing;
        report.identity_checks.push(IdentityCheck {
            left: left_label.to_string(),
            right: right_label.to_string(),
            same: comparison.is_same,
            sharing: comparison.is_sharing,
            passed,
        });
        if !passed {
            return Err(format!(
                "identity check {left_label}/{right_label} did not match"
            ));
        }
        Ok(())
    }

    fn scenario(
        runtime: &mut Runtime,
        options: &ProbeOptions,
        fixture: &ProfileFixture,
        observations: &Observations,
        report: &mut ProfileProbeReport,
        case_deadline: Instant,
    ) -> Result<(), String> {
        wait_runtime_ready(runtime, options.mode, phase_deadline(case_deadline))?;

        let default = runtime.default_profile_handle();
        report.runtime_generation = Some(default.runtime().value());
        report.default_profile_id = Some(default.local());
        report.default_profile_ready = runtime
            .profile_state(default)
            .map_err(|error| error.to_string())?
            == ProfileState::Ready;
        if !report.default_profile_ready {
            return Err("default profile is not Ready after ContextReady".into());
        }

        let mut browsers = Vec::new();

        // Default A uses the explicit default handle; B omits the profile. Both
        // must bind the very same selected default context (Memory is incognito).
        let token = "default-a-write-d";
        let url = fixture
            .url(token, "write", VALUE_D)
            .map_err(|error| format!("fixture url {token}: {error}"))?;
        let default_a = create_browser(runtime, "default-A", Some(default), url, options.backend)?;
        record_check(
            fixture,
            runtime,
            options.mode,
            report,
            token,
            default.local(),
            Expected::Value(VALUE_D),
            case_deadline,
        )?;
        browsers.push(default_a);

        let token = "default-b-read-d";
        let url = fixture
            .url(token, "read", "")
            .map_err(|error| format!("fixture url {token}: {error}"))?;
        let default_b = create_browser(runtime, "default-B", None, url, options.backend)?;
        record_check(
            fixture,
            runtime,
            options.mode,
            report,
            token,
            default.local(),
            Expected::Value(VALUE_D),
            case_deadline,
        )?;
        browsers.push(default_b.clone());

        let p1_config = ProfileConfig {
            name: "P1".into(),
            storage: ProfileStorage::Memory,
        };
        let first_p1 = runtime
            .create_profile(p1_config.clone())
            .map_err(|error| format!("create P1: {error}"))?;
        let p1 = first_p1.handle;
        let second_p1 = runtime
            .create_profile(p1_config.clone())
            .map_err(|error| format!("reuse P1: {error}"))?;
        report.p1_profile_id = Some(p1.local());
        report.same_handle_reused = second_p1.handle == p1 && !second_p1.is_new;
        if !report.same_handle_reused {
            return Err("same profile configuration did not reuse its handle".into());
        }

        let p2_config = ProfileConfig {
            name: "P2".into(),
            storage: ProfileStorage::Memory,
        };
        let p2 = runtime
            .create_profile(p2_config)
            .map_err(|error| format!("create P2: {error}"))?
            .handle;
        report.p2_profile_id = Some(p2.local());

        wait_profile_ready(
            runtime,
            p1,
            options.mode,
            phase_deadline(case_deadline),
            "P1",
        )?;
        wait_profile_ready(
            runtime,
            p2,
            options.mode,
            phase_deadline(case_deadline),
            "P2",
        )?;

        // Initial same-origin reads of the explicit profiles must observe
        // nothing. Register each URL immediately before its own navigation and
        // never reuse a served read token.
        let p1a = {
            let token = "p1a-initial-none";
            let url = fixture
                .url(token, "read", "")
                .map_err(|error| format!("fixture url {token}: {error}"))?;
            let browser = create_browser(runtime, "P1-A", Some(p1), url, options.backend)?;
            record_check(
                fixture,
                runtime,
                options.mode,
                report,
                token,
                p1.local(),
                Expected::None,
                case_deadline,
            )?;
            browser
        };
        browsers.push(p1a.clone());

        let p1b = {
            let token = "p1b-initial-none";
            let url = fixture
                .url(token, "read", "")
                .map_err(|error| format!("fixture url {token}: {error}"))?;
            let browser = create_browser(runtime, "P1-B", Some(p1), url, options.backend)?;
            record_check(
                fixture,
                runtime,
                options.mode,
                report,
                token,
                p1.local(),
                Expected::None,
                case_deadline,
            )?;
            browser
        };
        browsers.push(p1b.clone());

        let p2a = {
            let token = "p2a-initial-none";
            let url = fixture
                .url(token, "read", "")
                .map_err(|error| format!("fixture url {token}: {error}"))?;
            let browser = create_browser(runtime, "P2-A", Some(p2), url, options.backend)?;
            record_check(
                fixture,
                runtime,
                options.mode,
                report,
                token,
                p2.local(),
                Expected::None,
                case_deadline,
            )?;
            browser
        };
        browsers.push(p2a.clone());

        let p2b = {
            let token = "p2b-initial-none";
            let url = fixture
                .url(token, "read", "")
                .map_err(|error| format!("fixture url {token}: {error}"))?;
            let browser = create_browser(runtime, "P2-B", Some(p2), url, options.backend)?;
            record_check(
                fixture,
                runtime,
                options.mode,
                report,
                token,
                p2.local(),
                Expected::None,
                case_deadline,
            )?;
            browser
        };
        browsers.push(p2b.clone());

        // The external observer may only attest a real live Browser tree.
        wait_browsers_alive_with_frames(
            &browsers,
            observations,
            runtime,
            options.mode,
            case_deadline,
            "six profiles browsers",
        )?;

        let gate = options
            .observer_gate
            .as_ref()
            .ok_or_else(|| "observer gate is required for profile evidence".to_string())?;
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
            tick(runtime, options.mode)?;
        }
        report.observer_released = true;

        // Real CEF is_same/is_sharing_with comparisons, delivered as owned
        // booleans by the UI sequence. No ID equality is substituted.
        identity_check(
            runtime,
            options.mode,
            report,
            case_deadline,
            "default",
            "default-explicit",
            default,
            default,
            true,
            true,
        )?;
        identity_check(
            runtime,
            options.mode,
            report,
            case_deadline,
            "P1",
            "P1",
            p1,
            p1,
            true,
            true,
        )?;
        identity_check(
            runtime,
            options.mode,
            report,
            case_deadline,
            "default",
            "P1",
            default,
            p1,
            false,
            false,
        )?;
        identity_check(
            runtime,
            options.mode,
            report,
            case_deadline,
            "P1",
            "P2",
            p1,
            p2,
            false,
            false,
        )?;
        identity_check(
            runtime,
            options.mode,
            report,
            case_deadline,
            "default",
            "P2",
            default,
            p2,
            false,
            false,
        )?;

        // P1-A writes; P1-B must read the same value while default keeps D and
        // P2 still observes nothing.
        let token = "p1a-write-p1";
        let url = fixture
            .url(token, "write", VALUE_P1)
            .map_err(|error| format!("fixture url {token}: {error}"))?;
        navigate(&p1a, "P1-A", url)?;
        record_check(
            fixture,
            runtime,
            options.mode,
            report,
            token,
            p1.local(),
            Expected::Value(VALUE_P1),
            case_deadline,
        )?;
        ensure_alive(&p1a, "P1-A")?;

        let token = "p1b-read-p1";
        let url = fixture
            .url(token, "read", "")
            .map_err(|error| format!("fixture url {token}: {error}"))?;
        navigate(&p1b, "P1-B", url)?;
        record_check(
            fixture,
            runtime,
            options.mode,
            report,
            token,
            p1.local(),
            Expected::Value(VALUE_P1),
            case_deadline,
        )?;
        ensure_alive(&p1b, "P1-B")?;

        let token = "default-read-d-after-p1";
        let url = fixture
            .url(token, "read", "")
            .map_err(|error| format!("fixture url {token}: {error}"))?;
        navigate(&default_b, "default-B", url)?;
        record_check(
            fixture,
            runtime,
            options.mode,
            report,
            token,
            default.local(),
            Expected::Value(VALUE_D),
            case_deadline,
        )?;
        ensure_alive(&default_b, "default-B")?;

        let token = "p2a-read-none-after-p1";
        let url = fixture
            .url(token, "read", "")
            .map_err(|error| format!("fixture url {token}: {error}"))?;
        navigate(&p2a, "P2-A", url)?;
        record_check(
            fixture,
            runtime,
            options.mode,
            report,
            token,
            p2.local(),
            Expected::None,
            case_deadline,
        )?;
        ensure_alive(&p2a, "P2-A")?;

        // P2 writes; P2-B reads P2, P1 and default keep their own values.
        let token = "p2a-write-p2";
        let url = fixture
            .url(token, "write", VALUE_P2)
            .map_err(|error| format!("fixture url {token}: {error}"))?;
        navigate(&p2a, "P2-A", url)?;
        record_check(
            fixture,
            runtime,
            options.mode,
            report,
            token,
            p2.local(),
            Expected::Value(VALUE_P2),
            case_deadline,
        )?;
        ensure_alive(&p2a, "P2-A")?;

        let token = "p2b-read-p2";
        let url = fixture
            .url(token, "read", "")
            .map_err(|error| format!("fixture url {token}: {error}"))?;
        navigate(&p2b, "P2-B", url)?;
        record_check(
            fixture,
            runtime,
            options.mode,
            report,
            token,
            p2.local(),
            Expected::Value(VALUE_P2),
            case_deadline,
        )?;
        ensure_alive(&p2b, "P2-B")?;

        let token = "p1b-read-p1-after-p2";
        let url = fixture
            .url(token, "read", "")
            .map_err(|error| format!("fixture url {token}: {error}"))?;
        navigate(&p1b, "P1-B", url)?;
        record_check(
            fixture,
            runtime,
            options.mode,
            report,
            token,
            p1.local(),
            Expected::Value(VALUE_P1),
            case_deadline,
        )?;
        ensure_alive(&p1b, "P1-B")?;

        let token = "default-read-d-after-p2";
        let url = fixture
            .url(token, "read", "")
            .map_err(|error| format!("fixture url {token}: {error}"))?;
        navigate(&default_b, "default-B", url)?;
        record_check(
            fixture,
            runtime,
            options.mode,
            report,
            token,
            default.local(),
            Expected::Value(VALUE_D),
            case_deadline,
        )?;
        ensure_alive(&default_b, "default-B")?;

        // Force-close one browser of P1. Its sibling must stay Alive and still
        // read the profile value.
        p1a.request_close(CloseMode::Force);
        wait_closed(&p1a, runtime, options.mode, case_deadline, "P1-A")?;
        if p1b.snapshot().state != SessionState::Alive {
            return Err("P1-B did not survive its sibling's Force close".into());
        }
        let token = "p1b-read-p1-after-close";
        let url = fixture
            .url(token, "read", "")
            .map_err(|error| format!("fixture url {token}: {error}"))?;
        navigate(&p1b, "P1-B", url)?;
        let read_after_close = record_check(
            fixture,
            runtime,
            options.mode,
            report,
            token,
            p1.local(),
            Expected::Value(VALUE_P1),
            case_deadline,
        );
        report.other_browser_survived =
            read_after_close.is_ok() && p1b.snapshot().state == SessionState::Alive;
        read_after_close?;

        // Close every remaining P1 browser, then reuse the same profile with a
        // seventh Browser. The context must not have been destroyed.
        p1b.request_close(CloseMode::Force);
        wait_closed(&p1b, runtime, options.mode, case_deadline, "P1-B")?;
        if runtime
            .profile_state(p1)
            .map_err(|error| format!("P1 state after close: {error}"))?
            != ProfileState::Ready
        {
            return Err("P1 profile is no longer Ready after its browsers closed".into());
        }
        let reuse = runtime
            .create_profile(p1_config)
            .map_err(|error| format!("reuse P1 after close: {error}"))?;
        if reuse.is_new || reuse.handle != p1 {
            return Err("P1 was recreated instead of reusing its context".into());
        }
        report.same_handle_reused = true;

        let token = "p1c-reuse-read-p1";
        let url = fixture
            .url(token, "read", "")
            .map_err(|error| format!("fixture url {token}: {error}"))?;
        let p1c = create_browser(runtime, "P1-C", Some(reuse.handle), url, options.backend)?;
        record_check(
            fixture,
            runtime,
            options.mode,
            report,
            token,
            p1.local(),
            Expected::Value(VALUE_P1),
            case_deadline,
        )?;
        wait_browser_frame(
            &p1c,
            observations,
            runtime,
            options.mode,
            case_deadline,
            "P1-C",
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

    fn native(options: &ProbeOptions, report: &mut ProfileProbeReport) -> i32 {
        if options.scenario != ProbeScenario::Smoke {
            report.error = Some("profile probe implements the Smoke scenario only".into());
            return 1;
        }
        let host = match std::env::current_exe() {
            Ok(host) => host,
            Err(error) => {
                report.error = Some(error.to_string());
                return 1;
            }
        };
        let mut fixture = match ProfileFixture::new() {
            Ok(fixture) => fixture,
            Err(error) => {
                report.error = Some(format!("profile fixture: {error}"));
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
            report.error = Some(error);
        }

        let finished = drain(&mut runtime, options.mode);
        // No valid earlier observation may hide a later protocol failure.
        // Join before the final snapshot so HTTP errors cannot race success.
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
        report.profile_references_retained = diagnostics.profile_references_retained;
        report.native_calls = diagnostics.native_calls;
        report.native_calls_after_shutdown = diagnostics.native_calls_after_shutdown;
        report.cef_shutdown_calls = diagnostics.cef_shutdown_calls;
        report.app_reference_retained = Some(diagnostics.app_reference_retained);
        report.single_process_enabled = diagnostics.single_process_enabled;
        report.single_process_disabled = diagnostics.single_process_disabled;
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
        if report.error.is_none() && report.closed != report.created {
            report.error = Some(format!(
                "closed {} != created {}",
                report.closed, report.created
            ));
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

    fn finalize(report: &mut ProfileProbeReport, candidate: i32) -> i32 {
        report.exit_code = Some(candidate);
        if candidate != 0 {
            report.success = false;
            if report.error.is_none() {
                report.error = Some(format!("profile probe exited with {candidate}"));
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
                    .get_or_insert_with(|| format!("incomplete profile evidence: {error}"));
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
        let mut report = ProfileProbeReport::started(&options, os_thread_id());
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

        fn complete_report() -> ProfileProbeReport {
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
                    .join("profiles report.json")
                    .to_string_lossy()
                    .into_owned(),
                "--observer-gate".into(),
                std::env::temp_dir()
                    .join("observer.release")
                    .to_string_lossy()
                    .into_owned(),
            ])
            .unwrap();
            let mut report = ProfileProbeReport::started(&options, 1);
            report.requested_mode = "Threaded";
            report.actual_mode = Some("Threaded");
            report.actual_backend = Some("Software");
            report.pid = 1234;
            report.main_thread_id = 1;
            report.init_thread_id = Some(1);
            report.cef_ui_thread_id = Some(2);
            report.shutdown_thread_id = Some(1);
            report.manual_pump_count = 0;
            report.created = 7;
            report.closed = 7;
            report.valid_frame_browsers = 7;
            report.frames = 42;
            report.first_frame_width = FRAME_WIDTH_U32;
            report.first_frame_height = FRAME_HEIGHT_U32;
            report.tasks_posted = 10;
            report.tasks_completed = 10;
            report.pending_tasks = 0;
            report.cef_shutdown_calls = 1;
            report.native_calls = 100;
            report.native_calls_after_shutdown = 0;
            report.pending_requests = 0;
            report.pending_profiles = 0;
            report.profile_references_retained = 0;
            report.app_reference_retained = Some(false);
            report.single_process_enabled = Some(false);
            report.single_process_disabled = Some(true);
            report.runtime_generation = Some(1);
            report.default_profile_id = Some(0);
            report.p1_profile_id = Some(1);
            report.p2_profile_id = Some(2);
            report.default_profile_ready = true;
            report.same_handle_reused = true;
            report.other_browser_survived = true;
            report.observer_released = true;
            report.exit_code = Some(0);
            let (default_id, p1_id, p2_id) = (
                report.profile_id(ProfileSlot::Default).unwrap(),
                report.profile_id(ProfileSlot::P1).unwrap(),
                report.profile_id(ProfileSlot::P2).unwrap(),
            );
            report.storage_checks = REQUIRED_STORAGE
                .iter()
                .map(|(token, slot)| StorageCheck {
                    token: (*token).to_string(),
                    profile_id: match slot {
                        ProfileSlot::Default => default_id,
                        ProfileSlot::P1 => p1_id,
                        ProfileSlot::P2 => p2_id,
                    },
                    cookie_matches: true,
                    local_storage_matches: true,
                    passed: true,
                })
                .collect();
            report.identity_checks = REQUIRED_IDENTITY
                .iter()
                .map(|(left, right, same, sharing)| IdentityCheck {
                    left: (*left).to_string(),
                    right: (*right).to_string(),
                    same: *same,
                    sharing: *sharing,
                    passed: true,
                })
                .collect();
            report
        }

        fn fixture_with_result() -> ProfileFixture {
            use std::io::{Read, Write};
            use std::net::TcpStream;
            let fixture = ProfileFixture::new().unwrap();
            let url = fixture.url("ready-result", "read", "").unwrap();
            let address = fixture.origin().strip_prefix("http://").unwrap();
            let path = url.strip_prefix(fixture.origin()).unwrap();
            let exchange = |request: String| {
                let mut stream = TcpStream::connect(address).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream.write_all(request.as_bytes()).unwrap();
                let mut response = String::new();
                stream.read_to_string(&mut response).unwrap();
                assert!(response.starts_with("HTTP/1.1 200"));
            };
            exchange(format!("GET {path} HTTP/1.1\r\nHost: {address}\r\n\r\n"));
            let body = r#"{"token":"ready-result","cookie":null,"local_storage":null}"#;
            exchange(format!(
                "POST /result HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ));
            fixture
        }

        #[test]
        fn protocol_failure_wins_even_when_a_valid_result_is_waiting() {
            let fixture = fixture_with_result();
            assert!(fixture.url("ready-result", "read", "").is_err());
            assert!(next_observation(&fixture, "ready-result").is_err());
            assert!(
                fixture.take_result("ready-result").is_some(),
                "the valid result was not consumed to hide the error"
            );
        }

        #[test]
        fn protocol_failure_after_last_result_prevents_final_success() {
            let mut fixture = fixture_with_result();
            assert!(
                next_observation(&fixture, "ready-result")
                    .unwrap()
                    .is_some()
            );
            assert!(fixture.url("ready-result", "read", "").is_err());
            let mut report = complete_report();
            retain_fixture_failure(&mut report, fixture.finish());
            assert_eq!(finalize(&mut report, 0), 1);
            assert!(!report.success);
            assert!(
                report
                    .error
                    .as_ref()
                    .unwrap()
                    .contains("fixture protocol error")
            );
            let first = report.error.clone();
            retain_fixture_failure(&mut report, Vec::new());
            assert_eq!(report.error, first);
        }

        #[test]
        fn complete_evidence_can_only_become_success_after_exit_zero() {
            let mut report = complete_report();
            assert_eq!(finalize(&mut report, 0), 0);
            assert!(report.success);
            assert!(report.error.is_none());

            let mut mismatched = complete_report();
            mismatched.actual_mode = Some("Integrated");
            assert_eq!(finalize(&mut mismatched, 0), 1);
            assert!(!mismatched.success);
            assert!(mismatched.error.is_some());

            let mut failed_candidate = complete_report();
            assert_eq!(finalize(&mut failed_candidate, 1), 1);
            assert!(!failed_candidate.success);
        }

        #[test]
        fn required_backend_evidence_rejects_fallback() {
            let mut report = complete_report();
            report.requested_backend = "D3d11";
            assert_ne!(finalize(&mut report, 0), 0);

            let mut report = complete_report();
            report.requested_backend = "D3d11";
            report.actual_backend = Some("D3d11");
            assert_eq!(finalize(&mut report, 0), 0);
        }

        #[test]
        fn missing_or_unknown_storage_evidence_fails_closed() {
            let mut missing = complete_report();
            missing.storage_checks.pop();
            assert!(missing.validate().is_err());

            let mut unknown = complete_report();
            unknown.storage_checks[0].token = "unknown-token".into();
            assert!(unknown.validate().is_err());

            let mut duplicate = complete_report();
            duplicate.storage_checks[1] = duplicate.storage_checks[0].clone();
            assert!(duplicate.validate().is_err());

            let mut wrong_profile = complete_report();
            wrong_profile.storage_checks[0].profile_id = 2;
            assert!(wrong_profile.validate().is_err());

            let mut wrong_value = complete_report();
            wrong_value.storage_checks[0].cookie_matches = false;
            wrong_value.storage_checks[0].passed = false;
            assert!(wrong_value.validate().is_err());
        }

        #[test]
        fn missing_unknown_or_wrong_identity_evidence_fails_closed() {
            let mut missing = complete_report();
            missing.identity_checks.pop();
            assert!(missing.validate().is_err());

            let mut unknown = complete_report();
            unknown.identity_checks[0].left = "alien".into();
            assert!(unknown.validate().is_err());

            let mut duplicate = complete_report();
            duplicate.identity_checks[1] = duplicate.identity_checks[0].clone();
            assert!(duplicate.validate().is_err());

            let mut forged_same = complete_report();
            forged_same.identity_checks[2].same = true;
            forged_same.identity_checks[2].sharing = true;
            assert!(forged_same.validate().is_err());
        }

        #[test]
        fn invalidated_or_missing_lifecycle_evidence_fails_closed() {
            let cases: Vec<fn(&mut ProfileProbeReport)> = vec![
                |report| report.actual_mode = None,
                |report| report.actual_backend = None,
                |report| report.error = Some("boom".into()),
                |report| report.exit_code = None,
                |report| report.pid = 0,
                |report| report.init_thread_id = Some(9),
                |report| report.cef_ui_thread_id = Some(1),
                |report| report.shutdown_thread_id = None,
                |report| report.manual_pump_count = 1,
                |report| report.created = 5,
                |report| report.closed = 6,
                |report| report.valid_frame_browsers = 6,
                |report| report.frames = 0,
                |report| report.first_frame_width = 0,
                |report| report.first_frame_height = 2400,
                |report| report.tasks_posted = 0,
                |report| report.pending_tasks = 1,
                |report| report.tasks_completed = 9,
                |report| report.cef_shutdown_calls = 2,
                |report| report.native_calls_after_shutdown = 1,
                |report| report.pending_requests = 1,
                |report| report.pending_profiles = 1,
                |report| report.profile_references_retained = 1,
                |report| report.app_reference_retained = Some(true),
                |report| report.single_process_enabled = None,
                |report| report.single_process_enabled = Some(true),
                |report| report.single_process_disabled = None,
                |report| report.runtime_generation = None,
                |report| report.default_profile_id = None,
                |report| report.p1_profile_id = Some(0),
                |report| report.p2_profile_id = Some(1),
                |report| report.default_profile_ready = false,
                |report| report.same_handle_reused = false,
                |report| report.other_browser_survived = false,
                |report| report.observer_released = false,
            ];
            for (index, mutate) in cases.into_iter().enumerate() {
                let mut report = complete_report();
                mutate(&mut report);
                assert!(report.validate().is_err(), "case {index} must fail closed");
            }
        }

        #[test]
        fn integrated_mode_requires_same_thread_and_manual_pump() {
            let mut report = complete_report();
            report.requested_mode = "Integrated";
            report.actual_mode = Some("Integrated");
            report.cef_ui_thread_id = Some(1);
            report.manual_pump_count = 1;
            assert!(report.validate().is_ok());
            report.manual_pump_count = 0;
            assert!(report.validate().is_err());
            report.manual_pump_count = 1;
            report.cef_ui_thread_id = Some(2);
            assert!(report.validate().is_err());
        }
    }
}

fn main() {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    std::process::exit(windows::run());
    #[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
    {
        eprintln!("native profile probe requires Windows x64 MSVC");
        std::process::exit(2);
    }
}
