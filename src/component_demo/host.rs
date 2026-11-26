//! One foreground owner of a public Runtime and its single CefBrowser View.
//! The legacy PoC is not imported. Smoke-only observation is explicit CLI state.
use crate::cli::{Config, HostMode};
use gpui::{
    App, AppContext as _, AsyncApp, Bounds, Context, Entity, FocusHandle, IntoElement, MouseButton,
    Render, Window, WindowBounds, WindowHandle, WindowOptions, div, prelude::*, px, rgb, size,
};
use rust_cef_gpui::{
    CefBrowser, ContextMenuConfig, ContextMenuRegistration, ContextMenuServices,
    DefaultMenuPresenter, NativeCefBrowser, NativeDropTargetServices, NativeViewDiagnostics,
    Runtime, RuntimeServices, ViewDiagnostics,
    contracts::{
        BrowserConfig, BrowserCursor, BrowserId, BrowserOptions, BrowserSurfaceMode,
        NativeChildHwndConfig, PixelRect, ProfileStorage, RuntimeConfig, RuntimeFiles,
        RuntimeSettings, RuntimeState, SessionState, ShortcutFilter, ShortcutId, ShortcutRule,
        ThreadMode, TrustedContentPolicy, Viewport, step_canonical_zoom_level,
        zoom_level_to_percent,
    },
    custom_schemes::{CustomSchemeConfig, CustomSchemeOptions, MemoryAsset, SchemeAssetSource},
    dispatcher::{self, UiWakeReceiver, UiWakeSender},
    host_capabilities::{
        AuthDecision, AuthRequest, AuthService, CdpEvent, CdpMethodResult, CdpObserver,
        DownloadProgress, DownloadRequest, DownloadService, HostCapabilities, JsDialogDecision,
        JsDialogRequest, JsDialogService, PdfExportOptions, PopupRequest, PopupService,
    },
    js_bridge::{EvalOutcome, JsBridgeConfig, JsFrameScope, MAX_JS_PAYLOAD_BYTES},
    menu::{MenuContextLimits, MenuLimits},
    outbound_drag_contracts::OutboundDragPolicy,
    ports::{
        BrowserEvent, BrowserEventEnvelope, BrowserEventSink, CloseMode, FrameSink, OwnedFrame,
        PublishResult, UiDispatcher,
    },
    runtime::{ShutdownStatus, StartError},
};
use std::{
    cell::{Cell, RefCell},
    future::Future,
    io::Write as _,
    path::Path,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    task::Poll,
    time::{Duration, Instant},
};

const STARTUP_BUDGET: Duration = Duration::from_secs(30);
const DRAIN_BUDGET: Duration = Duration::from_secs(15);
const PUMP_PERIOD: Duration = Duration::from_millis(4);

#[derive(Default)]
struct DemoHostCapabilitiesState {
    downloads: Mutex<Vec<DownloadRequest>>,
    js_dialogs: Mutex<Vec<JsDialogRequest>>,
    auths: Mutex<Vec<AuthRequest>>,
    popups: Mutex<Vec<PopupRequest>>,
    cdp_events: Mutex<Vec<CdpEvent>>,
    cdp_results: Mutex<Vec<CdpMethodResult>>,
}

struct DemoHostCapabilitiesAdapter(Arc<DemoHostCapabilitiesState>);

impl DownloadService for DemoHostCapabilitiesAdapter {
    fn on_before_download(&self, request: DownloadRequest) {
        self.0
            .downloads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request);
    }

    fn on_download_updated(&self, progress: DownloadProgress) {
        println!(
            "COMPONENT_DEMO_DOWNLOAD cef_id={} state={:?} received={}",
            progress.id.cef_id, progress.state, progress.received_bytes
        );
    }
}

impl JsDialogService for DemoHostCapabilitiesAdapter {
    fn on_js_dialog(&self, request: JsDialogRequest) {
        self.0
            .js_dialogs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request);
    }
}

impl PopupService for DemoHostCapabilitiesAdapter {
    fn on_popup_request(&self, request: PopupRequest) {
        println!(
            "COMPONENT_DEMO_POPUP url_len={} allowed={}",
            request.target_url.len(),
            request.policy_allowed
        );
        self.0
            .popups
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request);
    }
}

impl AuthService for DemoHostCapabilitiesAdapter {
    fn on_auth_credentials(&self, request: AuthRequest) {
        self.0
            .auths
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request);
    }
}

impl CdpObserver for DemoHostCapabilitiesAdapter {
    fn on_method_result(&self, result: CdpMethodResult) {
        println!(
            "COMPONENT_DEMO_CDP_RESULT id={} success={}",
            result.id.message_id, result.success
        );
        self.0
            .cdp_results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(result);
    }

    fn on_event(&self, event: CdpEvent) {
        self.0
            .cdp_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event);
    }
}

#[derive(Default)]
struct Observations {
    created: AtomicU64,
    closed: AtomicU64,
    first_fault: Mutex<Option<String>>,
}
impl BrowserEventSink for Observations {
    fn publish(&self, event: BrowserEventEnvelope) -> PublishResult {
        match event.event {
            BrowserEvent::Created => {
                self.created.fetch_add(1, Ordering::Release);
            }
            BrowserEvent::Closed => {
                self.closed.fetch_add(1, Ordering::Release);
            }
            fault @ (BrowserEvent::CreateFailed(_)
            | BrowserEvent::CaptureFailed(_)
            | BrowserEvent::RendererTerminated { .. }) => {
                let message = format!("native demo Browser {:?}: {fault:?}", event.browser);
                self.first_fault
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_or_insert(message);
            }
            _ => {}
        }
        PublishResult::Published
    }
}

struct IgnoreFrame;
impl FrameSink for IgnoreFrame {
    fn publish(&self, _: OwnedFrame) -> PublishResult {
        PublishResult::Published
    }
}

fn demo_shortcut_filter() -> ShortcutFilter {
    ShortcutFilter::new(vec![
        ShortcutRule {
            id: ShortcutId(1),
            windows_key_code: 0xBB, // VK_OEM_PLUS ('=')
            shift: false,
            control: true,
            alt: false,
            platform: false,
        },
        ShortcutRule {
            id: ShortcutId(2),
            windows_key_code: 0xBD, // VK_OEM_MINUS ('-')
            shift: false,
            control: true,
            alt: false,
            platform: false,
        },
        ShortcutRule {
            id: ShortcutId(3),
            windows_key_code: 0x30, // '0'
            shift: false,
            control: true,
            alt: false,
            platform: false,
        },
        ShortcutRule {
            id: ShortcutId(4),
            windows_key_code: 0x4D, // 'M'
            shift: false,
            control: true,
            alt: false,
            platform: false,
        },
    ])
    .expect("valid component demo shortcuts")
}

#[derive(Clone)]
enum DemoView {
    Osr(Entity<CefBrowser>),
    Native(Entity<NativeCefBrowser>),
}

impl DemoView {
    fn as_osr(&self) -> Option<&Entity<CefBrowser>> {
        match self {
            Self::Osr(v) => Some(v),
            Self::Native(_) => None,
        }
    }

    fn as_native(&self) -> Option<&Entity<NativeCefBrowser>> {
        match self {
            Self::Osr(_) => None,
            Self::Native(v) => Some(v),
        }
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        use gpui::Focusable as _;
        match self {
            Self::Osr(v) => v.read(cx).focus_handle(cx),
            Self::Native(v) => v.read(cx).focus_handle(cx),
        }
    }

    fn ux_summary(&self, cx: &App) -> (BrowserCursor, f64, bool, String, String) {
        match self {
            Self::Osr(v) => {
                let b = v.read(cx);
                (
                    b.cursor(),
                    b.zoom_level(),
                    b.audio_muted(),
                    b.tooltip().to_owned(),
                    b.status_text().to_owned(),
                )
            }
            Self::Native(v) => {
                let b = v.read(cx);
                (
                    b.cursor(),
                    b.zoom_level(),
                    b.audio_muted(),
                    b.tooltip().to_owned(),
                    b.status_text().to_owned(),
                )
            }
        }
    }

