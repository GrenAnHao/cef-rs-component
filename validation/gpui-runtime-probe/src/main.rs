//! Minimal GPUI host for the public GPUI-CEF Runtime wiring (task 5.4).
//!
//! Scope: one passive host window, one real 320x240 Required backend browser, the
//! component's capacity-one UI wake adapter, the full all-browser shutdown
//! barrier, and evidence reporting. No CefBrowser product view, menu, input, or
//! CEF subprocess handling lives here.
//!
//! Thread model:
//! - The GPUI main thread calls `Runtime::start`, opens the window, and owns the
//!   Runtime inside a single foreground coroutine. `Runtime` never crosses a
//!   thread; the coroutine is intentionally `!Send`.
//! - Integrated: the coroutine waits on the wake channel or a 4 ms baseline tick,
//!   then calls `runtime.pump()` *outside* every GPUI borrow, then updates the
//!   view with owned data.
//! - Threaded: no manual pump and no pump ticker. Events and frames are consumed
//!   only after the capacity-one channel actually notified the foreground task.
//!   Bounded timers exist only for (a) startup readiness, (b) the external
//!   observer-gate file handshake, and (c) shutdown-barrier rechecks. A timer
//!   firing where a frame notification is required is reported as a failure, so
//!   polling is never presented as Threaded dispatch.

#![cfg_attr(
    not(all(target_os = "windows", target_arch = "x86_64")),
    allow(dead_code)
)]

