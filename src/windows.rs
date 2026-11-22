use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    browser::{
        BrowserChannel, CefAppBuilder, CefClientBuilder, CefRenderHandler, CursorShape, FrameStore,
    },
    ui::BrowserView,
};
use cef::{args::Args, *};
use gpui::{App as GpuiApp, AppContext as _, Bounds, WindowBounds, WindowOptions, px, size};

/// Coalesces wake-ups from the ticker thread into the main-thread pump.
///
/// A plain "pending" flag rather than a queue: if the main thread is busy for
/// 50 ms the ticker raises the flag three times, but the pump only needs to run
/// once afterwards. `do_message_loop_work` drains whatever accumulated inside
/// CEF, so nothing is lost by collapsing the requests.
#[derive(Default)]
struct PumpState {
    pending: AtomicBool,
}

impl PumpState {
    fn request_tick(&self) {
        // `swap` on an `AtomicBool` is enough; the pump wakes on its own timer
        // and checks this flag, so no OS notification is required.
        self.pending.store(true, Ordering::Release);
    }

    fn take_tick(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }
}

/// Frame-pump timing, reported when `POC_PUMP_STATS` is set.
#[derive(Default)]
struct PumpStats {
    ticks: u64,
    worst: Duration,
    last_report: u64,
}

impl PumpStats {
    fn record(&mut self, cost: Duration) {
        self.ticks += 1;
        if cost > self.worst {
            self.worst = cost;
        }
    }

    fn should_report(&mut self) -> bool {
        if self.ticks >= self.last_report + 120 {
            self.last_report = self.ticks;
            true
        } else {
            false
        }
    }

    fn summary(&self) -> String {
        format!(
            "ticks={} worst_tick_ms={}",
            self.ticks,
            self.worst.as_millis()
        )
    }
}

/// Advances the scripted self-test by one step.
///
/// `dispatch_event` is not callable from outside gpui (its return type is
/// crate-private), so the test drives the same `BrowserView` handlers the
/// listeners call: coordinate translation, key mapping and wheel units are all
/// covered, only the `cx.listener` glue in between is not.
fn run_self_test_step(
    view: &mut BrowserView,
    channel: &BrowserChannel,
    index: &mut usize,
    composition: bool,
) {
    let steps = crate::browser::probe::gpui_steps();
    if *index < steps.len() {
        view.apply_probe(&steps[*index]);
        *index += 1;
        return;
    }
    let direct = if composition {
        crate::browser::probe::composition_steps()
    } else {
        crate::browser::probe::steps()
    };
    let direct_index = *index - steps.len();
    if direct_index < direct.len()
        && let Some(host) = channel.host()
    {
        crate::browser::probe::apply(direct[direct_index], &host);
    }
    *index += 1;
}