    fn step_zoom(&self, steps: i32, window: &mut Window, cx: &mut App) {
        match self {
            Self::Osr(v) => v.update(cx, |b, cx| b.step_zoom(steps, window, cx)),
            Self::Native(v) => {
                let _ = v.update(cx, |b, _| {
                    let next = step_canonical_zoom_level(b.zoom_level(), steps);
                    b.set_zoom_level(next)
                });
            }
        }
    }

    fn reset_zoom(&self, window: &mut Window, cx: &mut App) {
        match self {
            Self::Osr(v) => v.update(cx, |b, cx| b.reset_zoom(window, cx)),
            Self::Native(v) => {
                let _ = v.update(cx, |b, _| b.set_zoom_level(0.0));
            }
        }
    }

    fn toggle_audio_muted(&self, cx: &mut App) {
        match self {
            Self::Osr(v) => {
                let _ = v.update(cx, |b, _| b.set_audio_muted(!b.audio_muted()));
            }
            Self::Native(v) => {
                let _ = v.update(cx, |b, _| b.set_audio_muted(!b.audio_muted()));
            }
        }
    }

    fn consume(&self, window: &mut Window, cx: &mut App) -> Result<(), String> {
        match self {
            Self::Osr(v) => v.update(cx, |b, cx| {
                b.consume(window, cx)
                    .map(|_| ())
                    .map_err(|e| format!("demo View consume: {e:?}"))
            }),
            Self::Native(v) => v.update(cx, |b, cx| {
                b.consume(window, cx)
                    .map_err(|e| format!("demo NativeView consume: {e:?}"))
            }),
        }
    }

    fn into_element_child(self) -> gpui::AnyElement {
        match self {
            Self::Osr(v) => v.into_any_element(),
            Self::Native(v) => v.into_any_element(),
        }
    }
}

struct Root {
    browser: Option<DemoView>,
    /// The component's own menu presenter. Without it the delivered host binds
    /// menu services that nothing ever renders, so no menu can appear at all.
    menu: Option<std::rc::Rc<std::cell::RefCell<DefaultMenuPresenter>>>,
    message: &'static str,
    ux_controls: bool,
}
impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let menu = self
            .menu
            .as_ref()
            .and_then(|menu| menu.borrow().element(window, cx));
        let root = if self.ux_controls
            && let Some(view) = self.browser.as_ref()
        {
            let (cursor, zoom, muted, tooltip, status) = view.ux_summary(cx);
            let zoom_pct = zoom_level_to_percent(zoom).round() as i32;
            let minus_view = view.clone();
            let reset_view = view.clone();
            let plus_view = view.clone();
            let mute_view = view.clone();
            let bar = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .px(px(8.0))
                .h(px(28.0))
                .flex_shrink_0()
                .bg(rgb(0x1f2937))
                .text_color(rgb(0xf3f4f6))
                .text_size(px(12.0))
                .child(
                    div()
                        .id("ux-zoom-out")
                        .px(px(6.0))
                        .bg(rgb(0x374151))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |_this, _, window, cx| {
                                minus_view.step_zoom(-1, window, cx);
                                cx.notify();
                            }),
                        )
                        .child("Zoom -"),
                )
                .child(
                    div()
                        .id("ux-zoom-reset")
                        .px(px(6.0))
                        .bg(rgb(0x374151))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |_this, _, window, cx| {
                                reset_view.reset_zoom(window, cx);
                                cx.notify();
                            }),
                        )
                        .child(format!("Zoom {zoom_pct}%")),
                )
                .child(
                    div()
                        .id("ux-zoom-in")
                        .px(px(6.0))
                        .bg(rgb(0x374151))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |_this, _, window, cx| {
                                plus_view.step_zoom(1, window, cx);
                                cx.notify();
                            }),
                        )
                        .child("Zoom +"),
                )
                .child(
                    div()
                        .id("ux-mute-toggle")
                        .px(px(6.0))
                        .bg(rgb(0x374151))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |_this, _, _, cx| {
                                mute_view.toggle_audio_muted(cx);
                                cx.notify();
                            }),
                        )
                        .child(if muted { "Unmute" } else { "Mute" }),
                )
                .child(format!("Cursor: {cursor:?}"))
                .child(if !tooltip.is_empty() {
                    format!("Tip: {tooltip}")
                } else if !status.is_empty() {
                    format!("Status: {status}")
                } else {
                    String::new()
                });
            div()
                .flex()
                .flex_col()
                .size_full()
                .overflow_hidden()
                .child(bar)
                .child(
                    div()
                        .flex_1()
                        .w_full()
                        .overflow_hidden()
                        .child(view.clone().into_element_child()),
                )
        } else {
            let base = div().size_full().overflow_hidden();
            if let Some(view) = self.browser.as_ref() {
                base.child(view.clone().into_element_child())
            } else {
                base.child(self.message)
            }
        };
        match menu {
            Some(menu) => root.child(menu),
            None => root,
        }
    }
}

/// The sole async wake consumer. A timer supplies Integrated's host pump or
/// an explicit deadline; timing out must never substitute for a frame wake.
async fn wait(
    cx: &mut AsyncApp,
    receiver: &UiWakeReceiver,
    timeout: Option<Duration>,
) -> Result<bool, String> {
    let Some(timeout) = timeout else {
        return if receiver.recv().await {
            Ok(true)
        } else {
            Err("demo wake channel closed".into())
        };
    };
    let mut wake = std::pin::pin!(receiver.recv());
    let mut timer = std::pin::pin!(cx.background_executor().timer(timeout));
    std::future::poll_fn(|cx| {
        if let Poll::Ready(ok) = wake.as_mut().poll(cx) {
            return Poll::Ready(if ok {
                Ok(true)
            } else {
                Err("demo wake channel closed".into())
            });
        }
        if timer.as_mut().poll(cx).is_ready() {
            Poll::Ready(Ok(false))
        } else {
            Poll::Pending
        }
    })
    .await
}

async fn service(
    cx: &mut AsyncApp,
    rt: &mut Runtime,
    receiver: &UiWakeReceiver,
    deadline: Option<Instant>,
) -> Result<bool, String> {
    let mut timeout = deadline.map(|t| t.saturating_duration_since(Instant::now()));
    if rt.mode() == ThreadMode::Integrated {
        timeout = Some(timeout.map_or(PUMP_PERIOD, |value| value.min(PUMP_PERIOD)));
    }
    // Bounded turns in every mode: the delivered host must keep observing (and
    // reporting) even when the runtime has nothing to wake it about.
    const MAX_TURN: std::time::Duration = std::time::Duration::from_millis(100);
    timeout = Some(timeout.map_or(MAX_TURN, |value| value.min(MAX_TURN)));
    let woke = wait(cx, receiver, timeout).await?;
    if rt.mode() == ThreadMode::Integrated {
        rt.pump().map_err(|e| format!("demo pump: {e:?}"))?;
    }
    Ok(woke)
}

fn announce(gate: &Path) -> Result<(), String> {
    let ready = gate.with_extension("ready");
    let temp = gate.with_extension("ready.pending");
    if gate.exists() || ready.exists() || temp.exists() {
        return Err("smoke observer paths must be unused".into());
    }
    let mut marker = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|e| format!("smoke marker: {e}"))?;
    write!(marker, "{}", std::process::id()).map_err(|e| format!("smoke marker: {e}"))?;
    drop(marker);
    std::fs::rename(temp, ready).map_err(|e| format!("smoke ready: {e}"))
}

