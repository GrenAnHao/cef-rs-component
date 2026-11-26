//! Isolated native integration host. Never used as the CEF helper executable.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[path = "runtime-probe/aux_devtools.rs"]
mod aux_devtools;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[path = "runtime-probe/capture_validation.rs"]
mod capture_validation;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[path = "runtime-probe/desktop_ux.rs"]
mod desktop_ux;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[path = "runtime-probe/find_isolation.rs"]
mod find_isolation;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[path = "runtime-probe/find_validation.rs"]
mod find_validation;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[path = "runtime-probe/host_capabilities.rs"]
mod host_capabilities;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[path = "runtime-probe/joint_find.rs"]
mod joint_find;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[path = "runtime-probe/js_bridge_schemes.rs"]
mod js_bridge_schemes;
#[path = "runtime-probe/menu_devtools.rs"]
mod menu_devtools;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[path = "runtime-probe/menu_edit_history.rs"]
mod menu_edit_history;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[path = "runtime-probe/native_child_hwnd.rs"]
mod native_child_hwnd;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[path = "runtime-probe/osr_parity.rs"]
mod osr_parity;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[path = "runtime-probe/owned_release.rs"]
mod owned_release;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[path = "runtime-probe/print_fence.rs"]
mod print_fence;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[path = "runtime-probe/profile_flush_validation.rs"]
mod profile_flush_validation;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[path = "runtime-probe/rejections.rs"]
mod rejections;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[path = "runtime-probe/source_validation.rs"]
mod source_validation;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[path = "runtime-probe/spelling_validation.rs"]
mod spelling_validation;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod windows {
    use crate::owned_release::OwnedRelease;
    use rust_cef_runtime::{
        contracts::{
            BrowserConfig, BrowserId, ProfileHandle, ProfileStorage, RenderPreference,
            RuntimeConfig, RuntimeFiles, SessionState, ThreadMode, TrustedContentPolicy, Viewport,
        },
        ports::{
            BrowserEvent, BrowserEventEnvelope, BrowserEventSink, CaptureFailureKind, CloseMode,
            EngineError, FrameSink, OwnedFrame, PolicyAction, PublishResult, SurfaceKind,
            UiDispatcher,
        },
        runtime::{
            CaptureDiagnostics, Runtime, RuntimeServices, ShutdownError, ShutdownStatus,
            ShutdownTicket, StartError,
        },
        test_support::{
            AcceleratedPressureCapture, CaptureFailureRecovery, CaptureTimingWitness,
            CloseTimeoutWitness, CommandPressureMenuClose, CreatePostRejection,
            DeviceCreateFailure, EarlyCloseBackendWitness, EventPressure, FrameProbeControl,
            LateCreateClose, LateFlushCleanup, MANUAL_PUMP_SAMPLE_CAPACITY, ManagedPressureCut,
            PRESSURE_EVENT_CAPACITY, PRESSURE_FIXTURE_URL, PRESSURE_SOURCE_FIXTURE_URL,
            PRESSURE_TITLE_PREFIX, PRESSURE_TITLE_READY, ProbeBrowserEvent, ProbeOptions,
            ProbeReport, ProbeScenario, RetiredFrameReplay, SourceVisitorDrain, TimingDistribution,
            VIEW_COPY_SAMPLE_CAPACITY,
        },
    };
    use std::{
        collections::HashSet,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    /// Deliberately unconsumed ordinary event lane of this validation host.
    /// Terminal events retain the separate two-slot Closed trace below.
    struct PressureEventLane {
        browser: rust_cef_runtime::contracts::BrowserId,
        titles: Vec<BrowserEventEnvelope>,
        seen: u64,
        rejected: u64,
        last_sequence: u64,
        last_rejected_sequence: u64,
        resynced_title: u64,
        closed: Option<(usize, u64)>,
    }

    impl PressureEventLane {
        fn new(browser: rust_cef_runtime::contracts::BrowserId) -> Self {
            Self {
                browser,
                titles: Vec::with_capacity(PRESSURE_EVENT_CAPACITY),
                seen: 0,
                rejected: 0,
                last_sequence: 0,
                last_rejected_sequence: 0,
                resynced_title: 0,
                closed: None,
            }
        }

        fn verdict(&self) -> EventPressure {
            let (ordinary_depth_at_first_close, first_close_sequence) =
                self.closed.unwrap_or_default();
            EventPressure {
                browser: (self.browser.runtime().value(), self.browser.local()),
                queued: self.titles.len(),
                rejected: self.rejected,
                last_rejected_sequence: self.last_rejected_sequence,
                accepted_sequences_ordered: self.titles.first().is_some_and(|e| e.sequence > 0)
                    && self
                        .titles
                        .windows(2)
                        .all(|w| w[1].sequence > w[0].sequence)
                    && self
                        .titles
                        .last()
                        .is_some_and(|e| e.sequence < self.last_rejected_sequence),
                retained_titles_ordered: self.titles.iter().enumerate().all(|(i, e)| {
                    e.browser == self.browser
                        && matches!(&e.event, BrowserEvent::TitleChanged(t)
                        if t == &format!("{PRESSURE_TITLE_PREFIX}{}", i+1))
                }),
                resynced_title: self.resynced_title,
                ordinary_depth_at_first_close,
                first_close_sequence,
            }
        }
    }

    #[derive(Default)]
    struct Observations {
        created: AtomicU64,
        expected_creation_cancellation: bool,
        expected_create_error: Option<EngineError>,
        rejected_create: Mutex<Option<(rust_cef_runtime::contracts::BrowserId, u64)>>,
        forbidden_frame_browser: Mutex<Option<rust_cef_runtime::contracts::BrowserId>>,
        frame_validation: bool,
        expected_capture_fault: Option<EngineError>,
        capture_fault_event: Mutex<Option<(rust_cef_runtime::contracts::BrowserId, u64)>>,
        expected_frame_fault: AtomicBool,
        expected_frame_faults_seen: AtomicU64,
        incomplete_capture_events: Mutex<Vec<serde_json::Value>>,
        cancelled_creates: AtomicU64,
        closed: AtomicU64,
        track_pressure_close_events: bool,
        pressure_close_events: Mutex<Vec<ProbeBrowserEvent>>,
        track_early_close: bool,
        early_closed: Mutex<Vec<ProbeBrowserEvent>>,
        early_cancelled: Mutex<Option<ProbeBrowserEvent>>,
        event_pressure: Mutex<Option<PressureEventLane>>,
        frames: AtomicU64,
        width: AtomicU32,
        height: AtomicU32,
        wakes: AtomicU64,
        valid_browsers: Mutex<HashSet<rust_cef_runtime::contracts::BrowserId>>,
        track_ux_events: bool,
        ux_events: Mutex<Vec<BrowserEvent>>,
        outbound_drags:
            Arc<Mutex<Vec<(BrowserId, rust_cef_core::outbound_drag::OutboundDragStart)>>>,
        policy_denials: Arc<Mutex<Vec<PolicyAction>>>,
        error: Mutex<Option<String>>,
    }
    impl Observations {
        fn apply_to(&self, report: &mut ProbeReport) {
            report.created = self.created.load(Ordering::Relaxed);
            report.closed = self.closed.load(Ordering::Relaxed);
            report.frames = self.frames.load(Ordering::Relaxed);
            report.first_frame_width = self.width.load(Ordering::Acquire);
            report.first_frame_height = self.height.load(Ordering::Acquire);
            report.valid_frame_browsers = self.valid_browsers.lock().unwrap().len() as u64;
            report.cancelled_creates = self.cancelled_creates.load(Ordering::Acquire);
            // Read after the shutdown barrier too: a valid first frame cannot
            // erase a CaptureFailed event delivered later in the same run.
            if let Some(error) = self.error.lock().unwrap().clone() {
                report.error.get_or_insert(error);
            }
        }
    }

    impl BrowserEventSink for Observations {
        fn publish(&self, envelope: BrowserEventEnvelope) -> PublishResult {
            if let BrowserEvent::TitleChanged(title) = &envelope.event
                && let Some(value) = title.strip_prefix(PRESSURE_TITLE_PREFIX)
                && let Some(lane) = self.event_pressure.lock().unwrap().as_mut()
            {
                let next = lane.seen + 1;
                if envelope.browser != lane.browser
                    || envelope.sequence <= lane.last_sequence
                    || value.parse::<u64>() != Ok(next)
                    || next > PRESSURE_EVENT_CAPACITY as u64 + 1
                {
                    self.error.lock().unwrap().get_or_insert(
                        "wrong owner, order or count of pressure title callbacks".into(),
                    );
                    return PublishResult::Rejected;
                }
                lane.seen = next;
                lane.last_sequence = envelope.sequence;
                if lane.titles.len() == PRESSURE_EVENT_CAPACITY {
                    lane.rejected += 1;
                    lane.last_rejected_sequence = envelope.sequence;
                    return PublishResult::Rejected;
                }
                lane.titles.push(envelope);
                return PublishResult::Published;
            }
            if self.track_ux_events
                && matches!(
                    &envelope.event,
                    BrowserEvent::CursorChanged(_)
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
                )
            {
                let mut ux = self.ux_events.lock().unwrap();
                if ux.len() < 256 {
                    ux.push(envelope.event.clone());
                }
            }
            match envelope.event {
                BrowserEvent::Created => {
                    self.created.fetch_add(1, Ordering::Relaxed);
                }
                BrowserEvent::Closed => {
                    if self.track_early_close {
                        let mut events = self.early_closed.lock().unwrap();
                        if events.len() < 2 {
                            events.push((browser_identity(envelope.browser), envelope.sequence));
                        } else {
                            self.error
                                .lock()
                                .unwrap()
                                .get_or_insert("extra early Closed event".into());
                        }
                    }
                    if self.track_pressure_close_events {
                        let mut seen = self.pressure_close_events.lock().unwrap();
                        if seen.len() == 2 {
                            self.error
                                .lock()
                                .unwrap()
                                .get_or_insert("extra pressure close event".into());
                        } else {
                            seen.push((
                                (envelope.browser.runtime().value(), envelope.browser.local()),
                                envelope.sequence,
                            ));
                        }
                    }
                    if let Some(lane) = self.event_pressure.lock().unwrap().as_mut()
                        && lane.browser == envelope.browser
                    {
                        if lane.closed.is_some() {
                            self.error
                                .lock()
                                .unwrap()
                                .get_or_insert("duplicate original pressure Closed event".into());
                        } else {
                            lane.closed = Some((lane.titles.len(), envelope.sequence));
                        }
                    }
                    self.closed.fetch_add(1, Ordering::Relaxed);
                }
                BrowserEvent::CreateFailed(EngineError::Closing)
                    if self.expected_creation_cancellation =>
                {
                    if self.track_early_close {
                        let mut cancelled = self.early_cancelled.lock().unwrap();
                        if cancelled.is_some() {
                            self.error
                                .lock()
                                .unwrap()
                                .get_or_insert("duplicate early cancellation".into());
                        } else {
                            *cancelled =
                                Some((browser_identity(envelope.browser), envelope.sequence));
                        }
                    }
                    self.cancelled_creates.fetch_add(1, Ordering::Relaxed);
                }
                BrowserEvent::CreateFailed(error)
                    if self.expected_create_error.as_ref() == Some(&error) =>
                {
                    let mut rejected = self.rejected_create.lock().unwrap();
                    if rejected.is_some() {
                        self.error
                            .lock()
                            .unwrap()
                            .get_or_insert("duplicate rejected create".into());
                    } else {
                        *rejected = Some((envelope.browser, envelope.sequence));
                    }
                }
                BrowserEvent::CaptureFailed(error)
                    if self.expected_capture_fault.as_ref() == Some(&error) =>
                {
                    let mut event = self.capture_fault_event.lock().unwrap();
                    if event.is_some() {
                        self.error
                            .lock()
                            .unwrap()
                            .get_or_insert("duplicate callback capture fault".into());
                    } else {
                        *event = Some((envelope.browser, envelope.sequence));
                    }
                }
                BrowserEvent::CaptureFailed(EngineError::Capture { kind, .. })
                    if self.frame_validation
                        && kind == rust_cef_runtime::ports::CaptureFailureKind::ResizeExhausted
                        && self
                            .expected_frame_fault
                            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                        && self
                            .expected_frame_faults_seen
                            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok() =>
                {
                    // One-shot acknowledgement of the one controlled exhaust.
                    // IncompleteFrame is never phase-whitelisted. Real errors
                    // require separate strict callback-witness accounting.
                }
                BrowserEvent::CaptureFailed(EngineError::Capture {
                    kind: rust_cef_runtime::ports::CaptureFailureKind::IncompleteFrame,
                    code,
                }) if self.frame_validation => {
                    // Kept as real failure evidence, never a phase whitelist.
                    // Final success requires exact event/witness/failure matching.
                    let mut events = self.incomplete_capture_events.lock().unwrap();
                    if events.len() < 32 {
                        events.push(serde_json::json!({"browser":format!("{:?}",envelope.browser),"event_sequence":envelope.sequence,"code":code}));
                    } else {
                        self.error
                            .lock()
                            .unwrap()
                            .get_or_insert("incomplete capture event overflow".into());
                    }
                }
                BrowserEvent::CreateFailed(error) | BrowserEvent::CaptureFailed(error) => {
                    self.error
                        .lock()
                        .unwrap()
                        .get_or_insert_with(|| format!("{:?}", error));
                }
                BrowserEvent::PolicyDenied { action } => {
                    let mut denials = self.policy_denials.lock().unwrap();
                    if denials.len() < 128 {
                        denials.push(action);
                    }
                }
                BrowserEvent::StartOutboundDrag(req) => {
                    let mut drags = self.outbound_drags.lock().unwrap();
                    if drags.len() < 32 {
                        drags.push((envelope.browser, req));
                    }
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
            if *self.forbidden_frame_browser.lock().unwrap() == Some(frame.info.browser) {
                self.error
                    .lock()
                    .unwrap()
                    .get_or_insert("frame from rejected or retired browser".into());
            }
            self.frames.fetch_add(1, Ordering::Relaxed);
            // The fixture has a solid opaque RGB(26,76,126) background. A blank
            // initial paint is not enough: require its actual BGRA center pixel.
            let width = frame.info.width;
            let height = frame.info.height;
            let center = ((height as usize / 2) * width as usize + width as usize / 2) * 4;
            let valid = if self.frame_validation {
                crate::capture_validation::valid_frame(&frame)
            } else {
                width == 320
                    && height == 240
                    && frame.cpu_pixels().and_then(|px| px.get(center..center + 4))
                        == Some(&[126, 76, 26, 255])
            };
            if valid {
                if self
                    .width
                    .compare_exchange(0, width, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    self.height.store(height, Ordering::Release);
                }
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
            // Native-only probe: this records an owned-data notification, not a
            // GPUI borrow. The separate GPUI probe is required by task 5.4.
            self.wakes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }
    /// Only Smoke uses this additional real FrameSource/ownership witness. It
    /// does not expand the process into the unrelated native scenario matrix.
    struct FrameMailboxSmoke {
        browser: rust_cef_runtime::contracts::BrowserId,
        source: Arc<dyn rust_cef_runtime::ports::FrameSource>,
        pixels: Arc<rust_cef_core::frame_budget::BudgetedPixels>,
        generation: u64,
        sequence: u64,
        live_used: usize,
        live_peak: usize,
        live_capture: rust_cef_runtime::runtime::CaptureDiagnostics,
    }
    impl FrameMailboxSmoke {
        fn observe(
            runtime: &Runtime,
            browser: rust_cef_runtime::contracts::BrowserId,
        ) -> Result<Self, String> {
            let source = runtime
                .frame_source(browser)
                .map_err(|e| format!("frame source: {e:?}"))?;
            let update = source
                .take_update()
                .ok_or("first frame source update missing")?;
            let frame = update.view.frame.as_ref().ok_or("source VIEW slot empty")?;
            let pixels = frame
                .cpu_pixels()
                .ok_or("FrameSource actual VIEW expected CPU pixels")?;
            let center = ((240 / 2) * 320 + 320 / 2) * 4;
            if update.browser != browser
                || update.closed
                || !update.view.visible
                || update.popup.visible
                || update.popup.frame.is_some()
                || frame.info.browser != browser
                || frame.info.surface != SurfaceKind::View
                || frame.info.surface_generation != update.view.generation
                || frame.info.sequence == 0
                || frame.info.width != 320
                || frame.info.height != 240
                || frame.info.format != rust_cef_runtime::ports::PixelFormat::PremultipliedBgra8
                || pixels.len() != 320 * 240 * 4
                || pixels.get(center..center + 4) != Some(&[126, 76, 26, 255])
            {
                return Err("FrameSource actual VIEW identity/layout/BGRA witness failed".into());
            }
            let stats = runtime
                .frame_budget_stats(browser)
                .map_err(|e| format!("frame budget: {e:?}"))?;
            if stats.limit != rust_cef_core::frame_budget::DEFAULT_CPU_FRAME_BUDGET
                || stats.used < pixels.len()
                || stats.used > stats.limit
                || stats.peak < stats.used
                || stats.peak > stats.limit
            {
                return Err("live frame budget witness failed".into());
            }
            let live_capture = runtime
                .frame_capture_diagnostics(browser)
                .map_err(|e| format!("capture diagnostics: {e:?}"))?;
            let ui_thread = runtime
                .diagnostics()
                .cef_ui_thread_id
                .or(runtime.diagnostics().context_thread_id);
            let backend_ok = match live_capture.observed_backend {
                Some("Software") => {
                    live_capture.requested_backend == Some("Software")
                        && live_capture.software_captures > 0
                        && live_capture.d3d11_captures == 0
                        && live_capture.maps == 0
                        && !live_capture.native_resources_live
                }
                Some("D3d11") => {
                    live_capture.requested_backend == Some("D3d11")
                        && live_capture.d3d11_captures > 0
                        && live_capture.software_captures == 0
                        && live_capture.opens >= live_capture.d3d11_captures
                        && live_capture.copies >= live_capture.d3d11_captures
                        && live_capture.maps >= live_capture.d3d11_captures
                        && live_capture.maps == live_capture.unmaps
                        && live_capture.live_imports == 0
                        && live_capture.staging_bytes >= pixels.len() as u64
                        && live_capture.native_resources_live
                }
                _ => false,
            };
            if !backend_ok
                || Some(live_capture.owner_thread) != ui_thread
                || live_capture.owner_thread == 0
                || live_capture.failures != 0
                || live_capture.retired
                || live_capture.abandoned
            {
                return Err(format!(
                    "actual capture backend/CEF UI ownership witness failed: {live_capture:?}"
                ));
            }
            Ok(Self {
                browser,
                source,
                pixels: pixels.clone(),
                generation: update.view.generation,
                sequence: frame.info.sequence,
                live_used: stats.used,
                live_peak: stats.peak,
                live_capture,
            })
        }
        fn finish(self, runtime: &Runtime) -> Result<serde_json::Value, String> {
            let terminal = self
                .source
                .take_update()
                .ok_or("closed frame source update missing")?;
            if !terminal.closed
                || terminal.browser != self.browser
                || terminal.view.frame.is_some()
                || terminal.popup.frame.is_some()
                || terminal.view.visible
                || terminal.popup.visible
                || self.source.latest(SurfaceKind::View).is_some()
                || self.source.latest(SurfaceKind::Popup).is_some()
            {
                return Err("same retained FrameSource not permanently closed/cleared".into());
            }
            let retained = runtime
                .frame_budget_stats(self.browser)
                .map_err(|e| format!("closed budget: {e:?}"))?;
            if retained.used != self.pixels.len() || retained.peak > retained.limit {
                return Err(
                    "detached live pixels were not exactly charged after native close".into(),
                );
            }
            let center = ((240 / 2) * 320 + 320 / 2) * 4;
            if self.pixels.get(center..center + 4) != Some(&[126, 76, 26, 255]) {
                return Err("detached frame pixels changed after shutdown".into());
            }
            drop(self.pixels);
            let released = runtime
                .frame_budget_stats(self.browser)
                .map_err(|e| format!("released budget: {e:?}"))?;
            if released.used != 0 {
                return Err("last pixel owner did not refund budget".into());
            }
            let retired_capture = runtime
                .frame_capture_diagnostics(self.browser)
                .map_err(|e| format!("retired capture: {e:?}"))?;
            if !retired_capture.retired
                || retired_capture.abandoned
                || retired_capture.native_resources_live
                || retired_capture.retirement_thread != self.live_capture.owner_thread
                || retired_capture.staging_bytes != 0
                || retired_capture.live_imports != 0
                || retired_capture.maps != retired_capture.unmaps
                || retired_capture.failures != 0
            {
                return Err(format!(
                    "capture native resource retirement witness failed: {retired_capture:?}"
                ));
            }
            Ok(serde_json::json!({
                "schema":2,"kind":"frame-mailbox-capture-smoke","success":true,
                "live_capture":self.live_capture,"retired_capture":retired_capture,
                "browser_local":self.browser.local(),"runtime_generation":self.browser.runtime().value(),
                "view_generation":self.generation,"view_sequence":self.sequence,
                "limit":retained.limit,"live_used":self.live_used,"live_peak":self.live_peak,
                "peak":released.peak,"held_after_native_close":retained.used,"used_after_last_pixel_drop":released.used,
                "source_closed":terminal.closed,"checks":[
                    {"id":"actual_source_view_bgra_identity","passed":true},
                    {"id":"live_cpu_budget_bounded","passed":true},
                    {"id":"same_source_terminal_cleared","passed":true},
                    {"id":"detached_pixels_survive_and_remain_charged","passed":true},
                    {"id":"last_pixel_drop_refunds","passed":true},
                    {"id":"actual_capture_backend_and_ui_sequence","passed":true},
                    {"id":"capture_native_resources_retired_on_ui","passed":true}
                ],"not_proven":["native POPUP composition","complete task 9.3 alpha/fingerprint/resize matrix","GPUI presentation or IME"]
            }))
        }
    }

    fn saturate_event_lane(
        runtime: &mut Runtime,
        options: &ProbeOptions,
        original: &rust_cef_runtime::ports::BrowserHandle,
        observations: &Observations,
    ) -> Result<(), String> {
        let browser = original.snapshot().browser;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let snapshot = original.snapshot();
            if snapshot.is_ready() && snapshot.navigation.title == PRESSURE_TITLE_READY {
                break;
            }
            if Instant::now() >= deadline {
                return Err("pressure page baseline title not observed".into());
            }
            tick(runtime, options.mode)?;
        }
        *observations.event_pressure.lock().unwrap() = Some(PressureEventLane::new(browser));
        let last = PRESSURE_EVENT_CAPACITY as u16 + 1;
        for step in 1..=last {
            runtime
                .probe_pressure_title(browser, step)
                .map_err(|e| format!("pressure title post at {step}: {e:?}"))?;
            loop {
                if let Some(error) = observations.error.lock().unwrap().clone() {
                    return Err(error);
                }
                if observations
                    .event_pressure
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|lane| lane.seen == u64::from(step))
                {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "missing CEF title callback for pressure step {step}"
                    ));
                }
                tick(runtime, options.mode)?;
            }
        }
        let snapshot = original.snapshot();
        let observed = snapshot
            .navigation
            .title
            .strip_prefix(PRESSURE_TITLE_PREFIX)
            .and_then(|value| value.parse::<u64>().ok());
        let mut state = observations.event_pressure.lock().unwrap();
        let lane = state.as_mut().ok_or("host pressure lane missing")?;
        if snapshot.browser != browser
            || !snapshot.is_ready()
            || observed != Some(u64::from(last))
            || lane.titles.len() != PRESSURE_EVENT_CAPACITY
            || lane.rejected != 1
        {
            return Err("latest snapshot did not resynchronize after event overflow".into());
        }
        lane.resynced_title = observed.unwrap();
        Ok(())
    }

    fn run_command_pressure_menu_close(
        runtime: &mut Runtime,
        options: &ProbeOptions,
        handles: &[rust_cef_runtime::ports::BrowserHandle],
        observations: &Observations,
        report: &mut ProbeReport,
        held_flush: Option<(ProfileHandle, u64)>,
    ) -> Result<(), String> {
        use rust_cef_core::{menu_lease::MenuLeaseOwner, ports::WindowBinding};
        let [first, survivor] = handles else {
            return Err("pressure fixture requires two Browsers".into());
        };
        let a = first.snapshot().browser;
        let b = survivor.snapshot().browser;
        let identity =
            |id: rust_cef_runtime::contracts::BrowserId| (id.runtime().value(), id.local());
        let a_source = runtime
            .frame_source(a)
            .map_err(|e| format!("first frame source: {e:?}"))?;
        let b_source = runtime
            .frame_source(b)
            .map_err(|e| format!("survivor frame source: {e:?}"))?;
        let mut owner = MenuLeaseOwner::new(
            WindowBinding {
                browser: a,
                window: a.local(),
            },
            std::num::NonZeroUsize::MIN,
        )
        .map_err(|e| format!("pressure menu lease: {e:?}"))?;
        runtime
            .probe_menu_deferred_enable(a, owner.lease())
            .map_err(|e| format!("enable pressure menu: {e:?}"))?;
        runtime
            .probe_menu_open(a)
            .map_err(|e| format!("open pressure menu: {e:?}"))?;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(menu) = runtime.probe_menu_status(a) {
                if menu.error.is_some() {
                    return Err(format!("pressure menu setup: {:?}", menu.error));
                }
                if menu.copies == 1 && menu.pending_continuations == 1 && menu.cancel_calls == 0 {
                    break;
                }
            }
            if Instant::now() >= deadline {
                return Err("pending real menu continuation not observed".into());
            }
            tick(runtime, options.mode)?;
        }
        match held_flush {
            Some((profile, sequence)) => {
                runtime.probe_command_pressure_close_with_flush(a, profile, sequence)
            }
            None => runtime.probe_command_pressure_close(a),
        }
        .map_err(|e| format!("post pressure test: {e:?}"))?;
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            if let Some(p) = runtime.probe_command_pressure_status(a) {
                if let Some(e) = p.error.as_deref() {
                    return Err(format!("pressure task: {e}"));
                }
                if first.snapshot().state == SessionState::Closed
                    && observations.closed.load(Ordering::Acquire) == 1
                {
                    let closed_events = observations.pressure_close_events.lock().unwrap().clone();
                    if !matches!(closed_events.as_slice(), [(first_id, sequence)] if *first_id == identity(a) && *sequence > 0)
                    {
                        return Err(
                            "first Closed event did not belong to the pressured Browser".into()
                        );
                    }
                    report.command_pressure_menu_close = Some(CommandPressureMenuClose {
                        pressure: p,
                        closed_browser: identity(a),
                        survivor_browser: identity(b),
                        frame_before: 0,
                        frame_after: 0,
                        first_source_closed: a_source.snapshot().closed,
                        closed_events,
                        accelerated_capture: None,
                    });
                    break;
                }
            }
            if Instant::now() >= deadline {
                return Err("close did not drain the pressured Browser".into());
            }
            tick(runtime, options.mode)?;
        }
        let witness = report.command_pressure_menu_close.as_mut().unwrap();
        let menu = runtime
            .probe_menu_status(a)
            .ok_or("first closed menu status missing")?;
        let menu_done = menu.retired
            && menu.complete(1)
            && menu.pending_continuations == 0
            && menu.dismissal_work == [1];
        report.menu_probe = Some(menu);
        if !witness.pressure.complete()
            || !menu_done
            || runtime.diagnostics().pending_requests != 0
            || !survivor.snapshot().is_ready()
        {
            return Err("close did not independently retire commands and one native menu".into());
        }
        owner
            .retire()
            .map_err(|e| format!("retire pressure menu lease: {e:?}"))?;
        let before = b_source
            .latest(SurfaceKind::View)
            .ok_or("survivor lost its frame")?;
        witness.frame_before = before.info.sequence;
        let accelerated_before = if options.backend == RenderPreference::D3d11Required {
            Some(pressure_capture_before_invalidate(runtime, a, b)?)
        } else {
            None
        };
        runtime
            .probe_frame_control(b, FrameProbeControl::InvalidateView)
            .map_err(|e| format!("survivor invalidate: {e:?}"))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(frame) = b_source.latest(SurfaceKind::View)
                && runtime.probe_frame_state(b).is_some_and(|s| s.applied == 1)
                && frame.info.browser == b
                && frame.info.sequence > before.info.sequence
                && frame.info.surface_generation == before.info.surface_generation
                && (frame.info.width, frame.info.height) == (320, 240)
                && frame
                    .cpu_pixels()
                    .and_then(|px| px.get(4 * (120 * 320 + 160)..4 * (120 * 320 + 160) + 4))
                    == Some(&[126, 76, 26, 255])
                && survivor.snapshot().is_ready()
                && observations.closed.load(Ordering::Acquire) == 1
            {
                witness.frame_after = frame.info.sequence;
                witness.accelerated_capture =
                    pressure_capture_after_invalidate(runtime, b, accelerated_before)?;
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("survivor did not repaint after the pressured close".into());
            }
            tick(runtime, options.mode)?;
        }
    }

    fn pressure_capture_before_invalidate(
        runtime: &Runtime,
        first: BrowserId,
        survivor: BrowserId,
    ) -> Result<(CaptureDiagnostics, u64), String> {
        let capture = |browser| {
            runtime
                .frame_capture_diagnostics(browser)
                .map_err(|e| format!("pressure capture before survivor invalidate: {e:?}"))
        };
        Ok((capture(first)?, capture(survivor)?.maps))
    }

    fn pressure_capture_after_invalidate(
        runtime: &Runtime,
        survivor: BrowserId,
        before: Option<(CaptureDiagnostics, u64)>,
    ) -> Result<Option<AcceleratedPressureCapture>, String> {
        before
            .map(|(first_retired, survivor_maps_before_invalidate)| {
                Ok(AcceleratedPressureCapture {
                    first_retired,
                    survivor_maps_before_invalidate,
                    survivor_live_after_invalidate: runtime
                        .frame_capture_diagnostics(survivor)
                        .map_err(|e| format!("pressure survivor capture after A close: {e:?}"))?,
                    survivor_retired: None,
                })
            })
            .transpose()
    }

    fn observe_first_close_with_source_held(
        runtime: &mut Runtime,
        options: &ProbeOptions,
        probe: &crate::source_validation::SourceProbe,
        handles: &[rust_cef_runtime::ports::BrowserHandle],
        observations: &Observations,
        managed_cut: Option<ManagedPressureCut>,
    ) -> Result<SourceVisitorDrain, String> {
        let [first, peer] = handles else {
            return Err("source retirement requires two Browsers".into());
        };
        let a = first.snapshot().browser;
        let b = peer.snapshot().browser;
        // Owner-approved combined cut: keep a real managed DevTools auxiliary
        // associated with the survivor while the original visitor is held, so
        // the auxiliary-before-shutdown condition is exercised inside the same
        // run instead of in a separate scenario.
        let hold_aux = options.scenario.holds_aux_devtools();
        if hold_aux {
            runtime
                .configure_devtools(std::num::NonZeroUsize::new(1).unwrap())
                .map_err(|e| format!("combined cut configure_devtools: {e:?}"))?;
            runtime
                .open_devtools(b)
                .map_err(|e| format!("combined cut open_devtools: {e:?}"))?;
            let until = Instant::now() + Duration::from_secs(20);
            loop {
                if runtime.devtools_associated(b) == Some(true) {
                    break;
                }
                if Instant::now() >= until {
                    return Err("combined cut auxiliary never associated".into());
                }
                tick(runtime, options.mode)?;
            }
        }
        let original = runtime
            .frame_source(a)
            .map_err(|e| format!("first source: {e:?}"))?;
        let survivor = runtime
            .frame_source(b)
            .map_err(|e| format!("survivor source: {e:?}"))?;
        let id = runtime
            .probe_held_source_visitor()
            .filter(|id| id.browser == a)
            .ok_or("original native visitor not held")?;
        if let Some(cut) = managed_cut {
            if cut.source != id || cut.find.browser != b || cut.menu.browser != a {
                return Err(
                    "pressure Close did not target these original Browser operations".into(),
                );
            }
            runtime
                .probe_command_pressure_close_with_operations(cut)
                .map_err(|e| format!("post original managed Close pressure: {e:?}"))?;
        } else {
            let _ = first.request_close(CloseMode::Force);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            probe.flush_close()?;
            if managed_cut.is_some()
                && let Some(error) = runtime
                    .probe_command_pressure_status(a)
                    .and_then(|w| w.error)
            {
                return Err(format!("managed Close pressure task: {error}"));
            }
            if first.snapshot().state == SessionState::Closed
                && observations.closed.load(Ordering::Acquire) == 1
            {
                break;
            }
            if Instant::now() >= deadline {
                return Err("original Browser did not close with source visitor held".into());
            }
            tick(runtime, options.mode)?;
        }
        let closed_events = observations.pressure_close_events.lock().unwrap().clone();
        if !matches!(closed_events.as_slice(), [(owner, sequence)] if *owner == (a.runtime().value(), a.local()) && *sequence > 0)
        {
            return Err("first terminal event did not belong to A".into());
        }
        probe.flush_close()?;
        let d = runtime.source_view_diagnostics();
        let [q] = d.active.as_slice() else {
            return Err("expected one cancelled source row after closing A".into());
        };
        if !peer.snapshot().is_ready()
            || runtime.probe_held_source_visitor() != Some(id)
            || q.operation != id
            || !q.cancelled
            || !q.callback_received
            || !q.native_live
            || q.host_live
            || d.pending != 1
            || d.native_live != 1
            || d.host_live != 0
            || d.faulted
        {
            return Err(
                "expected only the original native visitor reference after A closed".into(),
            );
        }
        let before = survivor
            .latest(SurfaceKind::View)
            .ok_or("survivor first frame missing")?
            .info;
        // Sample both original Browser IDs after A Closed and before this
        // existing B invalidate. No extra frames, pumps or retirement work.
        let accelerated_before =
            if managed_cut.is_some() && options.backend == RenderPreference::D3d11Required {
                Some(pressure_capture_before_invalidate(runtime, a, b)?)
            } else {
                None
            };
        runtime
            .probe_frame_control(b, FrameProbeControl::InvalidateView)
            .map_err(|e| format!("survivor invalidate: {e:?}"))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if runtime.probe_frame_state(b).is_some_and(|p| p.applied == 1)
                && peer.snapshot().is_ready()
                && observations.closed.load(Ordering::Acquire) == 1
                && let Some(frame) = survivor.latest(SurfaceKind::View)
                && frame.info.browser == b
                && frame.info.sequence > before.sequence
                && frame.info.surface_generation == before.surface_generation
                && (frame.info.width, frame.info.height) == (320, 240)
                && frame
                    .cpu_pixels()
                    .and_then(|px| px.get(4 * (120 * 320 + 160)..4 * (120 * 320 + 160) + 4))
                    == Some(&[126, 76, 26, 255])
            {
                return Ok(SourceVisitorDrain {
                    operation: [a.runtime().value(), a.local(), id.sequence],
                    survivor: (b.runtime().value(), b.local()),
                    source_closed: original.snapshot().closed,
                    callback_received_before_release: q.callback_received,
                    native_live_after_first_close: d.native_live,
                    host_live_after_first_close: d.host_live,
                    survivor_frame_before: before.sequence,
                    survivor_frame_after: frame.info.sequence,
                    survivor_witness: None,
                    aux_held_during_cut: hold_aux,
                    aux_associated_during_cut: runtime.devtools_associated(b),
                    aux_pending_before_shutdown: None,
                    aux_retired_before_shutdown: None,
                    closed_events,
                    accelerated_capture: pressure_capture_after_invalidate(
                        runtime,
                        b,
                        accelerated_before,
                    )?,
                    shutdown_calls_before_release: None,
                    blocked_only_by_source: false,
                    flush_marker_at_joint_barrier: None,
                    source_retired_before_flush_release: None,
                    explicit_release_posted: false,
                    held_reference_gone: false,
                });
            }
            if Instant::now() >= deadline && runtime.devtools_pending(b) > 0 {
                // An associated auxiliary suppresses new View frames for the
                // survivor, so a new frame cannot be the liveness witness here.
                // Accept a different witness, and only a different witness: the
                // invalidate task really ran on the survivor's UI thread, the
                // survivor's stored frame identity is unchanged (nothing was
                // dropped by the other Browser's close), and a fresh command
                // still round-trips through the survivor's own executor.
                if runtime.probe_frame_state(b).is_some_and(|p| p.applied == 1)
                    && let Some(frame) = survivor.latest(SurfaceKind::View)
                    && frame.info.browser == b
                    && frame.info.sequence == before.sequence
                    && frame.info.surface_generation == before.surface_generation
                    && (frame.info.width, frame.info.height) == (before.width, before.height)
                    && frame
                        .cpu_pixels()
                        .and_then(|px| px.get(4 * (120 * 320 + 160)..4 * (120 * 320 + 160) + 4))
                        == Some(&[126, 76, 26, 255])
                {
                    peer.submit(rust_cef_runtime::ports::BrowserCommand::SetFocus(true))
                        .map_err(|e| format!("combined cut survivor round-trip post: {e:?}"))?;
                    let until = Instant::now() + Duration::from_secs(10);
                    while runtime.diagnostics().tasks.pending != 0 {
                        if Instant::now() >= until {
                            return Err(
                                "combined cut survivor never ran a task after the other closed"
                                    .into(),
                            );
                        }
                        tick(runtime, options.mode)?;
                    }
                    return Ok(SourceVisitorDrain {
                        operation: [a.runtime().value(), a.local(), id.sequence],
                        survivor: (b.runtime().value(), b.local()),
                        source_closed: original.snapshot().closed,
                        callback_received_before_release: q.callback_received,
                        native_live_after_first_close: d.native_live,
                        host_live_after_first_close: d.host_live,
                        survivor_frame_before: before.sequence,
                        survivor_frame_after: frame.info.sequence,
                        survivor_witness: Some(
                            "auxiliary_outstanding:invalidate_applied+frame_identity_held+task_roundtrip"
                                .into(),
                        ),
                        aux_held_during_cut: hold_aux,
                        aux_associated_during_cut: runtime.devtools_associated(b),
                        aux_pending_before_shutdown: None,
                        aux_retired_before_shutdown: None,
                        closed_events,
                        accelerated_capture: pressure_capture_after_invalidate(
                            runtime,
                            b,
                            accelerated_before,
                        )?,
                        shutdown_calls_before_release: None,
                        blocked_only_by_source: false,
                        flush_marker_at_joint_barrier: None,
                        source_retired_before_flush_release: None,
                        explicit_release_posted: false,
                        held_reference_gone: false,
                    });
                }
            }
            if Instant::now() >= deadline {
                // Record what the survivor actually reached: without the last
                // observed sequence this timeout is indistinguishable from a
                // stall, a generation change or no frame at all.
                let seen = survivor
                    .latest(SurfaceKind::View)
                    .map(|f| {
                        format!(
                            "seq={} gen={} {}x{}",
                            f.info.sequence, f.info.surface_generation, f.info.width, f.info.height
                        )
                    })
                    .unwrap_or_else(|| "no View frame at all".into());
                return Err(format!(
                    "survivor did not produce a frame while the original visitor was held: before seq={} gen={}, last {seen}",
                    before.sequence, before.surface_generation
                ));
            }
            tick(runtime, options.mode)?;
        }
    }

    fn observe_source_shutdown_barrier(
        runtime: &mut Runtime,
        ticket: &ShutdownTicket,
        mode: ThreadMode,
        report: &mut SourceVisitorDrain,
        flush: Option<(ProfileHandle, u64)>,
    ) -> Result<(), String> {
        let expected_flushes = u64::from(flush.is_some());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match runtime.poll_shutdown(ticket) {
                ShutdownStatus::Draining {
                    open_browsers,
                    pending_tasks: 0,
                    pending_requests: 1,
                    pending_profiles: 0,
                    pending_cookie_flushes,
                    post_failures: 0,
                } if open_browsers.is_empty() && pending_cookie_flushes == expected_flushes => {
                    break;
                }
                ShutdownStatus::Draining { .. } => {}
                status => return Err(format!("unexpected hold shutdown state: {status:?}")),
            }
            if Instant::now() >= deadline {
                return Err(
                    "shutdown did not reach the exact original visitor/flush barrier".into(),
                );
            }
            tick(runtime, mode)?;
        }
        let d = runtime.source_view_diagnostics();
        let Some(id) = runtime.probe_held_source_visitor() else {
            return Err("original visitor missing before release".into());
        };
        if d.active.len() != 1
            || d.active[0].operation != id
            || d.pending != 1
            || d.native_live != 1
            || d.host_live != 0
            || !d.active[0].callback_received
            || !d.active[0].cancelled
            || d.faulted
            || [
                id.browser.runtime().value(),
                id.browser.local(),
                id.sequence,
            ] != report.operation
        {
            return Err("non-original source state in shutdown barrier".into());
        }
        let refused = runtime.finish_shutdown();
        let diagnostics = runtime.diagnostics();
        report.shutdown_calls_before_release = Some(diagnostics.cef_shutdown_calls);
        let blocked = matches!(refused, Err(ShutdownError::Blocked {open_browsers, pending_tasks:0,
            pending_requests:1,pending_profiles:0,pending_cookie_flushes,post_failures:0})
                if open_browsers.is_empty() && pending_cookie_flushes == expected_flushes);
        if !blocked || report.shutdown_calls_before_release != Some(0) {
            return Err("finish_shutdown did not refuse the exact outstanding resources".into());
        }
        if let Some((profile, sequence)) = flush {
            if diagnostics.cookie_flushes_accepted != 1
                || diagnostics.cookie_flushes_completed != 0
                || !runtime.probe_cookie_flush_marker().is_some_and(|m| {
                    m.profile == profile
                        && m.sequence == sequence
                        && !m.faulted
                        && !m.release_reserved
                        && Some(m.callback_thread) == diagnostics.cef_ui_thread_id
                })
            {
                return Err("original flush marker not retained beside the visitor".into());
            }
            report.flush_marker_at_joint_barrier =
                Some([profile.runtime().value(), profile.local(), sequence]);
        } else {
            report.blocked_only_by_source = true;
        }
        Ok(())
    }

    fn release_source_once(
        runtime: &Runtime,
        owner: BrowserId,
        action: &mut OwnedRelease,
        report: &mut ProbeReport,
    ) {
        if action.attempted() {
            return;
        }
        let Some(id) = runtime.probe_held_source_visitor() else {
            return;
        };
        if id.browser != owner || id.sequence == 0 {
            report
                .error
                .get_or_insert("refusing non-original source release".into());
            return;
        }
        let Some(id) = action.claim(Some(id)) else {
            return;
        };
        match runtime.probe_release_source_visitor(id) {
            Ok(()) => {
                if let Some(w) = report.source_visitor_drain.as_mut() {
                    w.explicit_release_posted = true;
                }
            }
            Err(e) => {
                report
                    .error
                    .get_or_insert(format!("release retained source visitor: {e:?}"));
            }
        }
    }

    fn run_retired_frame_replay(
        runtime: &mut Runtime,
        options: &ProbeOptions,
        handles: &[rust_cef_runtime::ports::BrowserHandle],
        observations: &Observations,
    ) -> Result<RetiredFrameReplay, String> {
        let [old, live] = handles else {
            return Err("replay requires two real browsers".into());
        };
        let old_id = old.snapshot().browser;
        let live_id = live.snapshot().browser;
        let identity =
            |id: rust_cef_runtime::contracts::BrowserId| (id.runtime().value(), id.local());
        let source = |id| {
            runtime
                .frame_source(id)
                .map_err(|e| format!("replay source: {e:?}"))
        };
        let (old_source, live_source) = (source(old_id)?, source(live_id)?);
        let valid = |frame: &OwnedFrame, owner| {
            frame.info.browser == owner
                && frame.info.surface == SurfaceKind::View
                && (frame.info.width, frame.info.height) == (320, 240)
                && frame
                    .cpu_pixels()
                    .and_then(|px| px.get(4 * (120 * 320 + 160)..4 * (120 * 320 + 160) + 4))
                    == Some(&[126, 76, 26, 255])
        };
        let retained = old_source
            .latest(SurfaceKind::View)
            .ok_or("original native frame missing")?;
        if !valid(&retained, old_id) || old.snapshot().state != SessionState::Alive {
            return Err("original live Browser frame did not match fixture".into());
        }
        for id in [old_id, live_id] {
            let d = runtime
                .frame_capture_diagnostics(id)
                .map_err(|e| format!("replay capture: {e:?}"))?;
            if d.observed_backend != Some(options.backend_name()) {
                return Err("per-Browser required backend not observed".into());
            }
        }
        let _ = old.request_close(CloseMode::Force);
        let deadline = Instant::now() + Duration::from_secs(10);
        while old.snapshot().state != SessionState::Closed
            || observations.closed.load(Ordering::Acquire) != 1
        {
            if let Some(error) = observations.error.lock().unwrap().clone() {
                return Err(error);
            }
            if Instant::now() >= deadline {
                return Err("original native close not observed".into());
            }
            tick(runtime, options.mode)?;
        }
        *observations.forbidden_frame_browser.lock().unwrap() = Some(old_id);
        let closed_on_same_source = Arc::ptr_eq(
            &old_source,
            &runtime
                .frame_source(old_id)
                .map_err(|e| format!("terminal source: {e:?}"))?,
        );
        let budget = |rt: &Runtime| {
            rt.frame_budget_stats(old_id)
                .map(|stats| stats.used)
                .map_err(|e| format!("old frame budget: {e:?}"))
        };
        let old_cpu_bytes_before_drop = budget(runtime)?;
        let live_frame = live_source
            .latest(SurfaceKind::View)
            .ok_or("live Browser lost its current native frame")?;
        let generation = retained.info.surface_generation;
        let retired_sequence = retained.info.sequence;
        let live_sequence_before = live_frame.info.sequence;
        if !valid(&live_frame, live_id)
            || !live.snapshot().is_ready()
            || live_frame.info.surface_generation != generation
        {
            return Err(
                "replay did not have two same-geometry, same-surface-generation frames".into(),
            );
        }
        let [closed_error, live_error] = runtime
            .probe_replay_retired_view(live_id, &retained)
            .map_err(|e| format!("replay admission: {e:?}"))?;
        let terminal = old_source.snapshot();
        let old_source_closed_empty = closed_on_same_source
            && terminal.closed
            && terminal.view.frame.is_none()
            && terminal.popup.frame.is_none();
        let live_frame_unchanged = live_source
            .latest(SurfaceKind::View)
            .is_some_and(|f| Arc::ptr_eq(&live_frame, &f));
        drop(live_frame);
        drop(retained);
        let old_cpu_bytes_after_drop = budget(runtime)?;
        let mut witness = RetiredFrameReplay {
            old_browser: identity(old_id),
            live_browser: identity(live_id),
            generation,
            retired_sequence,
            live_sequence_before,
            live_sequence_after_repaint: 0,
            closed_error,
            live_error,
            old_source_closed_empty,
            live_frame_unchanged,
            old_cpu_bytes_before_drop,
            old_cpu_bytes_after_drop,
        };
        if witness.closed_error.as_deref() != Some("Closed")
            || witness.live_error.as_deref() != Some("WrongIdentity")
            || !witness.old_source_closed_empty
            || !witness.live_frame_unchanged
            || witness.old_cpu_bytes_after_drop != 0
        {
            return Ok(witness); // keep the actual refusal results in a failed report
        }
        runtime
            .probe_frame_control(live_id, FrameProbeControl::InvalidateView)
            .map_err(|e| format!("invalidate original survivor: {e:?}"))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(error) = observations.error.lock().unwrap().clone() {
                return Err(error);
            }
            let applied = runtime
                .probe_frame_state(live_id)
                .is_some_and(|p| p.applied == 1);
            if applied
                && live.snapshot().is_ready()
                && let Some(frame) = live_source.latest(SurfaceKind::View)
                && valid(&frame, live_id)
                && frame.info.surface_generation == generation
                && frame.info.sequence > live_sequence_before
            {
                witness.live_sequence_after_repaint = frame.info.sequence;
                return Ok(witness);
            }
            if Instant::now() >= deadline {
                return Ok(witness);
            }
            tick(runtime, options.mode)?;
        }
    }

    fn run_capture_fault(
        runtime: &mut Runtime,
        options: &ProbeOptions,
        handle: &rust_cef_runtime::ports::BrowserHandle,
        observations: &Observations,
    ) -> Result<CaptureFailureRecovery, String> {
        let browser = handle.snapshot().browser;
        let identity = (browser.runtime().value(), browser.local());
        let source = runtime
            .frame_source(browser)
            .map_err(|e| format!("fault source: {e:?}"))?;
        if source.latest(SurfaceKind::View).is_none()
            || observations.capture_fault_event.lock().unwrap().is_some()
        {
            return Err("capture failure probe lacks a good frame or has a pre-arm fault".into());
        }
        runtime
            .probe_frame_control(browser, FrameProbeControl::CaptureFaultAndInvalidate)
            .map_err(|e| format!("arm capture fault: {e:?}"))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let (callback, event_sequence) = loop {
            if let Some(error) = observations.error.lock().unwrap().clone() {
                return Err(error);
            }
            let state = runtime
                .probe_frame_state(browser)
                .ok_or("capture fault state unavailable")?;
            let event = *observations.capture_fault_event.lock().unwrap();
            if let Some(callback) = state.capture_fault {
                if !callback.complete()
                    || callback.browser != identity
                    || callback.backend != options.backend_name()
                    || !state.drop_view
                    || state.applied != 1
                {
                    return Err(format!("unexpected callback fault witness: {callback:?}"));
                }
                if let Some((owner, sequence)) = event {
                    if owner != browser {
                        return Err("callback fault reported for another Browser".into());
                    }
                    break (callback, sequence);
                }
            }
            if Instant::now() >= deadline {
                return Err("timed out waiting for callback fault and event".into());
            }
            tick(runtime, options.mode)?;
        };
        let old = source
            .latest(SurfaceKind::View)
            .ok_or("good frame lost after fault")?;
        let good = callback
            .good_frame
            .ok_or("missing callback frame identity")?;
        if (old.info.surface_generation, old.info.sequence) != good || old.info.browser != browser {
            return Err("good frame changed while failed VIEW was held".into());
        }
        drop(old);
        runtime
            .probe_frame_control(browser, FrameProbeControl::ResumeCaptureAndInvalidate)
            .map_err(|e| format!("resume capture: {e:?}"))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(error) = observations.error.lock().unwrap().clone() {
                return Err(error);
            }
            let state = runtime
                .probe_frame_state(browser)
                .ok_or("capture resume state unavailable")?;
            if !state.drop_view
                && state.applied == 2
                && let Some(new) = source.latest(SurfaceKind::View)
                && new.info.browser == browser
                && new.info.surface_generation == good.0
                && new.info.sequence > good.1
                && (new.info.width, new.info.height) == (320, 240)
                && new
                    .cpu_pixels()
                    .and_then(|px| px.get(4 * (120 * 320 + 160)..4 * (120 * 320 + 160) + 4))
                    == Some(&[126, 76, 26, 255])
            {
                let diag = runtime
                    .frame_capture_diagnostics(browser)
                    .map_err(|e| format!("capture diag: {e:?}"))?;
                if diag.failures != 1
                    || diag.observed_backend != Some(options.backend_name())
                    || !handle.snapshot().is_ready()
                {
                    return Err("expected one classified fault and same live Browser".into());
                }
                return Ok(CaptureFailureRecovery {
                    callback,
                    event_sequence,
                    recovered_frame: (new.info.surface_generation, new.info.sequence),
                    retired_capture: None,
                    closed_source: false,
                    final_cpu_bytes: None,
                });
            }
            if Instant::now() >= deadline {
                return Err("timed out waiting for same-generation frame recovery".into());
            }
            tick(runtime, options.mode)?;
        }
    }

    fn tick(runtime: &mut Runtime, mode: ThreadMode) -> Result<(), String> {
        if mode == ThreadMode::Integrated {
            runtime.pump().map_err(|error| format!("pump: {error:?}"))?;
        }
        thread::sleep(Duration::from_millis(3));
        Ok(())
    }
    fn browser_identity(id: rust_cef_runtime::contracts::BrowserId) -> (u64, u64) {
        (id.runtime().value(), id.local())
    }

    fn wait_for_observer(runtime: &mut Runtime, options: &ProbeOptions) -> Result<bool, String> {
        let Some(gate) = &options.observer_gate else {
            return Ok(false);
        };
        rust_cef_runtime::test_support::publish_observer_ready(gate, std::process::id())
            .map_err(|error| format!("observer ready: {error}"))?;
        let deadline = Instant::now() + Duration::from_secs(20);
        while !gate.is_file() {
            if Instant::now() >= deadline {
                return Err("external image observer timeout".into());
            }
            tick(runtime, options.mode)?;
        }
        Ok(true)
    }

    const FRAME_FIXTURE: &str = "data:text/html,%3Cbody%20style%3D%22margin%3A0%3Bbackground%3Argb(26%2C76%2C126)%22%3E%3C%2Fbody%3E";

    fn frame_fixture_config(backend: RenderPreference) -> BrowserConfig {
        BrowserConfig {
            initial_url: FRAME_FIXTURE.into(),
            profile: None,
            render_preference: backend,
            viewport: Viewport {
                width: 320,
                height: 240,
                scale: 1.0,
            },
            options: Default::default(),
        }
    }

    fn observe_early_backend(
        runtime: &mut Runtime,
        options: &ProbeOptions,
        observations: &Observations,
        browser: &rust_cef_runtime::ports::BrowserHandle,
        deadline: Instant,
    ) -> Result<EarlyCloseBackendWitness, String> {
        let id = browser.snapshot().browser;
        while browser.snapshot().state != SessionState::Alive
            || !observations.valid_browsers.lock().unwrap().contains(&id)
        {
            if let Some(error) = observations.error.lock().unwrap().clone() {
                return Err(error);
            }
            if Instant::now() >= deadline {
                return Err(
                    "early-close witness Browser did not publish a real fixture frame".into(),
                );
            }
            tick(runtime, options.mode)?;
        }
        let mailbox = FrameMailboxSmoke::observe(runtime, id)?;
        let witness = EarlyCloseBackendWitness {
            backend_browser: browser_identity(id),
            target_browser: None,
            frame_generation: mailbox.generation,
            frame_sequence: mailbox.sequence,
            capture: mailbox.live_capture.clone(),
            observer_released: false,
            target_before_drain: None,
            closed_events: Vec::new(),
            cancelled_creation: None,
            backend_source_closed: false,
            retired_capture: None,
            target_source_closed: false,
        };
        drop(mailbox);
        if witness.capture.observed_backend != Some("D3d11") {
            return Err("early-close witness did not use actual D3d11".into());
        }
        Ok(witness)
    }

    fn fixture_view(frame: &OwnedFrame, browser: rust_cef_runtime::contracts::BrowserId) -> bool {
        frame.info.browser == browser
            && frame.info.surface == SurfaceKind::View
            && (frame.info.width, frame.info.height) == (320, 240)
            && frame.info.format == rust_cef_runtime::ports::PixelFormat::PremultipliedBgra8
            && frame.cpu_pixels().is_some_and(|px| {
                px.len() == 320 * 240 * 4
                    && px.get(4 * (120 * 320 + 160)..4 * (120 * 320 + 160) + 4)
                        == Some(&[126, 76, 26, 255])
            })
    }

    fn measure_capture_timing(
        runtime: &mut Runtime,
        options: &ProbeOptions,
        handle: &rust_cef_runtime::ports::BrowserHandle,
        observations: &Observations,
        report: &mut ProbeReport,
        deadline: Instant,
        observer_released: bool,
    ) -> Result<(), String> {
        let browser = handle.snapshot().browser;
        let source = runtime
            .frame_source(browser)
            .map_err(|e| format!("timing frame source: {e:?}"))?;
        while handle.snapshot().navigation.is_loading {
            if Instant::now() >= deadline {
                return Err("timing fixture did not finish loading".into());
            }
            tick(runtime, options.mode)?;
        }
        let baseline = source
            .latest(SurfaceKind::View)
            .filter(|f| fixture_view(f, browser))
            .ok_or("timing baseline VIEW missing")?;
        let (generation, baseline_sequence) =
            (baseline.info.surface_generation, baseline.info.sequence);
        drop(baseline);
        let baseline_capture = runtime
            .frame_capture_diagnostics(browser)
            .map_err(|e| format!("timing baseline capture: {e:?}"))?;
        if !observer_released || baseline_capture.observed_backend != Some(options.backend_name()) {
            return Err(
                "timing requires actual fixture backend and external image observer".into(),
            );
        }
        runtime
            .probe_arm_view_copy_timing(browser)
            .map_err(|e| format!("arm VIEW copy timing: {e:?}"))?;
        let witness = report.capture_timing.insert(CaptureTimingWitness {
            browser: browser_identity(browser),
            generation,
            baseline_sequence,
            baseline_capture,
            observer_released,
            invalidates_applied: 0,
            published_sequences: Vec::with_capacity(VIEW_COPY_SAMPLE_CAPACITY),
            copy_samples: Vec::new(),
            copy_ns: None,
            manual_pump_calls: 0,
            manual_pump_samples_ns: Vec::with_capacity(MANUAL_PUMP_SAMPLE_CAPACITY),
            manual_pump_ns: None,
            rearm_refused: false,
            source_closed: false,
            final_cpu_bytes: None,
            peak_cpu_bytes: None,
            cpu_limit_bytes: None,
            retired_capture: None,
        });
        let mut previous = baseline_sequence;
        for step in 1..=VIEW_COPY_SAMPLE_CAPACITY {
            runtime
                .probe_frame_control(browser, FrameProbeControl::InvalidateView)
                .map_err(|e| format!("timing invalidate {step}: {e:?}"))?;
            loop {
                if let Some(error) = observations.error.lock().unwrap().clone() {
                    return Err(error);
                }
                let control = runtime
                    .probe_frame_state(browser)
                    .ok_or("timing control state missing")?;
                witness.invalidates_applied = control.applied;
                if let Some(frame) = source.latest(SurfaceKind::View)
                    && frame.info.sequence > previous
                    && control.applied == step as u64
                {
                    if !fixture_view(&frame, browser)
                        || frame.info.surface_generation != generation
                        || !handle.snapshot().is_ready()
                    {
                        return Err(
                            "timed repaint changed Browser, generation or fixture pixels".into(),
                        );
                    }
                    let samples = runtime
                        .probe_view_copy_samples(browser)
                        .ok_or("timing sample lane missing")?;
                    if samples.len() != step || samples[step - 1].sequence != frame.info.sequence {
                        return Err(format!(
                            "timed copy and actual mailbox VIEW disagree at {step}: {} samples",
                            samples.len()
                        ));
                    }
                    previous = frame.info.sequence;
                    witness.published_sequences.push(previous);
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(format!("timing repaint {step} timeout"));
                }
                if options.mode == ThreadMode::Integrated {
                    let started = Instant::now();
                    runtime
                        .pump()
                        .map_err(|e| format!("timed manual pump: {e:?}"))?;
                    let elapsed = u64::try_from(started.elapsed().as_nanos())
                        .map_err(|_| "manual pump clock overflow")?;
                    witness.manual_pump_calls += 1;
                    if witness.manual_pump_samples_ns.len() < MANUAL_PUMP_SAMPLE_CAPACITY {
                        witness.manual_pump_samples_ns.push(elapsed);
                    }
                }
                thread::sleep(Duration::from_millis(3));
            }
        }
        witness.rearm_refused = matches!(
            runtime.probe_arm_view_copy_timing(browser),
            Err(EngineError::InvalidRequest)
        );
        Ok(())
    }

    struct SlowHttpFixture {
        url: String,
        seen: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }
    impl SlowHttpFixture {
        fn new() -> std::io::Result<Self> {
            use std::io::{BufRead, BufReader, Read};
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            listener.set_nonblocking(true)?;
            let url = format!("http://{}/slow", listener.local_addr()?);
            let seen = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let (observed, stopped) = (seen.clone(), stop.clone());
            let worker = thread::spawn(move || {
                while !stopped.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                            let mut line = Vec::new();
                            let read = BufReader::new((&mut stream).take(1024))
                                .read_until(b'\n', &mut line);
                            if read.is_ok()
                                && matches!(
                                    line.as_slice(),
                                    b"GET /slow HTTP/1.1\r\n" | b"GET /slow HTTP/1.0\r\n"
                                )
                            {
                                observed.store(true, Ordering::Release);
                                // Deliberately send no HTTP response until the
                                // real Browser has been closed by the scenario.
                                while !stopped.load(Ordering::Acquire) {
                                    thread::sleep(Duration::from_millis(2));
                                }
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2))
                        }
                        Err(_) => break,
                    }
                }
            });
            Ok(Self {
                url,
                seen,
                stop,
                worker: Some(worker),
            })
        }
    }
    impl Drop for SlowHttpFixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn native(options: &ProbeOptions, report: &mut ProbeReport) -> i32 {
        let host = match std::env::current_exe() {
            Ok(host) => host,
            Err(error) => {
                report.error = Some(error.to_string());
                return 1;
            }
        };
        let early_d3d11 =
            options.backend == RenderPreference::D3d11Required && !options.scenario.needs_frame();
        let observations = Arc::new(Observations {
            expected_creation_cancellation: options.scenario == ProbeScenario::Creating,
            track_early_close: early_d3d11,
            track_pressure_close_events: options.scenario.has_command_pressure()
                || options.scenario.has_profile_flush()
                || matches!(
                    options.scenario,
                    ProbeScenario::SourceVisitorDrain | ProbeScenario::MenuFindIsolation
                ),
            expected_create_error: match options.scenario {
                ProbeScenario::CreatePostRejected => Some(EngineError::PostRejected),
                ProbeScenario::DeviceCreateFailure => Some(EngineError::Capture {
                    kind: CaptureFailureKind::DeviceCreation,
                    code: None,
                }),
                _ => None,
            },
            expected_capture_fault: (options.scenario == ProbeScenario::CaptureFailureRecovery)
                .then_some(EngineError::Capture {
                    kind: if options.backend == RenderPreference::D3d11Required {
                        CaptureFailureKind::IncompleteFrame
                    } else {
                        CaptureFailureKind::InvalidDimensions
                    },
                    code: None,
                }),
            frame_validation: options.scenario == ProbeScenario::Frames,
            track_ux_events: options.scenario == ProbeScenario::DesktopUx,
            ..Default::default()
        });
        let slow_http = if options.scenario == ProbeScenario::Loading {
            match SlowHttpFixture::new() {
                Ok(fixture) => Some(fixture),
                Err(error) => {
                    report.error = Some(format!("HTTP fixture: {error}"));
                    return 1;
                }
            }
        } else {
            None
        };
        let js_dir_fixture = if options.scenario == ProbeScenario::JsBridgeSchemes {
            match crate::js_bridge_schemes::DirFixture::create() {
                Ok(fixture) => Some(fixture),
                Err(error) => {
                    report.error = Some(format!("dir fixture: {error}"));
                    return 1;
                }
            }
        } else {
            None
        };
        let js_interceptor = (options.scenario == ProbeScenario::JsBridgeSchemes)
            .then(|| Arc::new(crate::js_bridge_schemes::ProbeInterceptor::default()));
        let hostcap_fixture = if options.scenario == ProbeScenario::HostCapabilities {
            match crate::host_capabilities::HttpFixture::start() {
                Ok(fixture) => Some(fixture),
                Err(error) => {
                    report.error = Some(format!("hostcap fixture: {error}"));
                    return 1;
                }
            }
        } else {
            None
        };
        let osr_fixture = if options.scenario == ProbeScenario::OsrParity {
            match crate::osr_parity::OsrHttpFixture::start() {
                Ok(fixture) => Some(fixture),
                Err(error) => {
                    report.error = Some(format!("osr_parity fixture: {error}"));
                    return 1;
                }
            }
        } else {
            None
        };
        let native_fixture = if options.scenario == ProbeScenario::NativeChildHwnd {
            match crate::native_child_hwnd::NativeHttpFixture::start() {
                Ok(fixture) => Some(fixture),
                Err(error) => {
                    report.error = Some(format!("native_child_hwnd fixture: {error}"));
                    return 1;
                }
            }
        } else {
            None
        };
        let mut osr_shutdown_ticket: Option<rust_cef_runtime::UrlRequestTicket> = None;
        let mut handles = Vec::new();
        let mut flush_case = None;
        let mut mailbox_smoke = None;
        let mut frame_validation = None;
        let mut menu_lease_owner = None;
        let mut menu_bridge_registration = None;
        let mut menu_bridge_inbox = None;
        let mut edit_history = (options.scenario == ProbeScenario::MenuEditHistory)
            .then(crate::menu_edit_history::EditHistory::default);
        let mut spelling_probe = (options.scenario == ProbeScenario::MenuSpelling)
            .then(crate::spelling_validation::SpellingProbe::new);
        let mut find_probe = (options.scenario == ProbeScenario::MenuFind)
            .then(crate::find_validation::FindProbe::new);
        let mut find_isolation = (options.scenario == ProbeScenario::MenuFindIsolation)
            .then(crate::find_isolation::FindIsolationProbe::new);
        let mut joint_find = None;
        let mut source_probe = match options.scenario {
            ProbeScenario::MenuSource | ProbeScenario::MenuSourceBudget => {
                Some(crate::source_validation::SourceProbe::new(
                    options.scenario == ProbeScenario::MenuSourceBudget,
                ))
            }
            scenario if scenario.has_source_visitor_drain() => {
                Some(crate::source_validation::SourceProbe::with_held_visitor())
            }
            _ => None,
        };
        let request_cancel_thread = Arc::new(AtomicU64::new(0));
        let config = RuntimeConfig {
            thread_mode: options.mode,
            files: RuntimeFiles {
                directory: host.parent().unwrap().to_path_buf(),
                helper_executable: options.helper.clone(),
            },
            root_cache_path: options.report.parent().unwrap().join("cache"),
            default_profile: ProfileStorage::Memory,
            content_policy: TrustedContentPolicy {
                allowed_origins: if options.scenario == ProbeScenario::JsBridgeSchemes {
                    vec!["app://local".into(), "dir://assets".into()]
                } else if let Some(fixture) = hostcap_fixture.as_ref() {
                    vec![fixture.origin.clone()]
                } else if let Some(fixture) = osr_fixture.as_ref() {
                    vec![fixture.origin.clone()]
                } else if let Some(fixture) = native_fixture.as_ref() {
                    vec![fixture.origin.clone()]
                } else {
                    slow_http
                        .as_ref()
                        .map(|fixture| fixture.url.trim_end_matches("/slow").to_string())
                        .into_iter()
                        .collect()
                },
                allow_embedded_data: true,
            },
            settings: if options.scenario == ProbeScenario::DesktopUx {
                crate::desktop_ux::runtime_settings()
            } else if options.scenario == ProbeScenario::JsBridgeSchemes {
                crate::js_bridge_schemes::runtime_settings()
            } else {
                Default::default()
            },
        };
        let services = RuntimeServices {
            events: observations.clone(),
            frames: observations.clone(),
            ui: observations.clone(),
        };
        let restart_config = config.clone();
        let mut runtime = match Runtime::start(config, services) {
            Ok(runtime) => runtime,
            Err(error) => {
                // No further CEF calls here, including shutdown. If native init
                // returned false, this executable writes its report and exits.
                report.error = Some(format!("start: {error:?}"));
                return match error {
                    StartError::ProcessExitRequired { exit_code } => exit_code.unwrap_or(1),
                    _ => 1,
                };
            }
        };
        let scenario = (|| -> Result<(), String> {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !runtime.is_ready() {
                if Instant::now() >= deadline {
                    return Err("ContextReady timeout".into());
                }
                tick(&mut runtime, options.mode)?;
            }
            if early_d3d11 {
                // Observe a live Required capture before starting the target.
                // No waits for target paint or external inspection after the
                // early Creating/Loading close condition has been reached.
                handles.push(
                    runtime
                        .create_browser(frame_fixture_config(options.backend))
                        .map_err(|error| {
                            format!("create early-close witness Browser: {error:?}")
                        })?,
                );
                report.early_close_backend = Some(observe_early_backend(
                    &mut runtime,
                    options,
                    &observations,
                    &handles[0],
                    deadline,
                )?);
                let released = wait_for_observer(&mut runtime, options)?;
                report
                    .early_close_backend
                    .as_mut()
                    .unwrap()
                    .observer_released = released;
                if !released {
                    return Err(
                        "early-close witness requires external child image observation".into(),
                    );
                }
            }
            if options.scenario == ProbeScenario::JsBridgeSchemes {
                runtime
                    .set_resource_interceptor(Some(js_interceptor.as_ref().unwrap().clone()))
                    .map_err(|e| format!("set interceptor: {e:?}"))?;
                let ticket = runtime
                    .register_profile_scheme_handler(
                        runtime.default_profile_handle(),
                        js_dir_fixture.as_ref().unwrap().dir_binding(),
                    )
                    .map_err(|e| format!("register dir scheme: {e:?}"))?;
                loop {
                    match ticket.poll() {
                        Some(Ok(())) => break,
                        Some(Err(e)) => return Err(format!("dir scheme barrier failed: {e:?}")),
                        None => {}
                    }
                    if Instant::now() >= deadline {
                        return Err("dir scheme registration timeout".into());
                    }
                    tick(&mut runtime, options.mode)?;
                }
            }
            if options.scenario.has_profile_flush() {
                flush_case = Some(crate::profile_flush_validation::FlushCase::open(
                    &mut runtime,
                    &restart_config.root_cache_path,
                    options.mode,
                    tick,
                )?);
            }
            for index in 0..options.scenario.browser_count() {
                let config = BrowserConfig {
                    initial_url: if options.scenario.has_event_pressure() && index == 0 {
                        if options.scenario.has_joint_title_pressure() {
                            PRESSURE_SOURCE_FIXTURE_URL.into()
                        } else {
                            PRESSURE_FIXTURE_URL.into()
                        }
                    } else if options.scenario == ProbeScenario::MenuSpelling {
                        crate::spelling_validation::fixture_url()
                    } else if options.scenario == ProbeScenario::MenuEditHistory {
                        crate::menu_edit_history::fixture_url()
                    } else if options.scenario == ProbeScenario::DesktopUx {
                        crate::desktop_ux::fixture_url()
                    } else if options.scenario == ProbeScenario::JsBridgeSchemes {
                        crate::js_bridge_schemes::initial_url(index)
                    } else if let Some(fixture) = hostcap_fixture.as_ref() {
                        crate::host_capabilities::initial_url(fixture, index)
                    } else if let Some(fixture) = osr_fixture.as_ref() {
                        crate::osr_parity::initial_url(fixture, index)
                    } else if matches!(
                        options.scenario,
                        ProbeScenario::MenuBridge
                            | ProbeScenario::MenuFind
                            | ProbeScenario::MenuFindIsolation
                            | ProbeScenario::MenuSource
                            | ProbeScenario::MenuSourceBudget
                            | ProbeScenario::SourceVisitorDrain
                            | ProbeScenario::SourceProfileFlushDrain
                            | ProbeScenario::SourceFindProfileDrain
                            | ProbeScenario::TitleSourceFindProfileDrain
                            | ProbeScenario::ManagedPressureSourceFindDrain
                            | ProbeScenario::ManagedPressureSourceFindDrainAux
                    ) {
                        rust_cef_runtime::menu_bridge_fixture::fixture_url()
                    } else if options.scenario == ProbeScenario::Frames {
                        crate::capture_validation::fixture_url()
                    } else {
                        slow_http
                            .as_ref()
                            .map(|fixture| fixture.url.clone())
                            .unwrap_or_else(|| FRAME_FIXTURE.into())
                    },
                    profile: flush_case
                        .as_ref()
                        .filter(|_| index == 0)
                        .map(|case| case.named()),
                    render_preference: options.backend,
                    viewport: Viewport {
                        width: 320,
                        height: 240,
                        scale: 1.0,
                    },
                    options: if options.scenario == ProbeScenario::DesktopUx {
                        crate::desktop_ux::browser_options()
                    } else if options.scenario == ProbeScenario::JsBridgeSchemes {
                        crate::js_bridge_schemes::browser_options(index)
                    } else if options.scenario == ProbeScenario::HostCapabilities {
                        crate::host_capabilities::browser_options(index)
                    } else if options.scenario == ProbeScenario::OsrParity {
                        crate::osr_parity::browser_options(index)
                    } else {
                        Default::default()
                    },
                };
                if options.scenario == ProbeScenario::CreatePostRejected {
                    let rejection = runtime.probe_create_with_refused_post(config.clone());
                    if !matches!(
                        rejection,
                        Err(rust_cef_runtime::runtime::BrowserCreateError::PostFailed(
                            EngineError::PostRejected
                        ))
                    ) {
                        if let Ok(handle) = rejection {
                            handles.push(handle);
                        }
                        return Err("expected create-post rejection".into());
                    }
                    let (browser, sequence) = observations
                        .rejected_create
                        .lock()
                        .unwrap()
                        .ok_or("missing CreateFailed(PostRejected) event")?;
                    let source = runtime
                        .frame_source(browser)
                        .map_err(|error| format!("rejected source: {error:?}"))?;
                    report.create_post_rejection = Some(CreatePostRejection {
                        rejected_browser: (browser.runtime().value(), browser.local()),
                        failed_event_sequence: sequence,
                        rejected_frame_source_closed: source.snapshot().closed,
                        recovered_browser: None,
                    });
                    if !source.snapshot().closed
                        || observations.created.load(Ordering::Acquire) != 0
                    {
                        return Err(
                            "refused creation retained a live browser or frame source".into()
                        );
                    }
                }
                if options.scenario == ProbeScenario::DeviceCreateFailure {
                    let rejected = runtime
                        .probe_create_with_device_failure(config.clone())
                        .map_err(|error| format!("device probe admission: {error:?}"))?;
                    let browser = rejected.snapshot().browser;
                    *observations.forbidden_frame_browser.lock().unwrap() = Some(browser);
                    let source = runtime
                        .frame_source(browser)
                        .map_err(|error| format!("device probe source: {error:?}"))?;
                    let sequence = loop {
                        if let Some(error) = observations.error.lock().unwrap().clone() {
                            return Err(error);
                        }
                        if let Some((failed, sequence)) =
                            *observations.rejected_create.lock().unwrap()
                        {
                            if failed != browser {
                                return Err("device failure for a different browser".into());
                            }
                            break sequence;
                        }
                        if rejected.snapshot().state == SessionState::Alive
                            || Instant::now() >= deadline
                        {
                            return Err("expected rejected D3d11 creation".into());
                        }
                        tick(&mut runtime, options.mode)?;
                    };
                    let witness = DeviceCreateFailure {
                        rejected_browser: (browser.runtime().value(), browser.local()),
                        failed_event_sequence: sequence,
                        rejected_frame_source_closed: source.snapshot().closed,
                        capture: runtime
                            .frame_capture_diagnostics(browser)
                            .map_err(|error| format!("rejected capture diagnostics: {error:?}"))?,
                        recovered_browser: None,
                    };
                    let proven = witness.rejected_without_fallback();
                    report.device_create_failure = Some(witness);
                    if !proven
                        || rejected.snapshot().state != SessionState::CreateFailed
                        || observations.created.load(Ordering::Acquire) != 0
                        || observations.frames.load(Ordering::Acquire) != 0
                        || runtime.observed_backend().is_some()
                    {
                        return Err(
                            "device failure did not retire the original instance without fallback"
                                .into(),
                        );
                    }
                }
                if options.scenario == ProbeScenario::LateCreateClose {
                    let late = runtime
                        .probe_create_then_close(config.clone())
                        .map_err(|error| format!("late create admission: {error:?}"))?;
                    let browser = late.snapshot().browser;
                    *observations.forbidden_frame_browser.lock().unwrap() = Some(browser);
                    let source = runtime
                        .frame_source(browser)
                        .map_err(|error| format!("late source: {error:?}"))?;
                    handles.push(late);
                    loop {
                        let state = handles[0].snapshot().state;
                        let callback_observed = runtime.probe_late_create_observed(browser);
                        report.late_create_close = Some(LateCreateClose {
                            late_browser: (browser.runtime().value(), browser.local()),
                            callback_after_accepted_close_intent: callback_observed,
                            frame_source_closed: source.snapshot().closed,
                            frames_before_recovery: observations.frames.load(Ordering::Acquire),
                            recovered_browser: None,
                        });
                        if state == SessionState::Closed
                            && observations.closed.load(Ordering::Acquire) == 1
                        {
                            let witness = report.late_create_close.as_ref().unwrap();
                            if !callback_observed
                                || !witness.frame_source_closed
                                || witness.frames_before_recovery != 0
                                || observations.created.load(Ordering::Acquire) != 1
                            {
                                return Err("inconsistent late creation and retirement".into());
                            }
                            break;
                        }
                        if let Some(error) = observations.error.lock().unwrap().clone() {
                            return Err(error);
                        }
                        if matches!(state, SessionState::Alive | SessionState::CreateFailed)
                            || Instant::now() >= deadline
                        {
                            return Err(format!(
                                "accepted close-before-callback not observed: {state:?}"
                            ));
                        }
                        tick(&mut runtime, options.mode)?;
                    }
                }
                let handle = if options.scenario == ProbeScenario::Frames {
                    runtime.probe_create_capture_browser(config)
                } else {
                    runtime.create_browser(config)
                };
                let handle = handle.map_err(|error| format!("create: {error:?}"))?;
                if let Some(witness) = report.early_close_backend.as_mut() {
                    witness.target_browser = Some(browser_identity(handle.snapshot().browser));
                }
                handles.push(handle);
            }
            if options.scenario == ProbeScenario::Creating {
                return Ok(());
            }
            if options.scenario == ProbeScenario::Frames {
                frame_validation = Some(crate::capture_validation::run(
                    &mut runtime,
                    options.mode,
                    &handles[0],
                    options.report.parent().unwrap(),
                    &observations.expected_frame_fault,
                )?);
            }
            if let Some(fixture) = &slow_http {
                while observations.created.load(Ordering::Acquire) != handles.len() as u64
                    || !fixture.seen.load(Ordering::Acquire)
                {
                    if Instant::now() >= deadline {
                        return Err("HTTP loading observation timeout".into());
                    }
                    tick(&mut runtime, options.mode)?;
                }
                report.loading_request_observed = true;
                return Ok(());
            }
            let expected_creates = options.scenario.browser_count()
                + u64::from(options.scenario == ProbeScenario::LateCreateClose);
            while observations.created.load(Ordering::Acquire) != expected_creates
                || observations.valid_browsers.lock().unwrap().len() as u64
                    != options.scenario.browser_count()
            {
                if let Some(error) = observations.error.lock().unwrap().clone() {
                    return Err(error);
                }
                if Instant::now() >= deadline {
                    return Err("Created/fixture BGRA frame timeout".into());
                }
                tick(&mut runtime, options.mode)?;
            }
            if let Some(witness) = report.create_post_rejection.as_mut() {
                let browser = handles[0].snapshot().browser;
                if !observations
                    .valid_browsers
                    .lock()
                    .unwrap()
                    .contains(&browser)
                {
                    return Err("recovery frame did not belong to the newly created browser".into());
                }
                witness.recovered_browser = Some((browser.runtime().value(), browser.local()));
            }
            if let Some(witness) = report.device_create_failure.as_mut() {
                let browser = handles[0].snapshot().browser;
                if !observations
                    .valid_browsers
                    .lock()
                    .unwrap()
                    .contains(&browser)
                {
                    return Err("D3d11 recovery frame for a different browser".into());
                }
                witness.recovered_browser = Some((browser.runtime().value(), browser.local()));
            }
            if let Some(witness) = report.late_create_close.as_mut() {
                let browser = handles.last().unwrap().snapshot().browser;
                if !observations
                    .valid_browsers
                    .lock()
                    .unwrap()
                    .contains(&browser)
                {
                    return Err("recovery frame arrived under the wrong browser identity".into());
                }
                witness.recovered_browser = Some((browser.runtime().value(), browser.local()));
            }
            if options.scenario == ProbeScenario::Smoke {
                mailbox_smoke = Some(FrameMailboxSmoke::observe(
                    &runtime,
                    handles[0].snapshot().browser,
                )?);
            }
            if options.scenario == ProbeScenario::PendingRequest {
                let observed_thread = request_cancel_thread.clone();
                runtime
                    .inject_pending_request(handles[0].snapshot().browser, move || {
                        observed_thread.store(
                            cef::get_current_platform_thread_id() as u64,
                            Ordering::Release,
                        );
                    })
                    .map_err(|error| format!("pending request: {error:?}"))?;
                if runtime.diagnostics().pending_requests != 1 {
                    return Err("injected request not retained".into());
                }
            }
            if options.scenario == ProbeScenario::CloseTimeout {
                runtime
                    .set_close_deferred(handles[0].snapshot().browser, true)
                    .map_err(|error| format!("defer close: {error:?}"))?;
            }
            // Preserve the real Browser for external OS-child inspection.
            let observer_released = wait_for_observer(&mut runtime, options)?;
            if options.scenario == ProbeScenario::CaptureTiming {
                measure_capture_timing(
                    &mut runtime,
                    options,
                    &handles[0],
                    &observations,
                    report,
                    deadline,
                    observer_released,
                )?;
            }
            if let Some(case) = flush_case.as_mut() {
                if !observer_released {
                    return Err("profile flush probe requires observed child images".into());
                }
                if options.scenario == ProbeScenario::ProfileFlushLateCleanup {
                    let marker = case.begin_original(&runtime)?;
                    if marker.callback_thread != 0 || marker.faulted || marker.release_reserved {
                        return Err(
                            "late-cleanup cut requires an admitted flush before its callback"
                                .into(),
                        );
                    }
                    report.late_flush_cleanup = Some(LateFlushCleanup {
                        profile: (marker.profile.runtime().value(), marker.profile.local()),
                        sequence: marker.sequence,
                        admitted_before_forced_failure: true,
                        ..Default::default()
                    });
                    // Integrated has not pumped since the real BeginCookieFlush
                    // post. Preserve this first error even if cleanup succeeds.
                    return Err("test-induced failure before original cookie flush callback".into());
                }
                if options.scenario == ProbeScenario::PressureProfileFlushDrain
                    || options.scenario.has_joint_source_flush()
                {
                    case.hold_original(
                        &mut runtime,
                        &handles,
                        options.mode,
                        tick,
                        Instant::now() + Duration::from_secs(20),
                        &mut report.profile_flush_drain,
                    )?;
                } else {
                    case.close_first(
                        &mut runtime,
                        &handles,
                        &observations.pressure_close_events,
                        options.mode,
                        tick,
                        &mut report.profile_flush_drain,
                    )?;
                }
            }
            if options.scenario.has_command_pressure() && options.scenario.has_event_pressure() {
                saturate_event_lane(&mut runtime, options, &handles[0], &observations)?;
            }
            if options.scenario.has_command_pressure() {
                let held_flush = if options.scenario == ProbeScenario::PressureProfileFlushDrain {
                    let case = flush_case
                        .as_ref()
                        .ok_or("joint pressure flush fixture missing")?;
                    let witness = report
                        .profile_flush_drain
                        .as_ref()
                        .ok_or("joint pressure flush witness missing")?;
                    Some((case.named(), witness.sequence))
                } else {
                    None
                };
                run_command_pressure_menu_close(
                    &mut runtime,
                    options,
                    &handles,
                    &observations,
                    report,
                    held_flush,
                )?;
                if held_flush.is_some() {
                    let case = flush_case
                        .as_ref()
                        .ok_or("joint pressure flush owner missing")?;
                    let pressure = report
                        .command_pressure_menu_close
                        .as_ref()
                        .ok_or("joint pressure close witness missing")?;
                    let flush = report
                        .profile_flush_drain
                        .as_mut()
                        .ok_or("joint profile flush witness missing")?;
                    case.after_pressured_close(&runtime, &handles[0], pressure, flush)?;
                }
            }
            if options.scenario == ProbeScenario::RetiredFrameReplay {
                let witness =
                    run_retired_frame_replay(&mut runtime, &options, &handles, &observations)?;
                let complete = witness.complete();
                report.retired_frame_replay = Some(witness);
                if !complete {
                    return Err("retired frame replay witness incomplete".into());
                }
            }
            if options.scenario == ProbeScenario::CaptureFailureRecovery {
                report.capture_failure_recovery = Some(run_capture_fault(
                    &mut runtime,
                    &options,
                    &handles[0],
                    &observations,
                )?);
            }
            if options.scenario == ProbeScenario::MenuDevTools {
                let proof = report.menu_devtools.insert(Default::default());
                crate::menu_devtools::exercise(
                    &mut runtime,
                    options.mode,
                    tick,
                    &handles[0],
                    proof,
                )?;
            }
            if options.scenario == ProbeScenario::AuxDevTools {
                let proof = report.aux_devtools.insert(Default::default());
                crate::aux_devtools::exercise(
                    &mut runtime,
                    options.mode,
                    tick,
                    &handles[0],
                    proof,
                )?;
            }
            if options.scenario == ProbeScenario::PrintDrainFence {
                crate::print_fence::exercise(
                    &mut runtime,
                    options.mode,
                    tick,
                    &handles,
                    report.print_fence.insert(Default::default()),
                )?;
            }
            if let Some(probe) = edit_history.as_mut() {
                probe.run(&mut runtime, options.mode, &handles[0], tick)?;
            }
            if options.scenario == ProbeScenario::DesktopUx {
                report.desktop_ux = Some(crate::desktop_ux::run(
                    &mut runtime,
                    options.mode,
                    &handles[0],
                    tick,
                    &observations.ux_events,
                )?);
            }
            if options.scenario == ProbeScenario::JsBridgeSchemes {
                report.js_bridge_schemes = Some(crate::js_bridge_schemes::run(
                    &mut runtime,
                    options.mode,
                    &handles,
                    tick,
                    js_interceptor.as_ref().unwrap(),
                )?);
            }
            if options.scenario == ProbeScenario::HostCapabilities {
                report.host_capabilities = Some(crate::host_capabilities::run(
                    &mut runtime,
                    options.mode,
                    &handles,
                    tick,
                    hostcap_fixture.as_ref().unwrap(),
                    &restart_config.root_cache_path,
                    &observations.policy_denials,
                )?);
            }
            if options.scenario == ProbeScenario::OsrParity {
                let (parity_report, ticket) = crate::osr_parity::run(
                    &mut runtime,
                    options.mode,
                    &handles,
                    tick,
                    osr_fixture.as_ref().unwrap(),
                    &observations.outbound_drags,
                    &observations.policy_denials,
                )?;
                report.osr_parity = Some(parity_report);
                osr_shutdown_ticket = Some(ticket);
            }
            if options.scenario == ProbeScenario::NativeChildHwnd {
                report.native_child_hwnd = Some(crate::native_child_hwnd::run(
                    &mut runtime,
                    options.mode,
                    options.backend,
                    &handles[0],
                    tick,
                    native_fixture.as_ref().unwrap(),
                    options.report.parent().unwrap(),
                    &observations.frames,
                    &observations.valid_browsers,
                    &observations.policy_denials,
                )?);
            }
            if let Some(probe) = spelling_probe.as_mut() {
                probe.run(&mut runtime, options.mode, &handles[0], tick)?;
            }
            if let Some(probe) = find_probe.as_mut() {
                probe.run(&mut runtime, options.mode, &handles[0], tick)?;
            }
            if let Some(probe) = find_isolation.as_mut() {
                probe.run(
                    &mut runtime,
                    options.mode,
                    &handles,
                    &observations.pressure_close_events,
                    tick,
                )?;
            }
            if let Some(probe) = source_probe.as_mut() {
                let mut managed_cut = None;
                let mut unanswered_menu = None;
                if options.scenario.has_joint_find() {
                    use std::num::NonZeroUsize;
                    crate::find_validation::FindProbe::configure(
                        &mut runtime,
                        NonZeroUsize::new(2).unwrap(),
                    )?;
                    probe.run_configured(&mut runtime, options.mode, &handles[0], tick)?;
                    let marker = flush_case
                        .as_ref()
                        .and_then(|case| case.original_marker(&runtime))
                        .ok_or("joint Find missing original flush identity")?;
                    let mut find = crate::joint_find::JointFind::start(
                        &mut runtime,
                        options.mode,
                        &handles[0],
                        &handles[1],
                        marker,
                        tick,
                    )?;
                    if options.scenario.has_joint_title_pressure() {
                        saturate_event_lane(&mut runtime, options, &handles[0], &observations)?;
                        if matches!(
                            options.scenario,
                            ProbeScenario::ManagedPressureSourceFindDrain
                                | ProbeScenario::ManagedPressureSourceFindDrainAux
                        ) {
                            let menu = probe.hold_unanswered_menu(
                                &mut runtime,
                                options.mode,
                                &handles[0],
                                tick,
                            )?;
                            managed_cut = Some(find.managed_cut(menu.presentation.request));
                            unanswered_menu = Some(menu);
                        }
                        find.before_first_close_under_titles(&runtime, &handles[0], &handles[1])?;
                    }
                    joint_find = Some(find);
                } else {
                    probe.run(&mut runtime, options.mode, &handles[0], tick)?;
                }
                if options.scenario.has_source_visitor_drain() {
                    let source = observe_first_close_with_source_held(
                        &mut runtime,
                        options,
                        probe,
                        &handles,
                        &observations,
                        managed_cut,
                    )?;
                    let mut source = source;
                    if options.scenario.holds_aux_devtools() {
                        let survivor = handles[1].snapshot().browser;
                        runtime
                            .close_dev_tools(survivor)
                            .map_err(|e| format!("combined cut close_dev_tools: {e:?}"))?;
                        let until = Instant::now() + Duration::from_secs(20);
                        loop {
                            if runtime.devtools_associated_after_close(survivor) == Some(false)
                                && runtime.devtools_pending(survivor) == 0
                            {
                                break;
                            }
                            if Instant::now() >= until {
                                return Err(
                                    "combined cut auxiliary never retired before shutdown".into()
                                );
                            }
                            tick(&mut runtime, options.mode)?;
                        }
                        source.aux_pending_before_shutdown =
                            Some(runtime.devtools_pending(survivor));
                        source.aux_retired_before_shutdown =
                            Some(runtime.devtools_associated_after_close(survivor) == Some(false));
                    }
                    if let Some(menu) = unanswered_menu {
                        report.managed_pressure_drain = Some(probe.managed_close(
                            &runtime,
                            &handles[0],
                            &source.closed_events,
                            menu,
                        )?);
                    }
                    let apply_flush = if options.scenario.has_joint_source_flush() {
                        let case = flush_case.as_ref().ok_or("missing joint flush fixture")?;
                        let flush = report
                            .profile_flush_drain
                            .as_mut()
                            .ok_or("missing joint flush witness")?;
                        case.after_source_close(&runtime, &handles[0], &source, flush)
                    } else {
                        Ok(())
                    };
                    report.source_visitor_drain = Some(source);
                    apply_flush?;
                    if let Some(find) = joint_find.as_mut() {
                        find.after_first_close(&runtime, &handles[0], &handles[1])?;
                    }
                }
            }
            if options.scenario == ProbeScenario::MenuBridge {
                use rust_cef_core::{
                    menu::{ContextMenuResponse, MenuLimits},
                    menu_context::MenuContextLimits,
                    menu_lease::MenuLeaseOwner,
                    ports::{BrowserCommand, ContextMenuTarget, WindowBinding},
                };
                let browser = handles[0].snapshot().browser;
                let nz = |n| std::num::NonZeroUsize::new(n).unwrap();
                runtime
                    .configure_context_menus(rust_cef_runtime::ContextMenuConfig {
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
                    .map_err(|e| format!("configure public menu: {e:?}"))?;
                let owner = MenuLeaseOwner::new(
                    WindowBinding {
                        browser,
                        window: browser.local(),
                    },
                    nz(1),
                )
                .map_err(|e| format!("fixture menu owner: {e:?}"))?;
                let inbox =
                    rust_cef_runtime::menu_bridge_fixture::MenuBridgeFixture::new(owner.lease());
                let registration = runtime
                    .bind_context_menu(ContextMenuTarget {
                        lease: owner.lease(),
                        delivery: inbox.clone(),
                    })
                    .map_err(|e| format!("bind public menu: {e:?}"))?;
                menu_bridge_registration = Some(registration);
                let registration = menu_bridge_registration.as_ref().unwrap();
                menu_bridge_inbox = Some(inbox.clone());
                menu_lease_owner = Some(owner);
                let outcome =
                    report
                        .menu_bridge
                        .insert(rust_cef_runtime::test_support::MenuBridgeReport {
                            binding_metadata_only: true,
                            ..Default::default()
                        });
                let mut old = None;
                for opening in 1..=3 {
                    outcome.phase = opening as u8;
                    runtime
                        .probe_menu_bridge_open(browser, false)
                        .map_err(|e| format!("bridge fixture input: {e:?}"))?;
                    let deadline = Instant::now() + Duration::from_secs(20);
                    let delivery = loop {
                        if let Some(e) = registration.status().first_failure {
                            return Err(format!("bridge capture: {e}"));
                        }
                        if let Some(d) = inbox.take() {
                            break d;
                        }
                        if Instant::now() >= deadline {
                            return Err("bridge owned delivery timeout".into());
                        }
                        tick(&mut runtime, options.mode)?;
                    };
                    // Delivery may be observed before RunContextMenu's guard
                    // returns on Threaded. Wait for that existing callback,
                    // without another input/post or a new deadline.
                    while runtime.diagnostics().pending_requests != 1 {
                        if runtime.diagnostics().pending_requests == 0 {
                            return Err("captured menu lost before response".into());
                        }
                        if Instant::now() >= deadline {
                            return Err("menu capture callback did not return".into());
                        }
                        tick(&mut runtime, options.mode)?;
                    }
                    let id = delivery.presentation.request;
                    if id.sequence != opening {
                        return Err("bridge request sequence mismatch".into());
                    }
                    if opening == 1 {
                        delivery
                            .response
                            .submit(
                                id,
                                ContextMenuResponse::Select {
                                    command_id: cef::MenuId::SELECT_ALL.get_raw(),
                                },
                            )
                            .map_err(|e| format!("bridge SelectAll post: {e:?}"))?;
                        old = Some((id, delivery.response.clone()));
                        let deadline = Instant::now() + Duration::from_secs(20);
                        while registration.status().actions_submitted != 1
                            || inbox.snapshot().terminal != 1
                            || handles[0].snapshot().navigation.title != "menu-bridge-selected"
                        {
                            if let Some(e) = registration.status().first_failure {
                                return Err(format!("bridge selection: {e}"));
                            }
                            if Instant::now() >= deadline {
                                return Err(
                                    "original-frame SelectAll acknowledgement timeout".into()
                                );
                            }
                            tick(&mut runtime, options.mode)?;
                        }
                    } else if opening == 2 {
                        outcome.selection_observed = delivery.presentation.context.selection.trim()
                            == "menu bridge selection";
                        if !outcome.selection_observed {
                            return Err(
                                "actual CEF context did not observe original frame SelectAll"
                                    .into(),
                            );
                        }
                        menu_lease_owner
                            .as_mut()
                            .unwrap()
                            .invalidate()
                            .map_err(|e| format!("fixture invalidation: {e:?}"))?;
                        outcome.stale_selection_rejected = delivery
                            .response
                            .submit(
                                id,
                                ContextMenuResponse::Select {
                                    command_id: cef::MenuId::SELECT_ALL.get_raw(),
                                },
                            )
                            .is_err();
                        let (old_id, old_response) = old.as_ref().unwrap();
                        outcome.old_id_rejected = old_response
                            .submit(*old_id, ContextMenuResponse::Cancel)
                            .is_err();
                        if !outcome.stale_selection_rejected
                            || !outcome.old_id_rejected
                            || runtime.diagnostics().pending_requests != 1
                        {
                            return Err("stale/old response affected current menu".into());
                        }
                        delivery
                            .response
                            .submit(id, ContextMenuResponse::Cancel)
                            .map_err(|e| format!("stale opening cleanup: {e:?}"))?;
                        let deadline = Instant::now() + Duration::from_secs(20);
                        while inbox.snapshot().terminal != 2 {
                            if Instant::now() >= deadline {
                                return Err("bridge cleanup timeout".into());
                            }
                            tick(&mut runtime, options.mode)?;
                        }
                    } else {
                        handles[0]
                            .submit(BrowserCommand::Reload)
                            .map_err(|e| format!("native navigation cutpoint: {e:?}"))?;
                        let deadline = Instant::now() + Duration::from_secs(20);
                        while inbox.snapshot().terminal != 3
                            || registration.status().invalidated < 2
                            || handles[0].snapshot().navigation.is_loading
                            || handles[0].snapshot().navigation.title != "menu-bridge-ready"
                        {
                            if Instant::now() >= deadline {
                                return Err("native navigation did not retire original menu".into());
                            }
                            tick(&mut runtime, options.mode)?;
                        }
                        outcome.native_navigation_cancelled = true;
                    }
                }
                if registration.status().first_failure.is_some() {
                    return Err("unexpected bridge failure before missing-provider negative".into());
                }
                outcome.phase = 4;
                runtime
                    .probe_menu_bridge_open(browser, true)
                    .map_err(|e| format!("missing required capability input: {e:?}"))?;
                let deadline = Instant::now() + Duration::from_secs(20);
                while registration.status().cancelled != 4
                    || registration.status().native_dismissed != 4
                    || runtime.diagnostics().pending_requests != 0
                {
                    if Instant::now() >= deadline {
                        return Err("missing required host capability not cancelled".into());
                    }
                    tick(&mut runtime, options.mode)?;
                }
                let status = registration.status();
                outcome.missing_managed_capability_cancelled = status.first_failure.as_deref()
                    == Some("missing required host capability for menu command 131")
                    && status.published == 3
                    && status.actions_submitted == 1;
                if !outcome.missing_managed_capability_cancelled {
                    return Err(format!(
                        "wrong first capability diagnostic: {:?}",
                        status.first_failure
                    ));
                }
                outcome.fixture = inbox.snapshot();
                outcome.status = status;
                outcome.phase = 5;
                menu_lease_owner
                    .as_mut()
                    .unwrap()
                    .retire()
                    .map_err(|e| format!("retire bridge fixture owner: {e:?}"))?;
            }
            if options.scenario == ProbeScenario::MenuDeferred {
                use rust_cef_core::{menu_lease::MenuLeaseOwner, ports::WindowBinding};
                let browser = handles[0].snapshot().browser;
                let owner = MenuLeaseOwner::new(
                    WindowBinding {
                        browser,
                        window: browser.local(),
                    },
                    std::num::NonZeroUsize::new(1).unwrap(),
                )
                .map_err(|e| format!("menu fixture lease: {e:?}"))?;
                runtime
                    .probe_menu_deferred_enable(browser, owner.lease())
                    .map_err(|e| format!("deferred fixture enable: {e:?}"))?;
                menu_lease_owner = Some(owner);
                for count in 1..=3 {
                    runtime
                        .probe_menu_open(browser)
                        .map_err(|e| format!("deferred menu input post: {e:?}"))?;
                    let deadline = Instant::now() + Duration::from_secs(20);
                    loop {
                        if let Some(s) = runtime.probe_menu_status(browser) {
                            if let Some(e) = s.error.as_ref() {
                                return Err(format!("deferred capture: {e}"));
                            }
                            if s.copies == count
                                && s.pending_continuations == 1
                                && s.cancel_returns == count - 1
                                && s.answering.is_none()
                            {
                                report
                                    .menu_pending_witnesses
                                    .push(runtime.diagnostics().pending_requests);
                                report.menu_probe = Some(s);
                                break;
                            }
                        }
                        if Instant::now() >= deadline {
                            return Err(format!("deferred opening {count} timeout"));
                        }
                        tick(&mut runtime, options.mode)?;
                    }
                    if count == 3 {
                        break;
                    }
                    runtime
                        .probe_menu_cancel(browser)
                        .map_err(|e| format!("deferred reply post: {e:?}"))?;
                    report.menu_explicit_cancel_posts += 1;
                    let deadline = Instant::now() + Duration::from_secs(20);
                    loop {
                        if let Some(s) = runtime.probe_menu_status(browser) {
                            if let Some(e) = s.error.as_ref() {
                                return Err(format!("deferred answer: {e}"));
                            }
                            if s.complete(count) && s.pending_continuations == 0 {
                                report.menu_probe = Some(s);
                                break;
                            }
                        }
                        if Instant::now() >= deadline {
                            return Err(format!("deferred reply {count} timeout"));
                        }
                        tick(&mut runtime, options.mode)?;
                    }
                }
                let owner = menu_lease_owner.as_mut().unwrap();
                owner
                    .retire()
                    .map_err(|e| format!("retire menu fixture lease: {e:?}"))?;
                report.menu_retired_lease_with_pending = owner
                    .lease()
                    .snapshot()
                    .map_err(|e| format!("retired lease: {e:?}"))?
                    .is_drained()
                    && runtime.diagnostics().pending_requests == 1;
                if !report.menu_retired_lease_with_pending {
                    return Err("lease drain was confused with pending native callback".into());
                }
            }
            if options.scenario == ProbeScenario::Menu {
                let browser = handles[0].snapshot().browser;
                for count in 1..=2 {
                    runtime
                        .probe_menu_open(browser)
                        .map_err(|e| format!("menu input post: {e:?}"))?;
                    let deadline = Instant::now() + Duration::from_secs(20);
                    loop {
                        if let Some(s) = runtime.probe_menu_status(browser) {
                            report.menu_probe = Some(s.clone());
                            if let Some(error) = s.error.as_ref() {
                                return Err(format!("native menu probe: {error}"));
                            }
                            if s.complete(count) {
                                break;
                            }
                        }
                        if Instant::now() >= deadline {
                            return Err(format!("native menu capture/cancel {count} timeout"));
                        }
                        tick(&mut runtime, options.mode)?;
                    }
                }
            }
            // Submit only after external observation. Otherwise the observer
            // wait drains the work and a historical peak proves no shutdown race.
            if options.scenario == ProbeScenario::InFlight {
                for _ in 0..256 {
                    handles[0]
                        .submit(rust_cef_runtime::ports::BrowserCommand::SetFocus(true))
                        .map_err(|error| format!("queued command: {error:?}"))?;
                    report.in_flight_peak = report
                        .in_flight_peak
                        .max(runtime.diagnostics().tasks.pending);
                }
            }
            Ok(())
        })();
        if let Err(error) = scenario {
            report.error = Some(error);
        }
        let frame_probe_evidence = if options.scenario == ProbeScenario::Frames {
            handles
                .first()
                .and_then(|handle| runtime.probe_frame_state(handle.snapshot().browser))
        } else {
            None
        };
        let shutdown_deadline = if options.scenario == ProbeScenario::CloseTimeout {
            Instant::now()
        } else {
            Instant::now() + Duration::from_secs(30)
        };
        if let Some(witness) = report.early_close_backend.as_mut()
            && witness.target_browser.is_some()
            && let Some(target) = handles.last()
        {
            // A sample immediately before request_shutdown, not an atomic
            // statement about a competing Threaded creation callback.
            witness.target_before_drain = match target.snapshot().state {
                SessionState::Creating => Some("Creating"),
                SessionState::Alive => Some("Alive"),
                _ => None,
            };
        }
        if let Some(proof) = report.aux_devtools.as_mut() {
            // A sample immediately before request_shutdown, not an atomic claim.
            proof.pending_before_shutdown = runtime.devtools_pending(handles[0].snapshot().browser);
        }
        let ticket = runtime.request_shutdown(CloseMode::Force, shutdown_deadline);
        if options.scenario == ProbeScenario::CloseTimeout && report.error.is_none() {
            let browser = handles[0].snapshot().browser;
            let identity =
                |id: rust_cef_runtime::contracts::BrowserId| (id.runtime().value(), id.local());
            let poll_open_browsers = match runtime.poll_shutdown(&ticket) {
                ShutdownStatus::Blocked { open_browsers, .. } => {
                    Some(open_browsers.into_iter().map(identity).collect())
                }
                _ => None,
            };
            let finish_open_browsers = match runtime.finish_shutdown() {
                Err(ShutdownError::Blocked { open_browsers, .. }) => {
                    Some(open_browsers.into_iter().map(identity).collect())
                }
                _ => None,
            };
            let witness = CloseTimeoutWitness {
                browser: identity(browser),
                poll_open_browsers,
                finish_open_browsers,
                shutdown_calls_before_release: runtime.diagnostics().cef_shutdown_calls,
                closed_events_before_release: observations.closed.load(Ordering::Acquire),
                browser_closing_before_release: handles[0].snapshot().state
                    == SessionState::Closing,
            };
            report.timeout_blocked_observed = witness.complete();
            report.close_timeout = Some(witness);
            if !report.timeout_blocked_observed {
                report.error =
                    Some("deadline did not retain the original unfinished browser".into());
            }
            if let Err(error) = runtime.set_close_deferred(browser, false) {
                report
                    .error
                    .get_or_insert(format!("release deferred close: {error:?}"));
            }
        }
        let joint_source_flush = options.scenario.has_joint_source_flush();
        let source_owner = handles
            .first()
            .filter(|_| options.scenario.has_source_visitor_drain())
            .map(|h| h.snapshot().browser);
        let mut source_release = OwnedRelease::default();
        let mut flush_release = OwnedRelease::default();
        if options.scenario.has_source_visitor_drain() {
            let flush = flush_case
                .as_ref()
                .zip(report.profile_flush_drain.as_ref())
                .map(|(case, w)| (case.named(), w.sequence));
            if report.error.is_none() && joint_source_flush && flush.is_none() {
                report.error = Some("joint barrier missing the original flush identity".into());
            }
            if report.error.is_none()
                && let Some(w) = report.source_visitor_drain.as_mut()
                && let Err(e) =
                    observe_source_shutdown_barrier(&mut runtime, &ticket, options.mode, w, flush)
            {
                report.error = Some(e);
            }
            if report.error.is_none() && options.scenario.has_joint_find() {
                let closed = observations.pressure_close_events.lock().unwrap().clone();
                let find = joint_find
                    .as_ref()
                    .ok_or_else(|| "joint Find missing before release".to_owned())
                    .and_then(|f| f.after_survivor_close(&runtime, &handles[1], closed));
                match find {
                    Ok(w) => report.joint_find_drain = Some(w),
                    Err(e) => report.error = Some(e),
                }
            }
            if let Some(owner) = source_owner {
                release_source_once(&runtime, owner, &mut source_release, report);
            }
        }
        if let Some(case) = &flush_case {
            if report.error.is_none()
                && let Some(witness) = report.profile_flush_drain.as_mut()
                && let Err(error) =
                    case.prove_barrier(&mut runtime, &ticket, options.mode, tick, witness)
            {
                report.error = Some(error);
            }
            if joint_source_flush && report.error.is_none() {
                let d = runtime.source_view_diagnostics();
                let retired = d.started == 1
                    && d.finished == 1
                    && d.active.is_empty()
                    && d.pending == 0
                    && d.native_live == 0
                    && d.host_live == 0
                    && !d.faulted
                    && runtime.probe_held_source_visitor().is_none();
                if let Some(w) = report.source_visitor_drain.as_mut() {
                    w.source_retired_before_flush_release = Some(retired);
                }
                if !retired {
                    report.error =
                        Some("original Source visitor not retired before flush release".into());
                }
            }
            if let Some(w) = report.late_flush_cleanup.as_mut() {
                w.callback_thread_at_first_cleanup =
                    case.original_marker(&runtime).map(|m| m.callback_thread);
            }
            if let Err(error) = case.release_once(
                &runtime,
                &mut flush_release,
                report.profile_flush_drain.as_mut(),
                report.late_flush_cleanup.as_mut(),
            ) {
                report.error.get_or_insert(error);
            }
            if let Some(w) = report.late_flush_cleanup.as_mut() {
                w.attempted_before_drain = flush_release.attempted();
            }
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut finished = false;
        loop {
            // Only the failed test host observes a hold that was absent at the
            // first sample, inside its existing drain loop. No second actual
            // post is possible, even when a reservation was rolled back.
            if report.error.is_some() && Instant::now() < deadline {
                if let Some(owner) = source_owner {
                    release_source_once(&runtime, owner, &mut source_release, report);
                }
                if let Some(case) = &flush_case
                    && let Err(error) = case.release_once(
                        &runtime,
                        &mut flush_release,
                        report.profile_flush_drain.as_mut(),
                        report.late_flush_cleanup.as_mut(),
                    )
                {
                    report.error.get_or_insert(error);
                }
            }
            if let Some(probe) = &source_probe {
                if let Err(e) = probe.flush_close() {
                    report.error.get_or_insert(e);
                }
            }
            match runtime.poll_shutdown(&ticket) {
                ShutdownStatus::Ready => {
                    match runtime.finish_shutdown() {
                        Ok(_) => {
                            finished = true;
                            if let Some(hc) = report.host_capabilities.as_mut() {
                                hc.cdp_active_after_shutdown =
                                    runtime.cdp_diagnostics().active_registrations;
                            }
                            if let Some(op) = report.osr_parity.as_mut() {
                                op.pending_url_requests_after_shutdown =
                                    runtime.url_request_pending_count();
                                op.url_request_shutdown_drain_ok =
                                    osr_shutdown_ticket.as_ref().is_some_and(|t| {
                                        matches!(
                                            t.poll(),
                                            rust_cef_runtime::UrlRequestState::Failed(
                                                rust_cef_core::url_request::UrlRequestError::Cancelled
                                            )
                                        )
                                    }) && op.pending_url_requests_after_shutdown == 0;
                            }
                        }
                        Err(error) => {
                            report
                                .error
                                .get_or_insert(format!("finish_shutdown: {error:?}"));
                        }
                    }
                    break;
                }
                ShutdownStatus::Stopped => break,
                ShutdownStatus::ProcessExitRequired { .. } => {
                    report
                        .error
                        .get_or_insert("shutdown refused: process exit required".into());
                    break;
                }
                ShutdownStatus::NotRequested
                | ShutdownStatus::Draining { .. }
                | ShutdownStatus::Blocked { .. } => {}
            }
            if Instant::now() >= deadline {
                report
                    .error
                    .get_or_insert("shutdown blocked: no early CEF shutdown authorized".into());
                break;
            }
            if let Err(error) = tick(&mut runtime, options.mode) {
                report.error.get_or_insert(error);
                break;
            }
        }
        if finished {
            let before = runtime.diagnostics().native_calls;
            if runtime.pump().is_err() {
                report.post_shutdown_rejections += 1;
            } else {
                report
                    .error
                    .get_or_insert("pump accepted after shutdown".into());
            }
            for handle in &handles {
                if handle
                    .submit(rust_cef_runtime::ports::BrowserCommand::SetFocus(true))
                    .is_err()
                {
                    report.post_shutdown_rejections += 1;
                } else {
                    report
                        .error
                        .get_or_insert("command accepted after shutdown".into());
                }
            }
            if runtime.finish_shutdown().is_ok() {
                report
                    .error
                    .get_or_insert("second shutdown accepted".into());
            }
            match Runtime::start(
                restart_config,
                RuntimeServices {
                    events: observations.clone(),
                    frames: observations.clone(),
                    ui: observations.clone(),
                },
            ) {
                Err(StartError::AlreadyActive) => report.post_shutdown_rejections += 1,
                Err(error) => {
                    report
                        .error
                        .get_or_insert(format!("restart did not reject at once-token: {error:?}"));
                }
                Ok(unexpected) => {
                    std::mem::forget(unexpected);
                    report
                        .error
                        .get_or_insert("same-process CEF restart accepted".into());
                }
            }
            if runtime.diagnostics().native_calls != before {
                report
                    .error
                    .get_or_insert("rejected calls entered native API after shutdown".into());
            }
        }
        if finished
            && let [_, b] = handles.as_slice()
            && let Some(capture) = report
                .command_pressure_menu_close
                .as_mut()
                .and_then(|witness| witness.accelerated_capture.as_mut())
                .or_else(|| {
                    report
                        .source_visitor_drain
                        .as_mut()?
                        .accelerated_capture
                        .as_mut()
                })
        {
            capture.survivor_retired = runtime.frame_capture_diagnostics(b.snapshot().browser).ok();
            if !runtime
                .diagnostics()
                .cef_ui_thread_id
                .is_some_and(|ui| capture.complete(ui))
            {
                report.error.get_or_insert(
                    "pressured Browser D3d11 capture/retirement witness incomplete".into(),
                );
            }
        }
        if let Some(witness) = report.capture_timing.as_mut() {
            let browser = handles[0].snapshot().browser;
            witness.copy_samples = runtime.probe_view_copy_samples(browser).unwrap_or_default();
            witness.copy_ns =
                TimingDistribution::of(witness.copy_samples.iter().map(|s| s.copy_ns));
            witness.manual_pump_ns =
                TimingDistribution::of(witness.manual_pump_samples_ns.iter().copied());
            if finished {
                witness.source_closed = runtime.frame_source(browser).is_ok_and(|source| {
                    let snapshot = source.snapshot();
                    snapshot.closed
                        && snapshot.view.frame.is_none()
                        && snapshot.popup.frame.is_none()
                });
                if let Ok(stats) = runtime.frame_budget_stats(browser) {
                    witness.final_cpu_bytes = Some(stats.used);
                    witness.peak_cpu_bytes = Some(stats.peak);
                    witness.cpu_limit_bytes = Some(stats.limit);
                }
                witness.retired_capture = runtime.frame_capture_diagnostics(browser).ok();
            }
        }
        if let Some(witness) = report.early_close_backend.as_mut() {
            witness.closed_events = observations.early_closed.lock().unwrap().clone();
            witness.cancelled_creation = *observations.early_cancelled.lock().unwrap();
            let source_closed = |id| {
                runtime.frame_source(id).is_ok_and(|source| {
                    let snapshot = source.snapshot();
                    snapshot.closed
                        && snapshot.view.frame.is_none()
                        && snapshot.popup.frame.is_none()
                })
            };
            if finished && let [backend, target] = handles.as_slice() {
                let backend = backend.snapshot().browser;
                let target = target.snapshot().browser;
                witness.backend_source_closed = source_closed(backend)
                    && runtime
                        .frame_budget_stats(backend)
                        .is_ok_and(|stats| stats.used == 0);
                witness.retired_capture = runtime.frame_capture_diagnostics(backend).ok();
                witness.target_source_closed = source_closed(target)
                    && runtime
                        .frame_budget_stats(target)
                        .is_ok_and(|stats| stats.used == 0);
            }
        }
        if finished && let Some(witness) = report.capture_failure_recovery.as_mut() {
            let browser = handles[0].snapshot().browser;
            witness.closed_source = runtime.frame_source(browser).is_ok_and(|source| {
                let snapshot = source.snapshot();
                snapshot.closed && snapshot.view.frame.is_none() && snapshot.popup.frame.is_none()
            });
            witness.retired_capture = runtime.frame_capture_diagnostics(browser).ok();
            witness.final_cpu_bytes = runtime
                .frame_budget_stats(browser)
                .ok()
                .map(|stats| stats.used);
            if !witness.complete() {
                report.error.get_or_insert(
                    "capture fault classification or post-close retirement mismatch".into(),
                );
            }
        }
        if options.scenario == ProbeScenario::Smoke {
            let witness = match (finished, mailbox_smoke) {
                (true, Some(witness)) => witness.finish(&runtime),
                _ => Err("native shutdown or first FrameSource witness incomplete".into()),
            };
            let value = match witness {
                Ok(value) => value,
                Err(error) => {
                    report.error.get_or_insert(error.clone());
                    serde_json::json!({"schema":2,"kind":"frame-mailbox-capture-smoke","success":false,"error":error})
                }
            };
            if let Err(error) = std::fs::write(
                options.report.with_file_name("frame-mailbox.json"),
                serde_json::to_vec_pretty(&value).unwrap(),
            ) {
                report
                    .error
                    .get_or_insert(format!("frame mailbox evidence write: {error}"));
            }
        }
        if options.scenario == ProbeScenario::Frames {
            if let Some(error) = observations.error.lock().unwrap().clone() {
                report.error.get_or_insert(error);
            }
            let mut value = frame_validation.unwrap_or_else(
                || serde_json::json!({"success":false,"error":"frame validation did not finish"}),
            );
            let final_probe = handles
                .first()
                .and_then(|handle| runtime.probe_frame_state(handle.snapshot().browser))
                .or(frame_probe_evidence);
            value["native_rejection_observations"] = serde_json::to_value(final_probe).unwrap();
            value["incomplete_capture_events"] = serde_json::to_value(
                observations
                    .incomplete_capture_events
                    .lock()
                    .unwrap()
                    .clone(),
            )
            .unwrap();
            if let Some(handle) = handles.first() {
                value["browser"] = format!("{:?}", handle.snapshot().browser).into();
            }
            value["cef_ui_thread_id"] = serde_json::json!(runtime.diagnostics().cef_ui_thread_id);
            let expected_events = observations
                .expected_frame_faults_seen
                .load(Ordering::Acquire);
            value["expected_resize_exhausted_events"] = expected_events.into();
            if expected_events != 1 {
                value["success"] = false.into();
                report.error.get_or_insert(format!(
                    "expected exactly one controlled ResizeExhausted event, got {expected_events}"
                ));
            }
            if finished && !handles.is_empty() {
                let browser = handles[0].snapshot().browser;
                let capture = runtime.frame_capture_diagnostics(browser);
                let recovery = runtime.frame_recovery_diagnostics(browser);
                let budget = runtime.frame_budget_stats(browser);
                match (capture, recovery, budget) {
                    (Ok(capture), Ok(recovery), Ok(budget))
                        if capture.retired
                            && !capture.native_resources_live
                            && !capture.abandoned
                            && capture.maps == capture.unmaps
                            && capture.live_imports == 0
                            && recovery.status == "Closed"
                            && !recovery.queued
                            && budget.used == 0 =>
                    {
                        value["native_close"] = serde_json::json!({"capture":capture,"recovery":recovery,"cpu_used":budget.used,"cpu_peak":budget.peak,"cpu_limit":budget.limit});
                    }
                    other => {
                        value["success"] = false.into();
                        report.error.get_or_insert(format!(
                            "capture/recovery resource retirement failed: {other:?}"
                        ));
                    }
                }
            } else {
                value["success"] = false.into();
                report
                    .error
                    .get_or_insert("frame validation native close incomplete".into());
            }
            let rejection_check = handles
                .first()
                .ok_or_else(|| "missing probe Browser".into())
                .and_then(|handle| {
                    let browser = format!("{:?}", handle.snapshot().browser);
                    crate::rejections::validate(
                        &value,
                        &browser,
                        runtime.diagnostics().cef_ui_thread_id.unwrap_or(0),
                    )
                });
            match rejection_check {
                Ok(proof) => value["native_rejection_accounting"] = proof,
                Err(error) => {
                    report
                        .error
                        .get_or_insert(format!("native rejection accounting: {error}"));
                }
            }
            if let Some(error) = report.error.as_ref() {
                value["success"] = false.into();
                value["error"] = error.clone().into();
            }
            if let Err(error) = std::fs::write(
                options.report.with_file_name("capture-validation.json"),
                serde_json::to_vec_pretty(&value).unwrap(),
            ) {
                report
                    .error
                    .get_or_insert(format!("frame validation evidence: {error}"));
            }
        }
        if options.scenario.has_command_pressure()
            || matches!(
                options.scenario,
                ProbeScenario::Menu | ProbeScenario::MenuDeferred
            )
        {
            if let Some(h) = handles.first() {
                report.menu_probe = runtime
                    .probe_menu_status(h.snapshot().browser)
                    .or(report.menu_probe.take());
            }
        }
        if let Some(witness) = report.command_pressure_menu_close.as_mut() {
            witness.closed_events = observations.pressure_close_events.lock().unwrap().clone();
        }
        if options.scenario.has_event_pressure() {
            report.event_pressure = observations
                .event_pressure
                .lock()
                .unwrap()
                .as_ref()
                .map(PressureEventLane::verdict);
        }
        let diagnostics = runtime.diagnostics();
        report.init_thread_id = Some(diagnostics.init_thread_id);
        report.cef_ui_thread_id = diagnostics
            .cef_ui_thread_id
            .or(diagnostics.context_thread_id);
        report.shutdown_thread_id = diagnostics.shutdown_thread_id;
        if let Some(probe) = &edit_history {
            report.edit_history = Some(probe.snapshot());
        }
        if let Some(probe) = &spelling_probe {
            report.spelling = Some(probe.snapshot());
        }
        if let Some(probe) = &find_probe {
            report.find = Some(probe.snapshot(&runtime));
        }
        if let Some(probe) = &find_isolation {
            report.find_isolation = probe.snapshot(&runtime, &observations.pressure_close_events);
        }
        if let Some(probe) = &source_probe {
            report.source_view = Some(probe.snapshot(&runtime));
        }
        if let Some(witness) = report.source_visitor_drain.as_mut() {
            witness.closed_events = observations.pressure_close_events.lock().unwrap().clone();
            witness.held_reference_gone = finished && runtime.probe_held_source_visitor().is_none();
        }
        if let (Some(case), Some(witness)) = (&flush_case, report.late_flush_cleanup.as_mut()) {
            case.finish_late(&runtime, finished, witness);
        }
        if let (Some(case), Some(witness)) = (&flush_case, report.profile_flush_drain.as_mut()) {
            case.finish(
                &runtime,
                &handles,
                &observations.pressure_close_events,
                finished,
                witness,
            );
        }
        if let (Some(registration), Some(bridge)) =
            (&menu_bridge_registration, report.menu_bridge.as_mut())
        {
            bridge.status = registration.status();
            if let Some(inbox) = &menu_bridge_inbox {
                bridge.fixture = inbox.snapshot();
            }
            bridge.pending_after_close = diagnostics.pending_requests;
        }
        report.app_reference_retained = Some(diagnostics.app_reference_retained);
        report.manual_pump_count = diagnostics.manual_pump_count;
        report.tasks_posted = diagnostics.tasks.posted;
        report.tasks_completed = diagnostics.tasks.completed;
        report.task_post_failures = diagnostics.tasks.post_failed;
        report.pending_tasks = diagnostics.tasks.pending;
        report.pending_tasks_at_shutdown = diagnostics.pending_tasks_at_shutdown;
        report.pending_requests = diagnostics.pending_requests;
        report.cancelled_requests = diagnostics.cancelled_requests;
        let cancel_thread = request_cancel_thread.load(Ordering::Acquire);
        report.request_cancel_thread_id = (cancel_thread != 0).then_some(cancel_thread);
        report.cef_shutdown_calls = diagnostics.cef_shutdown_calls;
        report.native_calls = diagnostics.native_calls;
        report.native_calls_after_shutdown = diagnostics.native_calls_after_shutdown;
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
        if finished && report.error.is_none() {
            0
        } else {
            1
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn creation_cancelled_by_requested_drain_is_not_a_capture_or_backend_failure() {
            let expected = Observations {
                expected_creation_cancellation: true,
                ..Default::default()
            };
            let id = rust_cef_runtime::contracts::BrowserId::new(
                rust_cef_runtime::contracts::RuntimeGeneration::new(1),
                1,
            );
            BrowserEventSink::publish(
                &expected,
                BrowserEventEnvelope {
                    browser: id,
                    sequence: 1,
                    event: BrowserEvent::CreateFailed(EngineError::Closing),
                },
            );
            assert!(expected.error.lock().unwrap().is_none());
            assert_eq!(expected.cancelled_creates.load(Ordering::Acquire), 1);
            BrowserEventSink::publish(
                &expected,
                BrowserEventEnvelope {
                    browser: id,
                    sequence: 2,
                    event: BrowserEvent::CaptureFailed(EngineError::Closing),
                },
            );
            assert!(
                expected.error.lock().unwrap().is_some(),
                "capture failures are never expected cancellations"
            );
            let normal = Observations::default();
            BrowserEventSink::publish(
                &normal,
                BrowserEventEnvelope {
                    browser: id,
                    sequence: 1,
                    event: BrowserEvent::CreateFailed(EngineError::Closing),
                },
            );
            assert!(normal.error.lock().unwrap().is_some());
        }

        #[test]
        fn controlled_exhaust_is_one_shot_and_never_hides_incomplete() {
            use rust_cef_runtime::ports::CaptureFailureKind;
            let browser = rust_cef_runtime::contracts::BrowserId::new(
                rust_cef_runtime::contracts::RuntimeGeneration::new(1),
                1,
            );
            let event = |kind| BrowserEventEnvelope {
                browser,
                sequence: 1,
                event: BrowserEvent::CaptureFailed(EngineError::Capture { kind, code: None }),
            };
            let armed = || {
                let observations = Observations {
                    frame_validation: true,
                    ..Default::default()
                };
                observations
                    .expected_frame_fault
                    .store(true, Ordering::Release);
                observations
            };
            let incomplete = armed();
            BrowserEventSink::publish(&incomplete, event(CaptureFailureKind::IncompleteFrame));
            assert!(incomplete.error.lock().unwrap().is_none());
            assert_eq!(
                incomplete.incomplete_capture_events.lock().unwrap().len(),
                1,
                "the real error must be retained, never phase-whitelisted"
            );
            // Without a matching callback witness, strict accounting fails.
            assert!(crate::rejections::validate(&serde_json::json!({"incomplete_capture_events":*incomplete.incomplete_capture_events.lock().unwrap()}),"B",7).is_err());
            assert_eq!(
                incomplete
                    .expected_frame_faults_seen
                    .load(Ordering::Acquire),
                0
            );
            let exhaust = armed();
            BrowserEventSink::publish(&exhaust, event(CaptureFailureKind::ResizeExhausted));
            assert!(exhaust.error.lock().unwrap().is_none());
            assert!(!exhaust.expected_frame_fault.load(Ordering::Acquire));
            assert_eq!(
                exhaust.expected_frame_faults_seen.load(Ordering::Acquire),
                1
            );
            // Even an accidental re-arm must never whitelist another exhaust.
            exhaust.expected_frame_fault.store(true, Ordering::Release);
            BrowserEventSink::publish(&exhaust, event(CaptureFailureKind::ResizeExhausted));
            assert!(exhaust.error.lock().unwrap().is_some());
            assert_eq!(
                exhaust.expected_frame_faults_seen.load(Ordering::Acquire),
                1
            );
        }

        #[test]
        fn late_capture_failure_cannot_be_hidden_by_ready_frame_and_close() {
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
                    .join("report.json")
                    .to_string_lossy()
                    .into_owned(),
            ])
            .unwrap();
            let mut report = ProbeReport::started(&options, 1);
            report.actual_mode = Some("Threaded");
            report.actual_backend = Some("Software");
            report.init_thread_id = Some(1);
            report.cef_ui_thread_id = Some(2);
            report.shutdown_thread_id = Some(1);
            report.app_reference_retained = Some(false);
            report.exit_code = Some(0);
            report.tasks_posted = 2;
            report.tasks_completed = 2;
            report.cef_shutdown_calls = 1;
            report.native_calls = 1;
            report.post_shutdown_rejections = 2;
            let observations = Observations::default();
            observations.created.store(1, Ordering::Release);
            observations.closed.store(1, Ordering::Release);
            observations.frames.store(1, Ordering::Release);
            observations.width.store(320, Ordering::Release);
            observations.height.store(240, Ordering::Release);
            observations.valid_browsers.lock().unwrap().insert(
                rust_cef_runtime::contracts::BrowserId::new(
                    rust_cef_runtime::contracts::RuntimeGeneration::new(1),
                    1,
                ),
            );
            observations.apply_to(&mut report);
            assert!(
                report.native_success(),
                "baseline must be a complete success before late error"
            );
            BrowserEventSink::publish(
                &observations,
                BrowserEventEnvelope {
                    browser: rust_cef_runtime::contracts::BrowserId::new(
                        rust_cef_runtime::contracts::RuntimeGeneration::new(1),
                        1,
                    ),
                    sequence: 3,
                    event: BrowserEvent::CaptureFailed(EngineError::Backend { code: Some(37) }),
                },
            );
            observations.apply_to(&mut report);
            assert!(report.error.is_some());
            assert!(!report.native_success());
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
        let mut report =
            ProbeReport::started(&options, cef::get_current_platform_thread_id() as u64);
        let code = native(&options, &mut report);
        report.exit_code = Some(code);
        let code = if code == 0 && report.error.is_none() && !report.native_success() {
            report.error = Some("incomplete or inconsistent native evidence".into());
            report.exit_code = Some(1);
            1
        } else {
            code
        };
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
}
fn main() {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    std::process::exit(windows::run());
    #[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
    {
        eprintln!("native probe requires Windows x64 MSVC");
        std::process::exit(2);
    }
}