#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
fn main() {
    eprintln!("gpui-runtime-probe requires Windows x64 MSVC");
    std::process::exit(2);
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn main() {
    windows::main();
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod windows {
    use std::{
        cell::Cell,
        collections::HashSet,
        future::Future,
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        },
        task::Poll,
        time::{Duration, Instant},
    };

    use gpui::{
        App, AppContext as _, AsyncApp, Bounds, Context, Entity, Render, SharedString, Window,
        WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
    };
    use rust_cef_gpui::{
        contracts::{
            BrowserConfig, ProfileStorage, RenderPreference, RuntimeConfig, RuntimeFiles,
            ThreadMode, TrustedContentPolicy, Viewport,
        },
        dispatcher::{self, UiWakeReceiver, UiWakeSender},
        ports::{
            BrowserCommand, BrowserEvent, BrowserEventEnvelope, BrowserEventSink, BrowserHandle,
            CloseMode, EngineError, FrameSink, OwnedFrame, PublishResult, SurfaceKind,
            UiDispatcher,
        },
    };
    use rust_cef_runtime::runtime::{
        Runtime, RuntimeDiagnostics, RuntimeServices, ShutdownStatus, StartError,
    };
    use rust_cef_runtime::test_support::{ProbeOptions, ProbeReport, ProbeScenario};
    use serde::Serialize;

    const FIXTURE_URL: &str = "data:text/html,%3Cbody%20style%3D%22margin%3A0%3Bbackground%3Argb(26%2C76%2C126)%22%3E%3C%2Fbody%3E";
    const WINDOW_TITLE: &str = "GPUI Runtime Probe";
    const SCENARIO_TIMEOUT: Duration = Duration::from_secs(30);
    const OBSERVER_TIMEOUT: Duration = Duration::from_secs(20);
    const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
    /// Integrated pump baseline. This is the hand-driven message pump, not a
    /// frame-delivery fallback; a pending wake shortens the wait.
    const INTEGRATED_TICK: Duration = Duration::from_millis(4);
    /// Bounded recheck, allowed only for startup readiness, the external gate
    /// file, and the shutdown barrier.
    const BOUNDED_RECHECK: Duration = Duration::from_millis(50);

    thread_local! {
        /// Depth of host-owned `App` borrows on this thread. Only our own
        /// update wrappers set it; gateways on other threads see their own
        /// thread-local zero and cannot produce a false violation.
        static APP_BORROW_DEPTH: Cell<u32> = const { Cell::new(0) };
    }

    fn borrow_active() -> bool {
        APP_BORROW_DEPTH.with(|depth| depth.get() != 0)
    }

    struct BorrowScope;

    impl BorrowScope {
        fn enter() -> Self {
            APP_BORROW_DEPTH.with(|depth| depth.set(depth.get() + 1));
            Self
        }
    }

    impl Drop for BorrowScope {
        fn drop(&mut self) {
            APP_BORROW_DEPTH.with(|depth| depth.set(depth.get() - 1));
        }
    }

    /// The only host-side entry into `App`. Every use is bracketed by the
    /// borrow-depth guard, so a manual pump or a finish that ran inside a host
    /// borrow is counted, not hidden.
    fn with_app<R>(cx: &mut AsyncApp, update: impl FnOnce(&mut App) -> R) -> R {
        let _guard = BorrowScope::enter();
        cx.update(update)
    }

    #[derive(Default)]
    struct Counters {
        created: Mutex<HashSet<(u64, u64)>>,
        closed: Mutex<HashSet<(u64, u64)>>,
        valid_frames: Mutex<HashSet<(u64, u64)>>,
        frames: AtomicU64,
        width: AtomicU32,
        height: AtomicU32,
        ffi_to_ui: AtomicU64,
        notify_count: AtomicU64,
        borrow_violation: AtomicU64,
        shutdown_rejections: AtomicU64,
        /// Real Threaded frame-phase notifications actually consumed by the host
        /// coroutine. FFI publishes and `cx.notify()` call counts are different
        /// facts and must not stand in for this.
        frame_notifications_consumed: AtomicU64,
        error: Mutex<Option<String>>,
    }

    impl Counters {
        fn record_ffi(&self) {
            self.ffi_to_ui.fetch_add(1, Ordering::Relaxed);
            if borrow_active() {
                // Publishing from a callback while the host holds an App borrow
                // would mean re-entrant GPUI access. Threaded callbacks on other
                // threads see their own thread-local zero and never trip this.
                self.borrow_violation.fetch_add(1, Ordering::Relaxed);
            }
        }

        fn fail(&self, message: impl Into<String>) {
            let mut error = self.error.lock().unwrap();
            if error.is_none() {
                *error = Some(message.into());
            }
        }

        fn error(&self) -> Option<String> {
            self.error.lock().unwrap().clone()
        }

        fn created_count(&self) -> u64 {
            self.created.lock().unwrap().len() as u64
        }

        fn closed_count(&self) -> u64 {
            self.closed.lock().unwrap().len() as u64
        }

        fn valid_count(&self) -> u64 {
            self.valid_frames.lock().unwrap().len() as u64
        }

        fn summary(&self) -> String {
            format!(
                "created={} closed={} frames={} valid={} {}x{}",
                self.created_count(),
                self.closed_count(),
                self.frames.load(Ordering::Acquire),
                self.valid_count(),
                self.width.load(Ordering::Acquire),
                self.height.load(Ordering::Acquire),
            )
        }
    }

    /// Event sink owned by the host. A newtype is required: the port traits and
    /// `Arc` are both foreign types, so they cannot be implemented directly on
    /// `Arc<Counters>`.
    #[derive(Clone)]
    struct EventSink {
        counters: Arc<Counters>,
    }

    impl BrowserEventSink for EventSink {
        fn publish(&self, envelope: BrowserEventEnvelope) -> PublishResult {
            self.counters.record_ffi();
            let counters = &self.counters;
            let key = (envelope.browser.runtime().value(), envelope.browser.local());
            match envelope.event {
                BrowserEvent::Created => {
                    counters.created.lock().unwrap().insert(key);
                }
                BrowserEvent::Closed => {
                    counters.closed.lock().unwrap().insert(key);
                }
                BrowserEvent::CreateFailed(error) | BrowserEvent::CaptureFailed(error) => {
                    counters.fail(format!("browser event: {error:?}"));
                }
                BrowserEvent::RendererTerminated { code, .. } => {
                    counters.fail(format!("renderer terminated: {code:?}"));
                }
                _ => {}
            }
            PublishResult::Published
        }
    }

    #[derive(Clone)]
    struct FrameSinkImpl {
        counters: Arc<Counters>,
    }

    impl FrameSink for FrameSinkImpl {
        fn publish(&self, frame: OwnedFrame) -> PublishResult {
            self.counters.record_ffi();
            let counters = &self.counters;
            if frame.info.surface != SurfaceKind::View {
                return PublishResult::Rejected;
            }
            counters.frames.fetch_add(1, Ordering::Relaxed);
            let width = frame.info.width;
            let height = frame.info.height;
            // The fixture paints a solid opaque RGB(26,76,126) background.
            // Require its real BGRA center pixel, not just any first paint.
            let center = ((height as usize / 2) * width as usize + width as usize / 2) * 4;
            if width == 320
                && height == 240
                && frame.pixels.get(center..center + 4) == Some(&[126, 76, 26, 255])
            {
                counters.width.store(width, Ordering::Release);
                counters.height.store(height, Ordering::Release);
                counters.valid_frames.lock().unwrap().insert((
                    frame.info.browser.runtime().value(),
                    frame.info.browser.local(),
                ));
            }
            PublishResult::Published
        }
    }

    /// Host-side counting wrapper around the component dispatcher. The component
    /// sender remains the only channel; this wrapper adds evidence counters.
    struct CountingDispatcher {
        wake: UiWakeSender,
        counters: Arc<Counters>,
    }

    impl UiDispatcher for CountingDispatcher {
        fn wake(&self) -> Result<(), EngineError> {
            self.counters.record_ffi();
            self.wake.wake()
        }
    }

    struct ProbeView {
        status: SharedString,
    }

    impl ProbeView {
        fn new() -> Self {
            Self {
                status: "GPUI runtime probe: starting".into(),
            }
        }
    }

    impl Render for ProbeView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .bg(rgb(0x101418))
                .text_color(rgb(0xffffff))
                .p_4()
                .child(self.status.clone())
        }
    }

    enum Tick {
        Notified,
        Timer,
    }

    /// One frame-phase wait. `Err` means the channel closed; `Tick::Timer` means
    /// the watchdog elapsed and must be judged by `accept_frame_tick`.
    async fn wait_next(
        cx: &mut AsyncApp,
        receiver: &UiWakeReceiver,
        timeout: Duration,
    ) -> Result<Tick, String> {
        let timer = cx.background_executor().timer(timeout);
        let mut timer = std::pin::pin!(timer);
        let mut recv = std::pin::pin!(receiver.recv());
        std::future::poll_fn(|context| {
            if let Poll::Ready(notified) = recv.as_mut().poll(context) {
                return Poll::Ready(if notified {
                    Ok(Tick::Notified)
                } else {
                    Err("UI wake channel closed".to_string())
                });
            }
            if timer.as_mut().poll(context).is_ready() {
                return Poll::Ready(Ok(Tick::Timer));
            }
            Poll::Pending
        })
        .await
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Flow {
        Integrated,
        Threaded,
    }

    fn flow_of(mode: ThreadMode) -> Flow {
        match mode {
            ThreadMode::Integrated => Flow::Integrated,
            ThreadMode::Threaded => Flow::Threaded,
        }
    }

    fn update_status(
        cx: &mut AsyncApp,
        view: &Entity<ProbeView>,
        counters: &Counters,
        text: String,
    ) {
        let changed = with_app(cx, |app| {
            view.update(app, |view, cx| {
                if view.status.as_ref() == text.as_str() {
                    return false;
                }
                view.status = SharedString::from(text);
                cx.notify();
                true
            })
        });
        if changed {
            counters.notify_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// One scheduler turn: wait as allowed by the flow, then (Integrated only)
    /// pump on the host thread with no GPUI borrow, then refresh the window with
    /// owned data only.
    async fn step(
        cx: &mut AsyncApp,
        runtime: &mut Runtime,
        receiver: &UiWakeReceiver,
        counters: &Arc<Counters>,
        view: &Entity<ProbeView>,
        flow: Flow,
        note: &str,
    ) -> Result<(), String> {
        // The only wait in a scheduler turn. Every `step` site is either the
        // Integrated baseline tick or an explicitly allowed Threaded bounded
        // recheck (startup readiness, observer gate, shutdown barrier).
        let timeout = match flow {
            Flow::Integrated => INTEGRATED_TICK,
            Flow::Threaded => BOUNDED_RECHECK,
        };
        wait_next(cx, receiver, timeout).await?;
        if flow == Flow::Integrated {
            // The CEF message-loop entry: no GPUI borrow may be open here.
            if borrow_active() {
                counters.borrow_violation.fetch_add(1, Ordering::Relaxed);
            }
            runtime.pump().map_err(|error| format!("pump: {error:?}"))?;
        }
        update_status(
            cx,
            view,
            counters,
            format!("{note} | {}", counters.summary()),
        );
        Ok(())
    }

    /// A Threaded frame/creation wait that timed out is a failure, never a
    /// silently polled delivery.
    /// Frame-phase decision for one scheduler wait. Threaded delivery must be
    /// wake-driven: a timer expiry is an unconditional failure in this phase,
    /// even when the owned counters already look complete, so a watchdog can
    /// never be mistaken for a notification.
    fn accept_frame_tick(
        flow: Flow,
        tick: Tick,
        _counters_ready: bool,
        consumed: &AtomicU64,
    ) -> Result<(), String> {
        if flow == Flow::Threaded {
            if matches!(tick, Tick::Timer) {
                return Err(
                    "browser creation/frame: bounded timer expired without a UI wake; Threaded delivery must be wake-driven"
                        .into(),
                );
            }
            consumed.fetch_add(1, Ordering::AcqRel);
        }
        Ok(())
    }

    /// Whether the frame phase may end. Threaded needs completed counters *and*
    /// at least one wake the host actually consumed.
    fn frame_phase_satisfied(flow: Flow, counters_ready: bool, consumed: u64) -> bool {
        match flow {
            Flow::Integrated => counters_ready,
            Flow::Threaded => counters_ready && consumed > 0,
        }
    }

    /// Host-owned evidence gate. Threaded additionally needs a real consumed
    /// wake in the frame phase; FFI arrivals and `cx.notify` counts are not it.
    fn host_evidence_ok(
        flow: Flow,
        ffi_to_ui: u64,
        notify_count: u64,
        borrow_violation: u64,
        frame_notifications: u64,
    ) -> bool {
        ffi_to_ui > 0
            && notify_count > 0
            && borrow_violation == 0
            && (flow == Flow::Integrated || frame_notifications > 0)
    }

    #[derive(Clone, Default)]
    struct Diag {
        main_thread_id: u64,
        init_thread_id: u64,
        context_thread_id: Option<u64>,
        cef_ui_thread_id: Option<u64>,
        shutdown_thread_id: Option<u64>,
        manual_pump_count: u64,
        observed_backend: Option<RenderPreference>,
        app_reference_retained: bool,
        cef_shutdown_calls: u64,
        native_calls: u64,
        native_calls_after_shutdown: u64,
        pending_requests: u64,
        cancelled_requests: u64,
        tasks_posted: u64,
        tasks_completed: u64,
        task_post_failures: u64,
        pending_tasks: u64,
    }

    impl Diag {
        fn of(diagnostics: &RuntimeDiagnostics) -> Self {
            Self {
                main_thread_id: diagnostics.main_thread_id,
                init_thread_id: diagnostics.init_thread_id,
                context_thread_id: diagnostics.context_thread_id,
                cef_ui_thread_id: diagnostics.cef_ui_thread_id,
                shutdown_thread_id: diagnostics.shutdown_thread_id,
                manual_pump_count: diagnostics.manual_pump_count,
                observed_backend: diagnostics.observed_backend,
                app_reference_retained: diagnostics.app_reference_retained,
                cef_shutdown_calls: diagnostics.cef_shutdown_calls,
                native_calls: diagnostics.native_calls,
                native_calls_after_shutdown: diagnostics.native_calls_after_shutdown,
                pending_requests: diagnostics.pending_requests,
                cancelled_requests: diagnostics.cancelled_requests,
                tasks_posted: diagnostics.tasks.posted,
                tasks_completed: diagnostics.tasks.completed,
                task_post_failures: diagnostics.tasks.post_failed,
                pending_tasks: diagnostics.tasks.pending,
            }
        }
    }

    #[derive(Default)]
    struct Outcome {
        exit_code: i32,
        /// Set when `Runtime::start` failed before any CEF initialize; no CEF
        /// call including shutdown may follow, so the host exits directly.
        fatal_start: bool,
        error: Option<String>,
        diag: Option<Diag>,
        finished: bool,
    }

    #[derive(Serialize)]
    struct HostReport {
        #[serde(flatten)]
        probe: ProbeReport,
        kind: &'static str,
        notify_count: u64,
        ffi_to_ui: u64,
        borrow_violation: u64,
        /// Threaded frame-phase wake-ups the host coroutine actually consumed.
        /// This is not the FFI arrival count and not the `cx.notify` count.
        frame_notifications_consumed: u64,
    }

    fn write_report(path: &PathBuf, report: HostReport) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(&report).map_err(|error| error.to_string())?;
        std::fs::write(path, bytes).map_err(|error| format!("report {}: {error}", path.display()))
    }

    struct Scenario {
        diag: Diag,
        result: Result<(), String>,
        finished: bool,
    }

    /// Everything after the window and Runtime exist. Always drives the full
    /// all-browser barrier, then proves the public handle is refused afterwards.
    async fn run_scenario(
        cx: &mut AsyncApp,
        mut runtime: Runtime,
        receiver: UiWakeReceiver,
        counters: Arc<Counters>,
        view: Entity<ProbeView>,
        backend: RenderPreference,
        observer_gate: Option<PathBuf>,
    ) -> Scenario {
        let flow = flow_of(runtime.mode());
        let mut browser = None;
        let mut result = scenario_body(
            cx,
            &mut runtime,
            &receiver,
            &counters,
            &view,
            flow,
            backend,
            observer_gate.as_ref(),
            &mut browser,
        )
        .await;
        if let Err(error) = &result {
            counters.fail(error.clone());
        }

        let started = Instant::now();
        let ticket = runtime.request_shutdown(CloseMode::Force, Instant::now() + SHUTDOWN_TIMEOUT);
        let mut finished = false;
        let mut last_blocked: Option<String> = None;
        loop {
            let mut finish_attempt = false;
            match runtime.poll_shutdown(&ticket) {
                ShutdownStatus::Ready => finish_attempt = true,
                ShutdownStatus::Stopped => {
                    finished = true;
                    break;
                }
                ShutdownStatus::ProcessExitRequired { exit_code } => {
                    result = Err(format!(
                        "CEF initialize required process exit: {exit_code:?}"
                    ));
                    break;
                }
                ShutdownStatus::NotRequested => {
                    result = Err("shutdown ticket was not accepted".into());
                    break;
                }
                ShutdownStatus::Draining { .. } | ShutdownStatus::Blocked { .. } => {}
            }
            if finish_attempt {
                if borrow_active() {
                    counters.borrow_violation.fetch_add(1, Ordering::Relaxed);
                }
                match runtime.finish_shutdown() {
                    Ok(_) => {
                        finished = true;
                        break;
                    }
                    // Readiness is a snapshot; the real native table can still
                    // be momentarily busy. Keep waiting instead of aborting.
                    Err(error) => last_blocked = Some(format!("finish_shutdown: {error:?}")),
                }
            }
            if started.elapsed() >= SHUTDOWN_TIMEOUT {
                result = Err(match last_blocked.as_deref() {
                    Some(detail) => format!(
                        "{detail}; no early CEF shutdown authorized after the host deadline"
                    ),
                    None => {
                        "shutdown blocked past the host deadline; no early CEF shutdown authorized"
                            .into()
                    }
                });
                break;
            }
            if let Err(error) = step(
                cx,
                &mut runtime,
                &receiver,
                &counters,
                &view,
                flow,
                "draining",
            )
            .await
            {
                result = Err(error);
                break;
            }
        }
        if !finished
            && result.is_ok()
            && let Some(detail) = last_blocked
        {
            result = Err(detail);
        }

        let mut native_before = None;
        if finished {
            // A public handle must refuse work after the barrier without
            // reaching a native entry. Record the audited native call count
            // across the rejected attempt instead of asserting it stays zero.
            let before = runtime.diagnostics().native_calls;
            if let Some(handle) = browser.as_ref() {
                match handle.submit(BrowserCommand::SetFocus(true)) {
                    Err(EngineError::Closed) | Err(EngineError::Closing) => {
                        counters.shutdown_rejections.fetch_add(1, Ordering::Release);
                        native_before = Some(before);
                    }
                    Err(other) => {
                        result = Err(format!("unexpected post-shutdown reject: {other:?}"));
                    }
                    Ok(_) => {
                        result = Err("browser command accepted after shutdown barrier".into());
                    }
                }
            } else {
                result = Err("no browser handle retained for the rejection check".into());
            }
        }
        let diagnostics = runtime.diagnostics();
        if let Some(before) = native_before
            && diagnostics.native_calls != before
        {
            result = Err(format!(
                "rejected post-shutdown command still entered native CEF: {} -> {}",
                before, diagnostics.native_calls
            ));
        }
        let diag = Diag::of(&diagnostics);
        drop(browser.take());
        if finished {
            // Normal completion: release the owner on the main thread; CEF was
            // shut down exactly once and no further call is made here.
            drop(runtime);
        } else {
            // Host failure path: never destroy undrained native owners. The OS
            // reclaims them when this process exits.
            std::mem::forget(runtime);
        }
        Scenario {
            diag,
            result,
            finished,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn scenario_body(
        cx: &mut AsyncApp,
        runtime: &mut Runtime,
        receiver: &UiWakeReceiver,
        counters: &Arc<Counters>,
        view: &Entity<ProbeView>,
        flow: Flow,
        backend: RenderPreference,
        observer_gate: Option<&PathBuf>,
        browser: &mut Option<BrowserHandle>,
    ) -> Result<(), String> {
        let deadline = Instant::now() + SCENARIO_TIMEOUT;
        while !runtime.is_ready() {
            if let Some(error) = counters.error() {
                return Err(error);
            }
            if Instant::now() >= deadline {
                return Err("ContextReady timeout".into());
            }
            // Startup readiness is an allowed bounded recheck in Threaded mode.
            step(cx, runtime, receiver, counters, view, flow, "starting").await?;
        }

        let handle = runtime
            .create_browser(BrowserConfig {
                initial_url: FIXTURE_URL.into(),
                profile: None,
                render_preference: backend,
                viewport: Viewport {
                    width: 320,
                    height: 240,
                    scale: 1.0,
                },
                options: Default::default(),
            })
            .map_err(|error| format!("create_browser: {error:?}"))?;
        *browser = Some(handle);

        loop {
            let counters_ready = counters.created_count() > 0 && counters.valid_count() > 0;
            let consumed = counters
                .frame_notifications_consumed
                .load(Ordering::Acquire);
            if frame_phase_satisfied(flow, counters_ready, consumed) {
                break;
            }
            if let Some(error) = counters.error() {
                return Err(error);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err("Created/fixture BGRA frame timeout".into());
            }
            let remaining = deadline - now;
            match flow {
                Flow::Integrated => {
                    step(cx, runtime, receiver, counters, view, flow, "frame").await?;
                }
                Flow::Threaded => {
                    // One watchdog timer for the whole remaining window. Frames
                    // themselves must arrive through the wake channel: a timer
                    // expiry fails even if the counters already look complete.
                    let counters_ready = counters.created_count() > 0 && counters.valid_count() > 0;
                    let consumed = counters
                        .frame_notifications_consumed
                        .load(Ordering::Acquire);
                    if !frame_phase_satisfied(Flow::Threaded, counters_ready, consumed) {
                        let tick = wait_next(cx, receiver, remaining).await?;
                        accept_frame_tick(
                            Flow::Threaded,
                            tick,
                            counters_ready,
                            &counters.frame_notifications_consumed,
                        )?;
                    }
                    update_status(
                        cx,
                        view,
                        counters,
                        format!("frame | {}", counters.summary()),
                    );
                }
            }
            if let Some(error) = counters.error() {
                return Err(error);
            }
        }

        if let Some(gate) = observer_gate {
            rust_cef_runtime::test_support::publish_observer_ready(gate, std::process::id())
                .map_err(|error| format!("observer ready: {error}"))?;
            let gate_deadline = Instant::now() + OBSERVER_TIMEOUT;
            while !gate.is_file() {
                if Instant::now() >= gate_deadline {
                    return Err("external image observer timeout".into());
                }
                // The release comes from another process's file system, not a
                // CEF callback; a bounded timer is the only option and it never
                // delivers frames or events.
                step(cx, runtime, receiver, counters, view, flow, "observer wait").await?;
            }
        }
        Ok(())
    }

    fn fill_report(report: &mut ProbeReport, counters: &Counters, diag: &Diag, outcome: &Outcome) {
        report.main_thread_id = diag.main_thread_id;
        report.init_thread_id = Some(diag.init_thread_id);
        report.cef_ui_thread_id = diag.cef_ui_thread_id.or(diag.context_thread_id);
        report.shutdown_thread_id = diag.shutdown_thread_id;
        report.app_reference_retained = Some(diag.app_reference_retained);
        report.manual_pump_count = diag.manual_pump_count;
        report.tasks_posted = diag.tasks_posted;
        report.tasks_completed = diag.tasks_completed;
        report.task_post_failures = diag.task_post_failures;
        report.pending_tasks = diag.pending_tasks;
        report.pending_requests = diag.pending_requests;
        report.cancelled_requests = diag.cancelled_requests;
        report.cef_shutdown_calls = diag.cef_shutdown_calls;
        report.native_calls = diag.native_calls;
        report.native_calls_after_shutdown = diag.native_calls_after_shutdown;
        report.post_shutdown_rejections = counters.shutdown_rejections.load(Ordering::Acquire);
        report.created = counters.created_count();
        report.closed = counters.closed_count();
        report.frames = counters.frames.load(Ordering::Acquire);
        report.valid_frame_browsers = counters.valid_count();
        report.first_frame_width = counters.width.load(Ordering::Acquire);
        report.first_frame_height = counters.height.load(Ordering::Acquire);
        report.actual_mode = diag
            .context_thread_id
            .filter(|id| match report.requested_mode {
                "Integrated" => *id == diag.main_thread_id,
                "Threaded" => *id != diag.main_thread_id,
                _ => false,
            })
            .map(|_| report.requested_mode);
        report.actual_backend = match diag.observed_backend {
            Some(RenderPreference::SoftwareRequired) => Some("Software"),
            Some(RenderPreference::D3d11Required) => Some("D3d11"),
            _ => None,
        };
        if let Some(error) = counters.error() {
            report.error.get_or_insert(error);
        }
        if let Some(error) = &outcome.error {
            report.error.get_or_insert_with(|| error.clone());
        }
    }

    /// Aggregates the final exit decision. Kept pure so complete fake evidence
    /// can be tested without native CEF or a GPUI window.
    fn finalize_report(
        report: &mut ProbeReport,
        finished: bool,
        fatal_start: bool,
        exit_code: i32,
        extra_ok: bool,
    ) -> i32 {
        if fatal_start {
            // CEF initialize asked the process to exit before a Runtime existed.
            // Preserve its code exactly; no CEF call may follow on this path.
            report.exit_code = Some(exit_code);
            return exit_code;
        }
        if !(finished && extra_ok) {
            // Host-controlled evidence already failed; do not offer a success
            // candidate to the self-consistency check.
            let code = exit_code.max(1);
            report.exit_code = Some(code);
            if report.error.is_none() {
                report.error =
                    Some("incomplete or inconsistent GPUI runtime evidence (see counters)".into());
            }
            return code;
        }
        // `native_success` requires exit_code Some(0), so the candidate must be
        // installed *before* it is consulted. This is only a candidate: it is
        // replaced by the real failure code when any actual field is missing.
        report.exit_code = Some(0);
        if report.native_success() {
            0
        } else {
            let code = exit_code.max(1);
            report.exit_code = Some(code);
            if report.error.is_none() {
                report.error =
                    Some("incomplete or inconsistent GPUI runtime evidence (see counters)".into());
            }
            code
        }
    }

    struct LaunchArgs {
        options: ProbeOptions,
        counters: Arc<Counters>,
        outcome: Arc<Mutex<Outcome>>,
    }

    fn launch(cx: &mut App, launch: LaunchArgs) {
        let LaunchArgs {
            options,
            counters,
            outcome,
        } = launch;
        let allow_close = Arc::new(AtomicBool::new(false));
        let root = {
            let allow_close = allow_close.clone();
            cx.open_window(
                WindowOptions {
                    focus: false,
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(560.), px(180.)),
                        cx,
                    ))),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some(WINDOW_TITLE.into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                move |window, cx| {
                    window.on_window_should_close(cx, move |_, _| {
                        // A window close must never race ahead of the CEF
                        // shutdown barrier; only this host sets the flag, after
                        // `finish_shutdown` or after a reported failure.
                        allow_close.load(Ordering::Acquire)
                    });
                    cx.new(|_| ProbeView::new())
                },
            )
        };
        let root = match root {
            Ok(root) => root,
            Err(error) => {
                let mut outcome = outcome.lock().unwrap();
                outcome.error = Some(format!("open_window: {error}"));
                cx.quit();
                return;
            }
        };
        let view: Entity<ProbeView> = match root.entity(cx) {
            Ok(view) => view,
            Err(error) => {
                let mut outcome = outcome.lock().unwrap();
                outcome.error = Some(format!("window root entity: {error}"));
                cx.quit();
                return;
            }
        };
        let host = match std::env::current_exe() {
            Ok(host) => host,
            Err(error) => {
                let mut outcome = outcome.lock().unwrap();
                outcome.error = Some(format!("current_exe: {error}"));
                cx.quit();
                return;
            }
        };
        let directory = match host.parent() {
            Some(parent) => parent.to_path_buf(),
            None => {
                let mut outcome = outcome.lock().unwrap();
                outcome.error = Some("host executable has no directory".into());
                cx.quit();
                return;
            }
        };
        let report = options.report.clone();
        let config = RuntimeConfig {
            thread_mode: options.mode,
            files: RuntimeFiles {
                directory,
                helper_executable: options.helper.clone(),
            },
            root_cache_path: report
                .parent()
                .map(|parent| parent.join("cache"))
                .unwrap_or_else(|| std::env::temp_dir().join("gpui-runtime-probe-cache")),
            default_profile: ProfileStorage::Memory,
            content_policy: TrustedContentPolicy {
                allowed_origins: vec![],
                allow_embedded_data: true,
            },
            settings: Default::default(),
        };
        let (wake_sender, receiver) = dispatcher::channel();
        let services = RuntimeServices {
            events: Arc::new(EventSink {
                counters: counters.clone(),
            }),
            frames: Arc::new(FrameSinkImpl {
                counters: counters.clone(),
            }),
            ui: Arc::new(CountingDispatcher {
                wake: wake_sender,
                counters: counters.clone(),
            }),
        };
        let runtime = match Runtime::start(config, services) {
            Ok(runtime) => runtime,
            Err(error) => {
                let mut outcome = outcome.lock().unwrap();
                outcome.fatal_start = true;
                outcome.exit_code = match &error {
                    StartError::ProcessExitRequired { exit_code } => exit_code.unwrap_or(1),
                    _ => 1,
                };
                outcome.error = Some(format!("start: {error:?}"));
                drop(outcome);
                allow_close.store(true, Ordering::Release);
                cx.quit();
                return;
            }
        };
        let gate = options.observer_gate.clone();
        let backend = options.backend;
        let counters_for_task = counters.clone();
        let outcome_for_task = outcome.clone();
        let allow_close_for_task = allow_close.clone();
        cx.spawn(async move |cx: &mut AsyncApp| {
            let scenario = run_scenario(
                cx,
                runtime,
                receiver,
                counters_for_task.clone(),
                view,
                backend,
                gate,
            )
            .await;
            let mut outcome = outcome_for_task.lock().unwrap();
            outcome.diag = Some(scenario.diag);
            outcome.finished = scenario.finished;
            if let Err(error) = scenario.result {
                outcome.error = Some(error);
            }
            drop(outcome);
            // Only after the barrier (or after a failure that left the native
            // owners untouched) may the window close and the app quit.
            allow_close_for_task.store(true, Ordering::Release);
            with_app(cx, |app| app.quit());
        })
        .detach();
    }

    /// Echo of the parsed request, retained after the run loop consumes the
    /// options, so the final report describes what was actually requested.
    struct Requested {
        mode: ThreadMode,
        backend: RenderPreference,
        report: PathBuf,
        helper: PathBuf,
        observer_gate: Option<PathBuf>,
        scenario: ProbeScenario,
    }

    impl Requested {
        fn of(options: &ProbeOptions) -> Self {
            Self {
                mode: options.mode,
                backend: options.backend,
                report: options.report.clone(),
                helper: options.helper.clone(),
                observer_gate: options.observer_gate.clone(),
                scenario: options.scenario,
            }
        }

        fn base_report(&self, main_thread_id: u64) -> ProbeReport {
            ProbeReport::started(
                &ProbeOptions {
                    mode: self.mode,
                    backend: self.backend,
                    report: self.report.clone(),
                    helper: self.helper.clone(),
                    observer_gate: self.observer_gate.clone(),
                    scenario: self.scenario,
                },
                main_thread_id,
            )
        }
    }

    fn write_early_failure(requested: &Requested, message: String, code: i32) -> ! {
        let mut report = requested.base_report(0);
        report.error = Some(message);
        report.exit_code = Some(code);
        let _ = write_report(
            &requested.report,
            HostReport {
                probe: report,
                kind: "gpui",
                notify_count: 0,
                ffi_to_ui: 0,
                borrow_violation: 0,
                frame_notifications_consumed: 0,
            },
        );
        std::process::exit(code);
    }

    pub(crate) fn main() {
        let options = match ProbeOptions::parse(std::env::args().skip(1)) {
            Ok(options) => options,
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(2);
            }
        };
        let requested = Requested::of(&options);
        if requested.scenario != ProbeScenario::Smoke {
            write_early_failure(
                &requested,
                "GPUI runtime probe supports only --scenario Smoke".into(),
                2,
            );
        }
        let counters = Arc::new(Counters::default());
        let outcome = Arc::new(Mutex::new(Outcome::default()));
        {
            let counters = counters.clone();
            let outcome = outcome.clone();
            gpui_platform::application().run(move |cx| {
                launch(
                    cx,
                    LaunchArgs {
                        options,
                        counters,
                        outcome,
                    },
                )
            });
        }

        let outcome = outcome.lock().unwrap();
        let mut report = match outcome.diag.as_ref() {
            Some(diag) => requested.base_report(diag.main_thread_id),
            None => requested.base_report(0),
        };
        if let Some(diag) = outcome.diag.as_ref() {
            fill_report(&mut report, &counters, diag, &outcome);
        }
        if !outcome.finished && outcome.error.is_none() {
            report
                .error
                .get_or_insert_with(|| "shutdown barrier did not complete".into());
        }
        let extra_ok = host_evidence_ok(
            flow_of(requested.mode),
            counters.ffi_to_ui.load(Ordering::Acquire),
            counters.notify_count.load(Ordering::Acquire),
            counters.borrow_violation.load(Ordering::Acquire),
            counters
                .frame_notifications_consumed
                .load(Ordering::Acquire),
        );
        let code = finalize_report(
            &mut report,
            outcome.finished,
            outcome.fatal_start,
            outcome.exit_code,
            extra_ok,
        );
        let host_report = HostReport {
            probe: report,
            kind: "gpui",
            notify_count: counters.notify_count.load(Ordering::Acquire),
            ffi_to_ui: counters.ffi_to_ui.load(Ordering::Acquire),
            borrow_violation: counters.borrow_violation.load(Ordering::Acquire),
            frame_notifications_consumed: counters
                .frame_notifications_consumed
                .load(Ordering::Acquire),
        };
        drop(outcome);
        if let Err(error) = write_report(&requested.report, host_report) {
            eprintln!("{error}");
            std::process::exit(1);
        }
        std::process::exit(code);
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn complete_diag(mode: ThreadMode) -> Diag {
            let main = 100u64;
            Diag {
                main_thread_id: main,
                init_thread_id: main,
                context_thread_id: Some(match mode {
                    ThreadMode::Integrated => main,
                    ThreadMode::Threaded => 200,
                }),
                cef_ui_thread_id: Some(match mode {
                    ThreadMode::Integrated => main,
                    ThreadMode::Threaded => 200,
                }),
                shutdown_thread_id: Some(main),
                manual_pump_count: match mode {
                    ThreadMode::Integrated => 3,
                    ThreadMode::Threaded => 0,
                },
                observed_backend: Some(RenderPreference::SoftwareRequired),
                app_reference_retained: false,
                cef_shutdown_calls: 1,
                native_calls: 12,
                native_calls_after_shutdown: 0,
                pending_requests: 0,
                cancelled_requests: 0,
                tasks_posted: 4,
                tasks_completed: 4,
                task_post_failures: 0,
                pending_tasks: 0,
            }
        }

        /// Fake evidence only: no native CEF, no GPUI window. It proves the
        /// report pipeline reaches `native_success` with every field populated.
        fn complete_counters() -> Counters {
            let counters = Counters::default();
            let key = (1, 1);
            counters.created.lock().unwrap().insert(key);
            counters.closed.lock().unwrap().insert(key);
            counters.valid_frames.lock().unwrap().insert(key);
            counters.frames.store(7, Ordering::Release);
            counters.width.store(320, Ordering::Release);
            counters.height.store(240, Ordering::Release);
            counters.ffi_to_ui.store(9, Ordering::Release);
            counters.notify_count.store(3, Ordering::Release);
            counters.shutdown_rejections.store(1, Ordering::Release);
            counters
        }

        fn requested(mode: ThreadMode) -> Requested {
            Requested {
                mode,
                backend: RenderPreference::SoftwareRequired,
                report: std::env::temp_dir().join("gpui-runtime-probe-unit.json"),
                helper: std::env::temp_dir().join("helper.exe"),
                observer_gate: None,
                scenario: ProbeScenario::Smoke,
            }
        }

        fn assembled(mode: ThreadMode) -> (ProbeReport, Counters, Diag) {
            let counters = complete_counters();
            let diag = complete_diag(mode);
            let outcome = Outcome {
                exit_code: 1,
                fatal_start: false,
                error: None,
                diag: Some(diag.clone()),
                finished: true,
            };
            let mut report = requested(mode).base_report(diag.main_thread_id);
            fill_report(&mut report, &counters, &diag, &outcome);
            (report, counters, diag)
        }

        #[test]
        fn complete_integrated_evidence_decides_success_not_default_failure() {
            let (mut report, _counters, _diag) = assembled(ThreadMode::Integrated);
            assert!(report.error.is_none());
            assert_eq!(
                report.exit_code, None,
                "fill_report must not invent a verdict"
            );
            let code = finalize_report(&mut report, true, false, 1, true);
            assert_eq!(
                report.exit_code,
                Some(0),
                "the exit code must be set before native_success is consulted"
            );
            assert_eq!(code, 0);
            assert!(report.native_success());
        }

        #[test]
        fn complete_threaded_evidence_decides_success_without_manual_pump() {
            let (mut report, _counters, diag) = assembled(ThreadMode::Threaded);
            assert_eq!(diag.manual_pump_count, 0);
            let code = finalize_report(&mut report, true, false, 1, true);
            assert_eq!(code, 0);
            assert_eq!(report.exit_code, Some(0));
            assert!(report.native_success());
        }

        #[test]
        fn missing_evidence_never_reports_success_or_fabricates_zero() {
            let (mut report, _counters, _diag) = assembled(ThreadMode::Integrated);
            // A scenario without a valid frame can never pass, and the fallback
            // must not silently claim exit 0 either.
            report.frames = 0;
            report.valid_frame_browsers = 0;
            let code = finalize_report(&mut report, true, false, 1, true);
            assert_ne!(code, 0);
            assert_eq!(report.exit_code, Some(code));
            assert!(report.error.is_some());
        }

        #[test]
        fn incomplete_host_evidence_fails_even_when_native_fields_look_complete() {
            let (mut report, _counters, _diag) = assembled(ThreadMode::Integrated);
            // No FFI callback/notify means the GPUI wiring never ran.
            let code = finalize_report(&mut report, true, false, 1, false);
            assert_ne!(code, 0);
            assert!(report.error.is_some());
        }

        #[test]
        fn shutdown_barrier_failure_is_not_overwritten_by_complete_diag() {
            let (mut report, _counters, _diag) = assembled(ThreadMode::Integrated);
            let code = finalize_report(&mut report, false, false, 1, true);
            assert_ne!(code, 0);
            assert_eq!(report.exit_code, Some(code));
            assert!(report.error.is_some());
        }

        #[test]
        fn threaded_frame_phase_requires_a_consumed_wake() {
            // Counters complete on their own must not end the Threaded frame
            // phase; only a real consumed notification may.
            assert!(
                !frame_phase_satisfied(Flow::Threaded, true, 0),
                "complete counters without a consumed wake must not satisfy Threaded"
            );
            assert!(frame_phase_satisfied(Flow::Threaded, true, 1));
            // Integrated has no wake-consumption requirement.
            assert!(frame_phase_satisfied(Flow::Integrated, true, 0));
            assert!(!frame_phase_satisfied(Flow::Integrated, false, 0));
        }

        #[test]
        fn threaded_timer_is_unconditional_failure_in_the_frame_phase() {
            let consumed = AtomicU64::new(0);
            assert_eq!(
                accept_frame_tick(Flow::Threaded, Tick::Timer, false, &consumed),
                Err("browser creation/frame: bounded timer expired without a UI wake; Threaded delivery must be wake-driven".into())
            );
            // Even when the counters already look complete, a timer expiry in
            // this phase is a failure: it would let an observer/watchdog timer
            // masquerade as a real wake.
            assert!(accept_frame_tick(Flow::Threaded, Tick::Timer, true, &consumed).is_err());
            assert_eq!(consumed.load(Ordering::Acquire), 0);
        }

        #[test]
        fn threaded_notified_tick_counts_one_consumed_wake() {
            let consumed = AtomicU64::new(0);
            assert!(accept_frame_tick(Flow::Threaded, Tick::Notified, false, &consumed).is_ok());
            assert_eq!(consumed.load(Ordering::Acquire), 1);
        }

        #[test]
        fn threaded_success_requires_consumed_frame_notifications() {
            assert!(
                !host_evidence_ok(Flow::Threaded, 9, 3, 0, 0),
                "Threaded success must not rest on FFI/notify counts alone"
            );
            assert!(host_evidence_ok(Flow::Threaded, 9, 3, 0, 1));
            // Integrated pump is the scheduler, so it has no consumed-wake
            // requirement beyond the existing evidence.
            assert!(host_evidence_ok(Flow::Integrated, 9, 3, 0, 0));
            // A real borrow violation always fails.
            assert!(!host_evidence_ok(Flow::Threaded, 9, 3, 1, 1));
        }

        #[test]
        fn fatal_start_preserves_the_requested_exit_code() {
            let (mut report, _counters, _diag) = assembled(ThreadMode::Integrated);
            let code = finalize_report(&mut report, false, true, 37, false);
            assert_eq!(code, 37);
            assert_eq!(report.exit_code, Some(37));
        }
    }
}