fn osr_diagnostics(cx: &mut AsyncApp, root: WindowHandle<Root>) -> Result<ViewDiagnostics, String> {
    root.update(cx, |root, _, cx| {
        root.browser
            .as_ref()
            .and_then(|view| view.as_osr())
            .map(|view| view.read(cx).diagnostics())
    })
    .map_err(|e| e.to_string())?
    .ok_or_else(|| "original demo View absent".into())
}

fn native_diagnostics(
    cx: &mut AsyncApp,
    root: WindowHandle<Root>,
) -> Result<NativeViewDiagnostics, String> {
    root.update(cx, |root, _, cx| {
        root.browser
            .as_ref()
            .and_then(|view| view.as_native())
            .map(|view| view.read(cx).diagnostics())
    })
    .map_err(|e| e.to_string())?
    .ok_or_else(|| "original demo NativeView absent".into())
}

fn apply_demo_shortcut(zoom: f64, muted: bool, id: ShortcutId) -> (Option<f64>, Option<bool>) {
    match id.0 {
        1 => (Some(step_canonical_zoom_level(zoom, 1)), None),
        2 => (Some(step_canonical_zoom_level(zoom, -1)), None),
        3 => (Some(0.0), None),
        4 => (None, Some(!muted)),
        _ => (None, None),
    }
}

#[derive(Default)]
struct ViewLife {
    browser: Option<BrowserId>,
    first_paint: bool,
    released: Option<ViewDiagnostics>,
    /// Opt-in owned menu services, target and native registration. Dropped with
    /// the view so the delivered host retires them on the ordinary teardown.
    menus: Option<(ContextMenuServices, ContextMenuRegistration)>,
    /// Renders and answers the component menu. Held with the view so it retires
    /// on the ordinary teardown.
    menu_presenter: Option<std::rc::Rc<std::cell::RefCell<DefaultMenuPresenter>>>,
    /// Replaces GPUI's file-only drop target in OSR mode so outbound/inbound/intra-page
    /// drags receive Win32 `IDropTarget` callbacks routed to CEF `DragTarget*`.
    drop_target: Option<NativeDropTargetServices>,
}