pub fn run() -> i32 {
    // Required by this cef-rs revision before any other CEF API call.
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    let args = Args::new();
    let mut cef_app = CefAppBuilder::new();
    let process_result = execute_process(
        Some(args.as_main_args()),
        Some(&mut cef_app),
        std::ptr::null_mut(),
    );
    if process_result >= 0 {
        // Renderer/GPU/utility children must never initialize GPUI or CEF again.
        return process_result;
    }

    let executable_dir = match std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_owned()))
    {
        Some(path) => path,
        None => {
            eprintln!("Cannot locate the executable directory");
            return 1;
        }
    };
    if !executable_dir.join("icudtl.dat").is_file() || !executable_dir.join("locales").is_dir() {
        eprintln!("CEF runtime is not staged. Use cargo run or scripts/run.ps1 to stage it.");
        return 1;
    }
    let runtime_dir = executable_dir.to_string_lossy();
    let locales_dir = executable_dir
        .join("locales")
        .to_string_lossy()
        .into_owned();
    // Each run gets its own CEF cache directory. Sharing the default one makes
    // two processes (or a leftover one) contend for the same lock files, and the
    // loser's renderer stalls or its IME stops delivering composition events.
    let cache_dir = std::env::temp_dir().join(format!("gpui-cef-poc-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&cache_dir);
    let cache_dir = cache_dir.to_string_lossy().into_owned();
    let settings = Settings {
        windowless_rendering_enabled: 1,
        // Periodic polling on GPUI's main thread, not the schedule-callback mode.
        external_message_pump: 0,
        multi_threaded_message_loop: 0,
        // Demo-only. Do not use this executable to browse untrusted content.
        no_sandbox: 1,
        resources_dir_path: runtime_dir.as_ref().into(),
        locales_dir_path: locales_dir.as_str().into(),
        root_cache_path: cache_dir.as_str().into(),
        ..Default::default()
    };
    if initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut cef_app),
        std::ptr::null_mut(),
    ) != 1
    {
        eprintln!("CEF initialization failed");
        return 1;
    }

    let frames = Arc::new(FrameStore::default());
    let channel = Arc::new(BrowserChannel::default());
    let closed = Arc::new(AtomicBool::new(false));
    // Accelerated OSR is opt-in: it needs a D3D11 device in this process and the
    // adapter must allow cross-process sharing. When either fails we stay on the
    // software path instead of failing to start.
    let accelerated = std::env::var("POC_ACCELERATED")
        .map(|value| value != "0")
        .unwrap_or(true);
    let receiver = accelerated
        .then(crate::browser::d3d11_receiver::TextureReceiver::new)
        .flatten()
        .map(Arc::new);
    let accelerated = receiver.is_some();
    if accelerated {
        eprintln!("Accelerated OSR: D3D11 device ready");
    } else if std::env::var("POC_ACCELERATED").is_ok() {
        eprintln!("Accelerated OSR requested but no D3D11 device; using software frames");
    }
    let render_handler = Arc::new(CefRenderHandler::new(
        frames.clone(),
        channel.clone(),
        receiver,
    ));
    let mut client =
        CefClientBuilder::build((*render_handler).clone(), closed.clone(), channel.clone());
    // "1" = load the probe page and drive scripted input; "2" = load the probe
    // page only, so an external tool can inject real OS-level input.
    let self_test_mode = std::env::var("POC_SELF_TEST").unwrap_or_default();
    // 3 = drag (pointer move with a held button), 4 = file drop.
    // 6 = IME composition only. Kept apart from the pointer/keyboard run because
    // an active system IME owns the composition state and makes the synthetic
    // calls unreliable.
    let self_test = matches!(self_test_mode.as_str(), "1" | "2" | "3" | "4" | "5" | "6");
    let scripted = self_test_mode == "1";
    let composition_test = self_test_mode == "6";
    // 5 = HTML5 drag between two elements inside the page.
    let drag_test = self_test_mode == "3";
    let html5_drag_test = self_test_mode == "5";
    let (initial_url, label) = match std::env::var("START_URL").ok().filter(|s| !s.is_empty()) {
        Some(url) => {
            eprintln!("Warning: sandbox is disabled. Only load trusted content in this PoC.");
            (url.clone(), url)
        }
        None if self_test => (
            gpui_cef_poc::interaction_test_url(),
            "Interaction self-test".to_string(),
        ),
        None => (gpui_cef_poc::demo_url(), "Local offline demo".to_string()),
    };
    let initial = channel.viewport();
    let window_info = WindowInfo {
        windowless_rendering_enabled: 1,
        // Without this CEF only ever calls OnPaint with a CPU buffer.
        shared_texture_enabled: accelerated as _,
        runtime_style: RuntimeStyle::ALLOY,
        ..Default::default()
    };
    let browser_settings = BrowserSettings {
        // 60 matches a normal display refresh. The old 30 Hz cap meant a window
        // resize could wait up to ~33 ms for the next frame, which is what made
        // the content trail the drag by a visible step.
        windowless_frame_rate: 60,
        background_color: 0xff_f3_f6_fb,
        ..Default::default()
    };
    let browser = browser_host_create_browser_sync(
        Some(&window_info),
        Some(&mut client),
        Some(&initial_url.as_str().into()),
        Some(&browser_settings),
        None,
        None,
    );
    let Some(browser) = browser else {
        eprintln!("CEF browser creation failed");
        drop(client);
        drop(cef_app);
        shutdown();
        return 1;
    };
    channel.set_browser(browser.clone());
    let _ = (initial.width, initial.height);

    // Extra browsers share the same CEF process. Each gets its own frame store,
    // channel, client and view, because CEF requires every browser to have a
    // distinct client/handler set: reusing one would mix their paint callbacks.
    let extra_count = std::env::var("POC_BROWSERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 4);
    let mut tiles: Vec<(Arc<FrameStore>, Arc<BrowserChannel>, cef::Browser, String)> = Vec::new();
    for index in 1..extra_count {
        let extra_frames = Arc::new(FrameStore::default());
        let extra_channel = Arc::new(BrowserChannel::default());
        let extra_receiver = accelerated
            .then(crate::browser::d3d11_receiver::TextureReceiver::new)
            .flatten()
            .map(Arc::new);
        let mut extra_client = CefClientBuilder::build(
            CefRenderHandler::new(extra_frames.clone(), extra_channel.clone(), extra_receiver),
            Arc::new(AtomicBool::new(false)),
            extra_channel.clone(),
        );
        let created = browser_host_create_browser_sync(
            Some(&window_info),
            Some(&mut extra_client),
            Some(&initial_url.as_str().into()),
            Some(&browser_settings),
            None,
            None,
        );
        match created {
            Some(extra_browser) => {
                extra_channel.set_browser(extra_browser.clone());
                tiles.push((
                    extra_frames,
                    extra_channel,
                    extra_browser,
                    format!("Instance {index}"),
                ));
            }
            None => eprintln!("Browser instance {index} failed to start"),
        }
    }
    let extra_tiles = Arc::new(tiles);

    let smoke_duration = std::env::var("POC_EXIT_AFTER_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis);
    let startup_failed = Arc::new(AtomicBool::new(false));
    let failed = startup_failed.clone();
    let frames_for_ui = frames.clone();
    let channel_for_view = channel.clone();
    let channel_for_ui = channel.clone();
    // gpui-pre removed `Application::new()`: the platform is injected rather
    // than compiled in, and `gpui_platform::application()` picks the current
    // one. This also means the app no longer needs a `gpui_platform` split.
    gpui_platform::application().run(move |cx: &mut GpuiApp| {
        // Registers the component library's theme, globals and root element.
        // Without this its menus and controls render unthemed (and panic on
        // missing globals).
        gpui_component::init(cx);
        let bounds = Bounds::centered(None, size(px(1100.), px(760.)), cx);
        // Extra instances render as a grid next to the primary one. Each has its
        // own view so paint callbacks, focus and IME stay per-browser.
        let extra_views: Vec<(gpui::Entity<BrowserView>, Arc<BrowserChannel>)> = extra_tiles
            .iter()
            .map(|(frames, channel, _browser, title)| {
                let entity = cx
                    .new(|cx| BrowserView::new(frames.clone(), channel.clone(), title.clone(), cx));
                (entity, channel.clone())
            })
            .collect();
        let view = cx.new(|cx| {
            BrowserView::new(
                frames_for_ui.clone(),
                channel_for_view.clone(),
                label.clone(),
                cx,
            )
            .with_tiles(extra_views.clone())
        });
        cx.on_window_closed(|cx, _window_id| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("GPUI + CEF PoC".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let view_for_window = view.clone();
                view_for_window.update(cx, |view, cx| view.install(window, cx));
                view.clone()
            },
        );
        if let Err(error) = opened {
            eprintln!("Cannot open GPUI window: {error}");
            failed.store(true, Ordering::Relaxed);
            cx.quit();
            return;
        }
        let weak_view = view.downgrade();
        let started = Instant::now();
        let mut last_sequence = 0;
        let mut last_cursor = CursorShape::Arrow;
        let mut self_test_index = 0_usize;
        let mut next_self_test_step = Duration::from_millis(0);
        let resize_after = std::env::var("POC_RESIZE_AFTER_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_millis);
        let mut resized = false;
        let mut drag_index = 0_usize;
        let mut html5_index = 0_usize;
        let drop_file = std::env::var("POC_DROP_FILE")
            .ok()
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from);
        let mut dropped = false;
        let mut drop_step = 0_usize;
        let mut next_drop_step = Duration::from_millis(0);
        // CEF must be pumped from the thread that owns the browser. That is the
        // gpui main thread, but gpui's foreground executor can go quiet for
        // hundreds of milliseconds while idle (measured: a 171 ms gap against a
        // 16 ms target), which starves the pump and drops the frame rate.
        //
        // So the pump runs on its own thread and wakes the main thread through
        // gpui's own dispatch (a PostMessage under the hood) for the parts that
        // need the App. `do_message_loop_work` itself must stay on the main
        // thread: CEF dispatches window messages from it.
        let pump = Arc::new(PumpState::default());
        let pump_for_ticker = pump.clone();
        let pump_for_tick = pump.clone();
        let mut stats = PumpStats::default();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(8));
                pump_for_ticker.request_tick();
            }
        });

        cx.spawn(async move |cx| {
            loop {
                // Yielding to the executor is only how this task waits; the
                // actual cadence comes from `request_tick`, and a backlog is
                // collapsed into one wake-up rather than a burst of ticks.
                //
                // The wait is 1 ms, not 4: gpui's timer adds its own dispatch
                // latency on top, and at 4 ms the pump was late often enough to
                // miss CEF's 16.7 ms frame cadence (measured 452 instead of 532
                // frames per 8 s).
                cx.background_executor()
                    .timer(Duration::from_millis(1))
                    .await;
                if !pump_for_tick.take_tick() {
                    continue;
                }
                let started_tick = Instant::now();
                // CEF dispatches window messages from here, so this must be the
                // main thread. Never hold a gpui `App` borrow across it.
                do_message_loop_work();
                // IME candidate placement needs the window's own thread.
                let scale = cx.update(|cx| {
                    cx.windows()
                        .first()
                        .copied()
                        .and_then(|handle| {
                            handle.update(cx, |_, window, _| window.scale_factor()).ok()
                        })
                        .unwrap_or(1.0)
                });
                crate::ui::apply_pending_ime_reposition(&channel_for_ui, scale);
                // A dropped resize capture can leave CEF waiting for a frame
                // indefinitely (#3826). Demand a fresh capture outside callbacks,
                // bounded in rate/count and cancelled by a complete target frame.
                channel_for_ui.recover_resize();
                for (_, channel) in &extra_views {
                    channel.recover_resize();
                }
                stats.record(started_tick.elapsed());
                if std::env::var("POC_PUMP_DETAIL").is_ok() {
                    let cost = started_tick.elapsed();
                    if cost > Duration::from_millis(30) {
                        eprintln!("slow pump tick: {} ms", cost.as_millis());
                    }
                }
                if std::env::var("POC_PUMP_STATS").is_ok() && stats.should_report() {
                    let readback = channel_for_ui.last_readback();
                    eprintln!("PUMP {} readback={readback}", stats.summary());
                }
                let result = cx.update(|cx| {
                    if cx.windows().is_empty() {
                        return false;
                    }
                    // Test hook: grow the window once to prove resize/DPI sync.
                    if let Some(after) = resize_after
                        && !resized
                        && started.elapsed() >= after
                    {
                        resized = true;
                        if let Some(handle) = cx.windows().first().copied() {
                            let _ = handle.update(cx, |_, window, _| {
                                window.resize(gpui::size(px(1500.), px(950.)));
                            });
                        }
                    }
                    let sequence = frames_for_ui.sequence();
                    let cursor = channel_for_ui.cursor();
                    if sequence != last_sequence || cursor != last_cursor {
                        last_sequence = sequence;
                        last_cursor = cursor;
                        let _ = weak_view.update(cx, |_, cx| cx.notify());
                    }
                    // Test hook: drop a real file into the browser, through the
                    // same `on_drop` path an Explorer drag ends in.
                    if let Some(path) = drop_file.as_ref() {
                        if !dropped && started.elapsed() >= Duration::from_millis(2500) {
                            dropped = true;
                            drop_step = 0;
                            let _ = weak_view.update(cx, |view, _| {
                                view.handle_file_drop(std::slice::from_ref(path));
                            });
                            next_drop_step = started.elapsed() + Duration::from_millis(200);
                        } else if dropped && drop_step < 3 && started.elapsed() >= next_drop_step {
                            let step = drop_step;
                            drop_step += 1;
                            let _ = weak_view.update(cx, |view, _| {
                                view.step_file_drop(step);
                            });
                            next_drop_step = started.elapsed() + Duration::from_millis(200);
                        }
                    }
                    // Scripted input self-tests (see `probe`).
                    if html5_drag_test
                        && started.elapsed() > Duration::from_millis(1800)
                        && started.elapsed() >= next_self_test_step
                    {
                        let _ = weak_view.update(cx, |view, _| {
                            view.run_html5_drag_probe(&mut html5_index);
                        });
                        next_self_test_step = started.elapsed() + Duration::from_millis(350);
                    }
                    if drag_test
                        && started.elapsed() > Duration::from_millis(1500)
                        && started.elapsed() >= next_self_test_step
                    {
                        let _ = weak_view.update(cx, |view, _| {
                            view.run_drag_probe(&mut drag_index);
                        });
                        next_self_test_step = started.elapsed() + Duration::from_millis(350);
                    }
                    if (scripted || composition_test)
                        && started.elapsed() > Duration::from_millis(1200)
                        && started.elapsed() >= next_self_test_step
                    {
                        let composition = composition_test;
                        let _ = weak_view.update(cx, |view, _| {
                            run_self_test_step(
                                view,
                                &channel_for_ui,
                                &mut self_test_index,
                                composition,
                            );
                        });
                        next_self_test_step = started.elapsed() + Duration::from_millis(250);
                    }
                    if smoke_duration.is_some_and(|duration| started.elapsed() >= duration) {
                        cx.quit();
                        return false;
                    }
                    true
                });
                if !result {
                    break;
                }
            }
        })
        .detach();
        cx.activate(true);
    });

    // close_browser is asynchronous. Continue pumping on the same main thread
    // until on_before_close is delivered; shutdown beforehand is invalid.
    if let Some(host) = browser.host() {
        host.close_browser(1);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while !closed.load(Ordering::Acquire) && Instant::now() < deadline {
        do_message_loop_work();
        std::thread::sleep(Duration::from_millis(5));
    }
    if !closed.load(Ordering::Acquire) {
        eprintln!("Timed out closing CEF; exiting without unsafe premature cef_shutdown");
        // Do not run CEF destructors or shutdown while its browser is still alive.
        std::process::exit(1);
    }
    drop(browser);
    drop(client);
    drop(cef_app);
    shutdown();
    eprintln!(
        "CEF browser closed; shutdown complete. Frames received: {}",
        frames.sequence()
    );
    if let Ok(path) = std::env::var("POC_FRAME_DUMP") {
        if let Some((width, height, sample)) = frames.fingerprint() {
            let body = format!(
                "accelerated={}
size={width}x{height}
sample={}
",
                channel.is_accelerated(),
                sample
                    .iter()
                    .map(|b| b.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            );
            let _ = std::fs::write(path, body);
        }
    }
    if self_test {
        let title = channel.title();
        let missing = if composition_test {
            crate::browser::probe::evaluate_composition(&title)
        } else if self_test_mode == "5" {
            crate::browser::probe::evaluate_html5_drag(&title)
        } else if self_test_mode == "3" {
            crate::browser::probe::evaluate_drag(&title)
        } else if self_test_mode == "4" {
            crate::browser::probe::evaluate_drop(&title)
        } else if scripted {
            crate::browser::probe::evaluate(&title)
        } else {
            crate::browser::probe::evaluate_os_input(&title)
        };
        let verdict = if missing.is_empty() {
            "ok".to_string()
        } else {
            format!("missing {missing:?}")
        };
        eprintln!("SELF_TEST_TITLE={title}");
        eprintln!("SELF_TEST_RESULT={verdict}");
        // Also written to a file: a pipe reader can miss output when the
        // process is killed, and the console code page mangles CJK text.
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(|dir| dir.to_path_buf()))
        {
            let body = format!("title={title}\nresult={verdict}\n");
            let _ = std::fs::write(dir.join("self-test-result.txt"), body);
        }
    }
    if startup_failed.load(Ordering::Relaxed)
        || (smoke_duration.is_some() && frames.sequence() == 0)
    {
        eprintln!("Validation failed: no GPUI window or no CEF paint callback");
        1
    } else {
        0
    }
}