async fn live(
    cx: &mut AsyncApp,
    rt: &mut Runtime,
    receiver: &UiWakeReceiver,
    root: WindowHandle<Root>,
    close: &Cell<bool>,
    obs: &Observations,
    config: &Config,
    life: &mut ViewLife,
) -> Result<(), String> {
    let startup = Instant::now() + STARTUP_BUDGET;
    while rt.state() != RuntimeState::Ready {
        if close.get() {
            return Ok(());
        }
        if Instant::now() >= startup {
            return Err("demo Runtime readiness timed out".into());
        }
        service(cx, rt, receiver, Some(startup)).await?;
    }
    let (available, scale) = root
        .update(cx, |_, w, _| (w.viewport_size(), w.scale_factor()))
        .map_err(|e| e.to_string())?;
    let (width, height) = (
        f32::from(available.width).round(),
        f32::from(available.height).round(),
    );
    let top_dip = if config.ux_controls { 28.0 } else { 0.0 };
    let browser_height = (height - top_dip).max(1.0);
    if [width, browser_height]
        .into_iter()
        .any(|v| !v.is_finite() || v < 1.0 || v >= i32::MAX as f32)
        || !scale.is_finite()
        || scale <= 0.0
    {
        return Err("invalid window geometry before Browser creation".into());
    }
    let viewport = Viewport {
        width: width as i32,
        height: browser_height as i32,
        scale,
    };
    println!(
        "COMPONENT_DEMO_VIEWPORT width={} height={} scale={}",
        viewport.width, viewport.height, viewport.scale
    );
    // Opt-in: configure the real menu service BEFORE the Browser exists, as the
    // validation host does. Nothing is synthesized; without the flag the
    // delivered smoke path stays exactly as before.
    if config.context_menu {
        let nz = |n: usize| std::num::NonZeroUsize::new(n).expect("non-zero menu limit");
        let limits = MenuLimits {
            max_entries: nz(32),
            max_label_bytes: nz(256),
            max_total_label_bytes: nz(2048),
        };
        let context_limits = MenuContextLimits {
            max_frame_id_bytes: nz(512),
            max_text_bytes: nz(4096),
            max_suggestions: nz(5),
            max_total_bytes: nz(8192),
        };
        rt.configure_context_menus(ContextMenuConfig {
            capacity: nz(1),
            model_limits: limits,
            context_limits,
        })
        .map_err(|e| format!("demo menu config: {e:?}"))?;
    }
    let initial_url = if let Some(url) = &config.custom_url {
        url.clone()
    } else if config.menu_page {
        gpui_cef_poc::menu_probe_url()
    } else if config.js_bridge_page {
        "app://demo/index.html".into()
    } else {
        gpui_cef_poc::demo_url()
    };
    let mut options = BrowserOptions::default()
        .with_frame_rate(rust_cef_gpui::recommended_windowless_frame_rate());
    if config.ux_controls {
        options = options
            .with_tooltip_events(true)
            .with_shortcut_filter(demo_shortcut_filter());
    }
    let demo_caps_state = if config.host_capabilities {
        let state = Arc::new(DemoHostCapabilitiesState::default());
        let adapter = Arc::new(DemoHostCapabilitiesAdapter(state.clone()));
        let _ = rt.set_host_capabilities(HostCapabilities {
            download: Some(adapter.clone()),
            js_dialog: Some(adapter.clone()),
            popup: Some(adapter.clone()),
            auth: Some(adapter.clone()),
            cdp_observer: Some(adapter),
            ..Default::default()
        });
        options = options.with_headless_cdp(true);
        Some(state)
    } else {
        None
    };
    let demo_download_dir = config
        .cache_root
        .parent()
        .unwrap_or(&config.cache_root)
        .join("demo-downloads");
    if config.host_capabilities {
        let _ = std::fs::create_dir_all(&demo_download_dir);
    }
    if config.js_bridge_page {
        options = options.with_js_bridge(JsBridgeConfig {
            enabled: true,
            allowed_origins: vec!["app://demo".into()],
            max_message_bytes: MAX_JS_PAYLOAD_BYTES,
            ..Default::default()
        });
    }
    let parent_hwnd = root
        .update(cx, |_, window, _| rust_cef_gpui::window_hwnd(window))
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "GPUI window Win32 HWND unavailable".to_owned())?;
    match config.host_mode {
        HostMode::Osr => {
            options = options
                .with_parent_hwnd(parent_hwnd)
                .with_outbound_drag_policy(OutboundDragPolicy::AllowAsync);
        }
        HostMode::NativeHwnd => {
            let top_dip = if config.ux_controls { 28.0 } else { 0.0 };
            let initial_bounds = PixelRect {
                x: 0,
                y: (top_dip * scale).round() as i32,
                width: ((width * scale).round().max(1.0)) as u32,
                height: (((height - top_dip).max(1.0) * scale).round().max(1.0)) as u32,
            };
            options = options.with_surface_mode(BrowserSurfaceMode::NativeChildHwnd(
                NativeChildHwndConfig {
                    parent_hwnd,
                    initial_bounds,
                    activate_on_creation: false,
                },
            ));
        }
    }
    let handle = rt
        .create_browser(BrowserConfig {
            initial_url,
            profile: Some(rt.default_profile_handle()),
            render_preference: config.backend,
            viewport,
            options,
        })
        .map_err(|e| format!("demo Browser create: {e:?}"))?;
    let id = handle.snapshot().browser;
    life.browser = Some(id);
    let attachment = rt
        .attach_browser(&handle)
        .map_err(|e| format!("demo attach: {e:?}"))?;
    root.update(cx, |root, window, cx| {
        let demo_view = match config.host_mode {
            HostMode::Osr => {
                let view = CefBrowser::new(attachment, window, cx).map_err(|e| e.to_string())?;
                let _ = view.update(cx, |view, _| {
                    view.set_pinch_zoom_enabled(true);
                    view.set_ctrl_wheel_zoom_enabled(true);
                    if config.ux_controls {
                        view.set_on_shortcut(Some(Box::new(|id, view, window, cx| {
                            match id.0 {
                                1 => view.step_zoom(1, window, cx),
                                2 => view.step_zoom(-1, window, cx),
                                3 => view.reset_zoom(window, cx),
                                4 => {
                                    let _ = view.set_audio_muted(!view.audio_muted());
                                }
                                _ => {}
                            }
                            cx.notify();
                        })));
                    }
                    if config.js_bridge_page {
                        view.set_on_js_message(Some(Box::new(|origin, payload, view, _, _| {
                            println!(
                                "COMPONENT_DEMO_JS_MSG origin={origin} bytes={}",
                                payload.len()
                            );
                            let _ = view.post_js_message(r#"{"from":"host","ack":true}"#);
                            let _ = view.evaluate_script(
                                "document.title",
                                JsFrameScope::MainFrame,
                                2000,
                            );
                        })));
                        view.set_on_eval_completed(Some(Box::new(|id, outcome, _, _, _| {
                            let ok = matches!(outcome, EvalOutcome::Value(_));
                            println!("COMPONENT_DEMO_EVAL id={} ok={ok}", id.sequence);
                        })));
                    }
                });
                DemoView::Osr(view)
            }
            HostMode::NativeHwnd => {
                let view =
                    NativeCefBrowser::new(attachment, window, cx).map_err(|e| e.to_string())?;
                if config.ux_controls || config.js_bridge_page {
                    let _ = view.update(cx, |view, _| {
                        if config.ux_controls {
                            view.set_on_shortcut(Some(Box::new(|id, view, _, cx| {
                                let (z, m) =
                                    apply_demo_shortcut(view.zoom_level(), view.audio_muted(), id);
                                if let Some(next) = z {
                                    let _ = view.set_zoom_level(next);
                                }
                                if let Some(muted) = m {
                                    let _ = view.set_audio_muted(muted);
                                }
                                cx.notify();
                            })));
                        }
                        if config.js_bridge_page {
                            view.set_on_js_message(Some(Box::new(
                                |origin, payload, view, _, _| {
                                    println!(
                                        "COMPONENT_DEMO_JS_MSG origin={origin} bytes={}",
                                        payload.len()
                                    );
                                    let _ = view.post_js_message(r#"{"from":"host","ack":true}"#);
                                    let _ = view.evaluate_script(
                                        "document.title",
                                        JsFrameScope::MainFrame,
                                        2000,
                                    );
                                },
                            )));
                            view.set_on_eval_completed(Some(Box::new(|id, outcome, _, _, _| {
                                let ok = matches!(outcome, EvalOutcome::Value(_));
                                println!("COMPONENT_DEMO_EVAL id={} ok={ok}", id.sequence);
                            })));
                        }
                    });
                }
                DemoView::Native(view)
            }
        };
        // The menu bridge only captures once the view reports itself ready, which
        // requires the view to own GPUI focus. Nothing focuses it by default.
        let view_focus = demo_view.focus_handle(cx);
        root.browser = Some(demo_view);
        window.focus(&view_focus, cx);
        cx.notify();
        Ok::<_, String>(())
    })
    .map_err(|e| e.to_string())??;
    if config.host_mode == HostMode::Osr {
        life.drop_target = Some(
            root.update(cx, |root, window, cx| {
                let entity = root
                    .browser
                    .as_ref()
                    .and_then(|v| v.as_osr())
                    .ok_or_else(|| "demo OSR View missing for drop target".to_owned())?;
                let nz1 = std::num::NonZeroUsize::new(1).expect("non-zero drop target");
                let mut ds = NativeDropTargetServices::new(nz1)
                    .map_err(|e| format!("drop target new: {e:?}"))?;
                ds.attach(entity, window, cx)
                    .map_err(|e| format!("drop target attach: {e:?}"))?;
                Ok::<_, String>(ds)
            })
            .map_err(|e| e.to_string())??,
        );
    }
    // Opt-in: attach the real default menu presenter and bind the native
    // registration, exactly like the validation host. The presenter owns the
    // menu UI; nothing here fabricates a context model.
    if config.context_menu && config.host_mode == HostMode::Osr {
        let nz = |n: usize| std::num::NonZeroUsize::new(n).expect("non-zero menu limit");
        let limits = MenuLimits {
            max_entries: nz(32),
            max_label_bytes: nz(256),
            max_total_label_bytes: nz(2048),
        };
        let context_limits = MenuContextLimits {
            max_frame_id_bytes: nz(512),
            max_text_bytes: nz(4096),
            max_suggestions: nz(5),
            max_total_bytes: nz(8192),
        };
        let entity = root
            .update(cx, |root, _, _| {
                root.browser
                    .as_ref()
                    .and_then(|v| v.as_osr().cloned())
                    .ok_or("demo OSR View missing for menu")
            })
            .map_err(|e| e.to_string())??;
        // Opt-in host-managed capabilities. Configure before binding: the menu
        // capture asks the registry what the host can actually perform.
        if config.menu_structure {
            rt.configure_menu_structure(true)
                .map_err(|e| format!("demo menu structure: {e:?}"))?;
        }
        if config.menu_devtools {
            rt.configure_devtools(nz(1))
                .map_err(|e| format!("demo menu devtools: {e:?}"))?;
        }
        let mut menus = ContextMenuServices::new(nz(1), nz(1), limits, context_limits);
        let target = root
            .update(cx, |_, window, cx| menus.register(&entity, window, cx))
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("demo menu register: {e:?}"))?;
        let registration = rt
            .bind_context_menu(target.clone())
            .map_err(|e| format!("demo menu bind: {e:?}"))?;
        // The component's default presenter, created on the view that owns the
        // native menu target. Nothing renders a menu without it.
        let presenter = root
            .update(cx, |root, window, cx| {
                let entity = root
                    .browser
                    .as_ref()
                    .and_then(|v| v.as_osr().cloned())
                    .ok_or("demo OSR View missing for presenter")?;
                DefaultMenuPresenter::new(&entity, window, limits, context_limits, cx)
                    .map(|p| std::rc::Rc::new(std::cell::RefCell::new(p)))
                    .map_err(|e| format!("demo menu presenter: {e:?}"))
            })
            .map_err(|e| e.to_string())??;
        if let Some(path) = config.menu_receipt.clone() {
            let mut last_written = String::new();
            presenter.borrow().set_layout_observer(Some(Box::new(
                move |layout, scale| {
                    let rows = layout
                        .commands
                        .iter()
                        .map(|c| {
                            let rect = c
                                .bounds
                                .map(|b| format!("{},{},{},{}", b.left, b.top, b.width, b.height))
                                .unwrap_or_default();
                            format!(
                                "{{\"id\":{},\"en\":{},\"bounds\":{},\"rect\":\"{}\"}}",
                                c.command_id,
                                c.enabled,
                                c.bounds.is_some(),
                                rect
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(",");
                    if rows == last_written {
                        return;
                    }
                    let menu_rect = layout
                        .menu
                        .map(|b| format!("\"{},{},{},{}\"", b.left, b.top, b.width, b.height))
                        .unwrap_or_else(|| "null".into());
                    let text = format!(
                        "{{\"open\":true,\"source\":\"win32_timer\",\"menu_rect\":{menu_rect},\"rows\":[{rows}],\"reply\":\"none\",\"scale\":{scale},\"native\":null,\"service\":null}}"
                    );
                    if std::fs::write(&path, &text).is_ok() {
                        last_written = rows;
                    }
                },
            )));
        }
        life.menu_presenter = Some(presenter.clone());
        let _ = root.update(cx, |root, _, cx| {
            root.menu = Some(presenter);
            cx.notify();
        });
        life.menus = Some((menus, registration));
    }

    let mut smoke_end = None;
    let mut pending_observer_until = None;
    // Consume an already-coalesced wake once if Browser creation ran inline.
    let menu_receipt = config.menu_receipt.clone();
    let _ = receiver.try_recv();
    loop {
        if close.get() {
            return Ok(());
        }
        if let Some(error) = obs
            .first_fault
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Err(error);
        }
        if handle.snapshot().state == SessionState::Closed {
            return Err("demo Browser closed unexpectedly".into());
        }
        if let Some(caps_state) = &demo_caps_state {
            for req in std::mem::take(
                &mut *caps_state
                    .js_dialogs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            ) {
                let _ = rt.resolve_js_dialog(req.id, JsDialogDecision::Cancel);
            }
            for req in
                std::mem::take(&mut *caps_state.auths.lock().unwrap_or_else(|e| e.into_inner()))
            {
                let _ = rt.resolve_auth(req.id, AuthDecision::Cancel);
            }
            for req in std::mem::take(
                &mut *caps_state
                    .downloads
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            ) {
                let name = Path::new(&req.suggested_name)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("download.bin");
                let dest = demo_download_dir.join(format!("{}-{name}", req.id.sequence));
                let _ = rt.confirm_download(req.id, dest);
            }
            for popup in
                std::mem::take(&mut *caps_state.popups.lock().unwrap_or_else(|e| e.into_inner()))
            {
                if popup.policy_allowed && !popup.target_url.is_empty() {
                    let _ = rt.execute_cdp_method(
                        id,
                        "Runtime.evaluate",
                        Some(r#"{"expression":"document.title","returnByValue":true}"#.into()),
                    );
                }
            }
        }
        let _ = PdfExportOptions::default();
        // Consume on every turn, not only after a wake: the view reports itself
        // menu-ready (which the bridge requires before it captures anything) from
        // inside consume, so waiting for a wake leaves readiness false forever.
        let view = root
            .update(cx, |root, _, _| Ok::<_, String>(root.browser.clone()))
            .map_err(|e| e.to_string())??;
        if let Some(view) = view {
            root.update(cx, |_root, window, cx| {
                let res = view.consume(window, cx);
                if let DemoView::Osr(v) = &view {
                    let _ = v.update(cx, |b, cx| {
                        if b.diagnostics().input.pending {
                            let _ = b.retry_input(window, cx);
                        }
                    });
                }
                res
            })
            .map_err(|e| e.to_string())??;
        }
        // Feed every pending menu update into the component presenter before
        // writing the receipt so the receipt reflects the current turn.
        // Menu capture does not necessarily post a UI wake, so this runs on
        // every turn rather than only when the runtime woke us.
        if let (Some((menus, _)), Some(presenter), Some(browser)) = (
            life.menus.as_mut(),
            life.menu_presenter.as_ref().cloned(),
            life.browser,
        ) {
            root.update(cx, |_root, window, cx| {
                menus
                    .dispatch(browser, window, cx, |update, window, cx| {
                        presenter.borrow_mut().update(&update, window, cx)
                    })
                    .map(|_| ())
                    .map_err(|e| format!("demo menu dispatch: {e:?}"))
            })
            .map_err(|e| e.to_string())??;
        }
        // Delivered-host menu receipt: what the presenter really has, so an
        // external driver can see the rows a real click would select.
        if let (Some(path), Some(presenter)) = (menu_receipt.as_ref(), life.menu_presenter.as_ref())
            && let Ok(status) =
                root.update(cx, |_, _, _| Ok::<_, String>(presenter.borrow().status()))
            && let Ok(status) = status
        {
            let rows = status
                .last_layout
                .as_ref()
                .map(|l| {
                    l.commands
                        .iter()
                        .map(|c| {
                            let rect = c
                                .bounds
                                .map(|b| format!("{},{},{},{}", b.left, b.top, b.width, b.height))
                                .unwrap_or_default();
                            format!(
                                "{{\"id\":{},\"en\":{},\"bounds\":{},\"rect\":\"{}\"}}",
                                c.command_id,
                                c.enabled,
                                c.bounds.is_some(),
                                rect
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            let reply = status
                .last_reply
                .as_ref()
                .map(|r| format!("{:?}", r.requested))
                .unwrap_or_else(|| "none".into());
            let native = life
                .menus
                .as_ref()
                .map(|(_, reg)| reg.status())
                .map(|st| {
                    let failure = st
                        .first_failure
                        .as_ref()
                        .map(|f| format!("\"{}\"", format!("{f:?}").replace('\\', "\\\\").replace('"', "'")))
                        .unwrap_or_else(|| "null".into());
                    format!(
                        "{{\"opened\":{},\"published\":{},\"cancelled\":{},\"actions\":{},\"failed\":{},\"first_failure\":{}}}",
                        st.opened,
                        st.published,
                        st.cancelled,
                        st.actions_submitted,
                        st.first_failure.is_some(),
                        failure
                    )
                })
                .unwrap_or_else(|| "null".into());
            let service = life
                .menus
                .as_mut()
                .zip(life.browser)
                .and_then(|((menus, _), browser)| menus.status(browser).ok())
                .map(|st| {
                    format!(
                        "{{\"ready\":{},\"pending\":{},\"reply_admitted\":{},\"selection_error\":{}}}",
                        st.ready,
                        st.pending,
                        st.reply_admitted,
                        st.selection_error.is_some()
                    )
                })
                .unwrap_or_else(|| "null".into());
            // Raw readiness inputs, so a receipt can say which precondition is
            // false instead of only that the bridge did not capture.
            let (active, focused, scale) = root
                .update(cx, |root, window, cx| {
                    let focused = root
                        .browser
                        .as_ref()
                        .is_some_and(|v| v.focus_handle(cx).contains_focused(window, cx));
                    Ok::<_, String>((window.is_window_active(), focused, window.scale_factor()))
                })
                .unwrap_or_else(|_| Ok::<_, String>((false, false, 1.0)))
                .unwrap_or((false, false, 1.0));
            let (presses_in, presses_out, paints, consumed) = root
                .update(cx, |root, _, cx| {
                    let d = root
                        .browser
                        .as_ref()
                        .and_then(|v| v.as_osr())
                        .map(|v| v.read(cx).diagnostics())
                        .ok_or("demo OSR View absent".to_string())?;
                    Ok::<_, String>((
                        d.presses_received,
                        d.presses_forwarded,
                        d.paint_calls,
                        d.consumed_updates,
                    ))
                })
                .unwrap_or_else(|_| Ok::<_, String>((0, 0, 0, 0)))
                .unwrap_or((0, 0, 0, 0));
            let (asked, bound) = rt.context_menu_handler_counts();
            let bridged = life
                .browser
                .map(|b| rt.context_menu_bound(b))
                .unwrap_or(false);
            let menu_rect = status
                .last_layout
                .as_ref()
                .and_then(|l| l.menu)
                .map(|b| format!("\"{},{},{},{}\"", b.left, b.top, b.width, b.height))
                .unwrap_or_else(|| "null".into());
            let text = format!(
                "{{\"open\":{},\"menu_rect\":{},\"rows\":[{}],\"reply\":\"{}\",\"opens_seen\":{},\"withdraws\":{},\"deferred\":{},\"first_error\":{},\"native\":{},\"service\":{},\"scale\":{scale},\"window_active\":{active},\"view_focused\":{focused},\"presses_received\":{presses_in},\"presses_forwarded\":{presses_out},\"paint_calls\":{paints},\"consumed_updates\":{consumed},\"handler_asked\":{asked},\"handler_bound\":{bound},\"bridge_present\":{bridged}}}",
                status.open_request.is_some(),
                menu_rect,
                rows,
                reply.replace('"', "'"),
                status.opens_seen,
                status.withdraws,
                status.deferred_replies,
                status.first_error.is_some(),
                native,
                service,
            );
            let _ = std::fs::write(path, text);
        }
        if !life.first_paint {
            match config.host_mode {
                HostMode::Osr => {
                    let d = osr_diagnostics(cx, root)?;
                    if let Some(error) = d.last_error {
                        return Err(format!("initial GPUI paint: {error}"));
                    }
                    if d.paint_calls > 0
                        && d.painted_sequence > 0
                        && d.frame_width > 0
                        && d.frame_height > 0
                    {
                        if rt.diagnostics().observed_backend != Some(config.backend) {
                            return Err("first paint did not use the requested backend".into());
                        }
                        life.first_paint = true;
                        println!(
                            "COMPONENT_DEMO_PAINT runtime={} browser={} width={} height={} generation={} sequence={} backend={:?}",
                            id.runtime().value(),
                            id.local(),
                            d.frame_width,
                            d.frame_height,
                            d.painted_generation,
                            d.painted_sequence,
                            config.backend
                        );
                        if let Some(gate) = &config.observer_gate {
                            announce(gate)?;
                            pending_observer_until = Some(Instant::now() + STARTUP_BUDGET);
                        } else if let Some(duration) = config.smoke {
                            smoke_end = Some(Instant::now() + duration);
                        }
                    } else if Instant::now() >= startup {
                        return Err("demo first actual GPUI paint timed out".into());
                    }
                }
                HostMode::NativeHwnd => {
                    let nd = native_diagnostics(cx, root)?;
                    if let Some(error) = nd.last_error {
                        return Err(format!("initial native HWND view: {error}"));
                    }
                    let snap = handle.snapshot();
                    let ready_bounds = nd.last_bounds.filter(|r| !r.is_empty());
                    if snap.state == SessionState::Alive
                        && !snap.navigation.is_loading
                        && !snap.navigation.address.is_empty()
                        && let Some(child_hwnd) = rt.native_browser_hwnd(id)
                        && nd.bounds_commands_sent > 0
                        && let Some(rect) = ready_bounds
                    {
                        life.first_paint = true;
                        println!(
                            "COMPONENT_DEMO_NATIVE_HWND_READY runtime={} browser={} parent_hwnd=0x{:x} child_hwnd=0x{:x} width={} height={}",
                            id.runtime().value(),
                            id.local(),
                            nd.parent_hwnd.get(),
                            child_hwnd.get(),
                            rect.width,
                            rect.height,
                        );
                        if let Some(gate) = &config.observer_gate {
                            announce(gate)?;
                            pending_observer_until = Some(Instant::now() + STARTUP_BUDGET);
                        } else if let Some(duration) = config.smoke {
                            smoke_end = Some(Instant::now() + duration);
                        }
                    } else if Instant::now() >= startup {
                        return Err("demo first native child HWND readiness timed out".into());
                    }
                }
            }
        }
        if let Some(until) = pending_observer_until {
            let gate = config.observer_gate.as_ref().expect("observer gate armed");
            if gate.is_file() {
                if std::fs::read_to_string(gate)
                    .map_err(|e| format!("observer release: {e}"))?
                    .trim()
                    != "release"
                {
                    return Err("invalid owned smoke observer release".into());
                }
                smoke_end = config.smoke.map(|d| Instant::now() + d);
                pending_observer_until = None;
            } else if Instant::now() >= until {
                return Err("owned smoke observer timed out".into());
            }
        }
        if smoke_end.is_some_and(|end| Instant::now() >= end) {
            return Ok(());
        }
        let next = if !life.first_paint {
            Some(startup)
        } else if let Some(until) = pending_observer_until {
            Some(until.min(Instant::now() + Duration::from_millis(25)))
        } else {
            smoke_end
        };
        let _ = service(cx, rt, receiver, next).await?;
    }
}

fn detach(cx: &mut AsyncApp, root: WindowHandle<Root>, life: &mut ViewLife) -> Result<(), String> {
    if let Some(mut ds) = life.drop_target.take() {
        let _ = ds.retire();
    }
    life.released = root
        .update(cx, |root, window, cx| {
            root.message = "Closing CEF…";
            cx.notify();
            let Some(view) = root.browser.take() else {
                return Ok::<Option<ViewDiagnostics>, String>(None);
            };
            let result = match view {
                DemoView::Osr(view) => view.update(cx, |view, cx| {
                    view.release_image(window, cx)
                        .map_err(|e| format!("demo release image: {e:?}"))?;
                    Ok::<_, String>(Some(view.diagnostics()))
                })?,
                DemoView::Native(view) => {
                    view.update(cx, |view, _| view.retire());
                    None
                }
            };
            cx.notify();
            Ok(result)
        })
        .map_err(|e| e.to_string())??;
    Ok(())
}

async fn drain(
    cx: &mut AsyncApp,
    rt: &mut Runtime,
    receiver: &UiWakeReceiver,
    root: WindowHandle<Root>,
    smoke: bool,
) -> Result<(), String> {
    let deadline = Instant::now() + DRAIN_BUDGET;
    let ticket = rt.request_shutdown(CloseMode::Graceful, deadline);
    let mut announced = false;
    loop {
        let status = rt.poll_shutdown(&ticket);
        match status {
            ShutdownStatus::Ready => {
                rt.finish_shutdown()
                    .map_err(|e| format!("demo finish: {e:?}"))?;
                return Ok(());
            }
            ShutdownStatus::ProcessExitRequired { .. } => {
                return Err(format!("demo Runtime requires process exit: {status:?}"));
            }
            ShutdownStatus::Blocked { .. } if smoke => {
                return Err(format!("demo smoke shutdown blocked: {status:?}"));
            }
            ShutdownStatus::Blocked { .. } if !announced => {
                eprintln!("demo shutdown still draining: {status:?}");
                // An interactive deadline diagnoses pending native ownership,
                // never authorizes closing the application on that ownership.
                let _ = root.update(cx, |root, _, cx| {
                    root.message = "CEF is still shutting down; waiting for native work.";
                    cx.notify();
                });
                announced = true;
            }
            ShutdownStatus::Stopped | ShutdownStatus::NotRequested => {
                return Err(format!("unexpected demo shutdown status: {status:?}"));
            }
            _ => {}
        }
        service(cx, rt, receiver, (!announced).then_some(deadline)).await?;
    }
}

fn js_bridge_demo_settings() -> RuntimeSettings {
    let mut assets = std::collections::BTreeMap::new();
    assets.insert(
        "index.html".into(),
        MemoryAsset::html(
            br#"<!doctype html><body style="margin:0;background:#1a4c7e;color:#f3f4f6;font-family:sans-serif;padding:20px"><h2>app://demo JS Bridge</h2><pre id="out">connecting...</pre><script>
(async () => {
  const out = document.getElementById('out');
  const res = await fetch('app://demo/info.json');
  const info = await res.json();
  let hostAck = 'pending';
  if (window.rustCef) {
    window.rustCef.onMessage((msg) => {
      hostAck = msg;
      out.textContent = JSON.stringify({ info, ack: JSON.parse(msg) }, null, 2);
      document.title = 'demo-js:acked';
    });
    window.rustCef.postMessage(JSON.stringify({ from: 'demo-page', ok: info.confined === true }));
  }
  out.textContent = JSON.stringify({ info, hostAck }, null, 2);
})();
</script></body>"#
                .to_vec(),
        ),
    );
    assets.insert(
        "info.json".into(),
        MemoryAsset::json(br#"{"scheme":"app","confined":true}"#.to_vec()),
    );
    RuntimeSettings {
        custom_schemes: vec![CustomSchemeConfig {
            scheme: "app".into(),
            options: CustomSchemeOptions::standard_app(),
            domain: Some("demo".into()),
            asset_source: Some(SchemeAssetSource::Memory(assets)),
        }],
        ..Default::default()
    }
}

async fn drive(
    cx: &mut AsyncApp,
    root: WindowHandle<Root>,
    config: Config,
    close: Rc<Cell<bool>>,
    sender: Arc<UiWakeSender>,
    receiver: UiWakeReceiver,
) -> i32 {
    let obs = Arc::new(Observations::default());
    let mut rt = match Runtime::start(
        RuntimeConfig {
            files: RuntimeFiles {
                directory: config.bundle.clone(),
                helper_executable: config.helper.clone(),
            },
            root_cache_path: config.cache_root.clone(),
            thread_mode: config.mode,
            default_profile: if config.custom_url.is_some() {
                ProfileStorage::Persistent {
                    directory: config.cache_root.clone(),
                }
            } else {
                ProfileStorage::Memory
            },
            content_policy: if config.custom_url.is_some() {
                TrustedContentPolicy::allow_all_http_https(true)
            } else {
                TrustedContentPolicy {
                    allowed_origins: if config.js_bridge_page {
                        vec!["app://demo".into()]
                    } else {
                        vec![]
                    },
                    allow_embedded_data: true,
                }
            },
            settings: {
                let mut settings = if config.js_bridge_page {
                    js_bridge_demo_settings()
                } else {
                    Default::default()
                };
                settings.windowless_rendering_enabled = config.host_mode == HostMode::Osr;
                settings
            },
        },
        RuntimeServices {
            events: obs.clone(),
            frames: Arc::new(IgnoreFrame),
            ui: sender,
        },
    ) {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("component demo start: {e:?}");
            return match e {
                StartError::ProcessExitRequired { exit_code }
                | StartError::InitializeFailed { exit_code } => {
                    exit_code.filter(|code| *code != 0).unwrap_or(1)
                }
                _ => 1,
            };
        }
    };
    let mut life = ViewLife::default();
    let live = live(
        cx, &mut rt, &receiver, root, &close, &obs, &config, &mut life,
    )
    .await;
    let detached = detach(cx, root, &mut life);
    let drained = drain(cx, &mut rt, &receiver, root, config.smoke.is_some()).await;
    if let Err(e) = drained {
        eprintln!("{e}");
        // Do not release a still-initialized native Runtime on an incomplete
        // smoke deadline or owner failure. Exit only as an explicit failure.
        std::mem::forget(rt);
        return 1;
    }
    if let Err(e) = live.and(detached) {
        eprintln!("{e}");
        return 1;
    }
    if let Some(error) = obs
        .first_fault
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        eprintln!("{error}");
        return 1;
    }
    let d = rt.diagnostics();
    let created = obs.created.load(Ordering::Acquire);
    let closed = obs.closed.load(Ordering::Acquire);
    let Some(budget) = life
        .browser
        .and_then(|id| rt.frame_budget_stats(id).ok())
        .map(|b| b.used)
        .or_else(|| (created == 0).then_some(0))
    else {
        eprintln!("demo retirement has no original budget observation");
        return 1;
    };
    let images = life
        .released
        .as_ref()
        .map_or((0, 0), |v| (v.created_images, v.released_images));
    let native_zero_osr = match (config.host_mode, life.browser) {
        (HostMode::NativeHwnd, Some(id)) => {
            let cap_ok = rt.frame_capture_diagnostics(id).is_ok_and(|c| {
                c.software_captures == 0 && c.d3d11_captures == 0 && !c.native_resources_live
            });
            rt.native_browser_hwnd(id).is_none() && cap_ok && images == (0, 0)
        }
        (HostMode::NativeHwnd, None) => images == (0, 0),
        (HostMode::Osr, _) => true,
    };
    let mode_smoke_ok = match config.host_mode {
        HostMode::Osr => {
            config.smoke.is_none() || (life.first_paint && created == 1 && images.0 > 0)
        }
        HostMode::NativeHwnd => config.smoke.is_none() || (life.first_paint && created == 1),
    };
    if d.cef_shutdown_calls != 1
        || d.native_calls_after_shutdown != 0
        || created != closed
        || budget != 0
        || d.tasks.pending != 0
        || d.tasks.posted != d.tasks.completed
        || d.tasks.post_failed != 0
        || d.pending_requests != 0
        || d.pending_profiles != 0
        || d.pending_cookie_flushes != 0
        || d.profile_references_retained != 0
        || d.root_lease_held
        || d.app_reference_retained
        || images.0 != images.1
        || !native_zero_osr
        || !mode_smoke_ok
    {
        eprintln!(
            "component demo termination evidence incomplete: {d:?}, created={created}, closed={closed}, budget={budget}, images={images:?}"
        );
        return 1;
    }
    if let Some(released) = life.released.as_ref()
        && released.zoom_gate.requests > 0
    {
        let stats = released.zoom_gate;
        println!(
            "COMPONENT_DEMO_ZOOM_GATE requests={} dispatched={} coalesced={} frame_ack_flushes={} timeout_flushes={} final_zoom_pct={}",
            stats.requests,
            stats.dispatched,
            stats.coalesced,
            stats.frame_ack_flushes,
            stats.timeout_flushes,
            zoom_level_to_percent(f64::from(released.zoom_level_milli) / 1000.0).round() as i32,
        );
    }
    println!(
        "COMPONENT_DEMO_STOP mode={:?} backend={:?} main={} ui={} shutdown_thread={} pump={} created={created} closed={closed} tasks={} images={} final_cpu={budget} shutdown={} native_after={}",
        d.mode,
        d.observed_backend,
        d.main_thread_id,
        d.cef_ui_thread_id.unwrap_or(0),
        d.shutdown_thread_id.unwrap_or(0),
        d.manual_pump_count,
        d.tasks.completed,
        images.0,
        d.cef_shutdown_calls,
        d.native_calls_after_shutdown
    );
    0
}

pub(super) fn run(config: Config) -> i32 {
    if config.cef_example {
        let host_mode = match config.host_mode {
            crate::cli::HostMode::Osr => crate::cef_example::model::ExampleHostMode::Osr,
            crate::cli::HostMode::NativeHwnd => {
                crate::cef_example::model::ExampleHostMode::NativeHwnd
            }
        };
        return crate::cef_example::run(crate::cef_example::model::ExampleConfig {
            bundle: config.bundle,
            helper: config.helper,
            cache_root: config.cache_root,
            mode: config.mode,
            backend: config.backend,
            host_mode,
            context_menu: config.context_menu,
            initial_url: config.custom_url,
            smoke: config.smoke,
            verify_tests: false,
        });
    }
    if config.browser_shell {
        return crate::shell::run(config);
    }
    let result = Rc::new(RefCell::new(None));
    let final_result = result.clone();
    gpui_platform::application().run(move |cx| {
        // Compatibility no-op: the default menu presenter uses GPUI primitives
        // directly and requires no third-party UI library initialization.
        if config.context_menu {
            rust_cef_gpui::init_default_menu_ui(cx);
        }
        let (sender, receiver) = dispatcher::channel();
        let sender = Arc::new(sender);
        let close = Rc::new(Cell::new(false));
        let allow_close = Rc::new(Cell::new(false));
        let (requested, wake, can_close) = (close.clone(), sender.clone(), allow_close.clone());
        let root = match cx.open_window(
            WindowOptions {
                focus: false,
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(800.), px(560.)),
                    cx,
                ))),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("GPUI-CEF component demo".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |window, cx| {
                window.on_window_should_close(cx, move |_, _| {
                    if can_close.get() {
                        return true;
                    }
                    requested.set(true);
                    let _ = wake.wake();
                    false
                });
                cx.new(|_| Root {
                    browser: None,
                    menu: None,
                    message: "Starting CEF…",
                    ux_controls: config.ux_controls,
                })
            },
        ) {
            Ok(root) => root,
            Err(e) => {
                eprintln!("component demo window: {e}");
                *final_result.borrow_mut() = Some(1);
                cx.quit();
                return;
            }
        };
        cx.spawn(async move |cx: &mut AsyncApp| {
            let code = drive(cx, root, config, close, sender, receiver).await;
            *final_result.borrow_mut() = Some(code);
            allow_close.set(true);
            cx.update(|app: &mut App| app.quit());
        })
        .detach();
    });
    result.take().unwrap_or(1)
}

#[cfg(test)]
mod win32_menu_gpui_tests {
    use super::*;
    use rust_cef_gpui::menu::{MenuCommand, MenuItem, MenuModel};
    use rust_cef_gpui::win32_menu::{Win32ContextMenuHost, Win32MenuAnchor, Win32MenuRequest};

    #[link(name = "user32")]
    unsafe extern "system" {
        fn FindWindowW(class_name: *const u16, window_name: *const u16) -> isize;
    }

    #[test]
    fn gpui_async_loop_observes_win32_menu_rects_while_modal_open() {
        let ticks_while_open = Rc::new(Cell::new(0u64));
        let obs_calls = Rc::new(Cell::new(0u64));
        let saw_all_bounds = Rc::new(Cell::new(false));
        let last_obs_dbg = Rc::new(RefCell::new(String::new()));

        let t_out = ticks_while_open.clone();
        let o_out = obs_calls.clone();
        let b_out = saw_all_bounds.clone();
        let d_out = last_obs_dbg.clone();

        gpui_platform::application().run(move |cx| {
            let root = cx
                .open_window(
                    WindowOptions {
                        focus: false,
                        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                            None,
                            size(px(800.), px(560.)),
                            cx,
                        ))),
                        titlebar: Some(gpui::TitlebarOptions {
                            title: Some("GPUI Win32Menu Probe".into()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    |_, cx| {
                        cx.new(|_| Root {
                            browser: None,
                            menu: None,
                            message: "probe",
                            ux_controls: false,
                        })
                    },
                )
                .expect("open GPUI window");

            let scale = root
                .update(cx, |_, window, _| window.scale_factor())
                .expect("read scale");
            let title_u16: Vec<u16> = "GPUI Win32Menu Probe\0".encode_utf16().collect();
            let owner_hwnd = unsafe { FindWindowW(std::ptr::null(), title_u16.as_ptr()) };
            assert_ne!(owner_hwnd, 0, "GPUI probe window HWND must be found");

            let host = Rc::new(Win32ContextMenuHost::new().expect("win32 host"));
            let host_task = host.clone();
            let ticks = ticks_while_open.clone();
            let obss = obs_calls.clone();
            let bounds_ok = saw_all_bounds.clone();
            let dbg_out = last_obs_dbg.clone();

            cx.spawn(async move |cx: &mut AsyncApp| {
                let anchor = Win32MenuAnchor::from_client_dip(owner_hwnd, 60.0, 40.0, scale)
                    .expect("anchor");
                let model = MenuModel {
                    x: 60,
                    y: 40,
                    entries: vec![
                        MenuItem::Command(MenuCommand {
                            command_id: 110,
                            label: "&Undo".into(),
                            enabled: false,
                            visible: true,
                            checked: None,
                            radio_group: None,
                        }),
                        MenuItem::Separator { visible: true },
                        MenuItem::Command(MenuCommand {
                            command_id: 112,
                            label: "Cu&t".into(),
                            enabled: true,
                            visible: true,
                            checked: None,
                            radio_group: None,
                        }),
                        MenuItem::Command(MenuCommand {
                            command_id: 117,
                            label: "Select &All".into(),
                            enabled: true,
                            visible: true,
                            checked: None,
                            radio_group: None,
                        }),
                        MenuItem::Separator { visible: true },
                        MenuItem::Command(MenuCommand {
                            command_id: 28440,
                            label: "Inspect".into(),
                            enabled: true,
                            visible: true,
                            checked: None,
                            radio_group: None,
                        }),
                        MenuItem::Command(MenuCommand {
                            command_id: 28500,
                            label: "Host Toggle".into(),
                            enabled: true,
                            visible: true,
                            checked: Some(true),
                            radio_group: None,
                        }),
                        MenuItem::Submenu(rust_cef_gpui::menu::MenuSubmenu {
                            label: "More".into(),
                            enabled: true,
                            visible: true,
                            entries: vec![rust_cef_gpui::menu::MenuChild::Command(MenuCommand {
                                command_id: 28501,
                                label: "Sub Item".into(),
                                enabled: true,
                                visible: true,
                                checked: None,
                                radio_group: None,
                            })],
                        }),
                    ],
                };

                let obss_cb = obss.clone();
                let bounds_cb = bounds_ok.clone();
                let dbg_cb = dbg_out.clone();
                let tok = host_task
                    .schedule_open(
                        Win32MenuRequest { anchor, model },
                        Box::new(move |obs| {
                            obss_cb.set(obss_cb.get() + 1);
                            let converted: Vec<_> = obs
                                .items
                                .iter()
                                .map(|it| {
                                    (
                                        it.command_id,
                                        it.rect.and_then(|r| r.to_client_dip(owner_hwnd, scale)),
                                    )
                                })
                                .collect();
                            *dbg_cb.borrow_mut() =
                                format!("menu_rect={:?} dip={:?}", obs.menu_rect, converted);
                            if obs.menu_rect.is_some()
                                && converted.iter().any(|(id, b)| *id == 28500 && b.is_some())
                            {
                                bounds_cb.set(true);
                            }
                        }),
                        Box::new(|_| {}),
                    )
                    .expect("schedule_open");

                let deadline = Instant::now() + Duration::from_millis(800);
                while Instant::now() < deadline {
                    cx.background_executor()
                        .timer(Duration::from_millis(4))
                        .await;
                    let _ = root.update(cx, |_, window, _| window.scale_factor());
                    if host_task.is_modal_active() {
                        ticks.set(ticks.get() + 1);
                    }
                    if bounds_ok.get() && ticks.get() >= 10 {
                        break;
                    }
                }
                host_task.cancel(Some(tok));
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let _ = cx.update(|app: &mut App| app.quit());
            })
            .detach();
        });

        assert!(o_out.get() > 0, "on_observe never fired under GPUI");
        assert!(
            b_out.get(),
            "bounds never populated under GPUI: {}",
            d_out.borrow()
        );
        assert!(
            t_out.get() > 0,
            "GPUI async task did not tick while TrackPopupMenuEx was open"
        );
    }

    #[test]
    fn observations_stays_strict_because_kernel_shields_recoverable_resize_events() {
        use rust_cef_gpui::contracts::RuntimeGeneration;
        use rust_cef_gpui::ports::{CaptureFailureKind, EngineError};

        assert!(CaptureFailureKind::IncompleteFrame.is_recoverable_resize_transition());
        assert!(CaptureFailureKind::ResizeExhausted.is_recoverable_resize_transition());
        assert!(CaptureFailureKind::Busy.is_recoverable_resize_transition());
        assert!(!CaptureFailureKind::DeviceRemoved.is_recoverable_resize_transition());

        assert!(
            EngineError::Capture {
                kind: CaptureFailureKind::IncompleteFrame,
                code: None,
            }
            .is_recoverable_capture()
        );
        assert!(
            !EngineError::Capture {
                kind: CaptureFailureKind::IncompleteFrame,
                code: Some(-1),
            }
            .is_recoverable_capture()
        );

        let browser = BrowserId::new(RuntimeGeneration::new(1), 1);
        let obs = Observations::default();
        obs.publish(BrowserEventEnvelope {
            browser,
            sequence: 1,
            event: BrowserEvent::CaptureFailed(EngineError::Capture {
                kind: CaptureFailureKind::DeviceRemoved,
                code: None,
            }),
        });
        assert!(
            obs.first_fault
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
        );
    }
}
