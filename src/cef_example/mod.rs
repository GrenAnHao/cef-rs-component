//! GPUI desktop browser example built over public
//! `rust-cef-gpui` and `rust-cef-core` APIs.

pub mod assets;
pub mod jsb_rpc;
pub mod model;
pub(crate) mod text_input;
pub mod ui;
mod verify;

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    future::Future,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    task::Poll,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App, AppContext as _, AsyncApp, Bounds, Entity, FocusHandle, Focusable as _,
    IntoElement as _, Window, WindowBounds, WindowHandle, WindowOptions, px, size,
};
use rust_cef_gpui::{
    CefBrowser, ContextMenuConfig, ContextMenuRegistration, ContextMenuServices,
    DefaultMenuPresenter, NativeCefBrowser, NativeDropTargetServices, PdfExportStatus,
    PdfExportTicket, Runtime, RuntimeServices, ViewDiagnostics,
    contracts::{
        BrowserConfig, BrowserId, BrowserOptions, BrowserSurfaceMode, CefFeatureState,
        NativeChildHwndConfig, PixelRect, ProfileStorage, RenderPreference, RuntimeConfig,
        RuntimeFiles, RuntimeSettings, RuntimeState, SessionState, ThreadMode,
        TrustedContentPolicy, Viewport, step_canonical_zoom_level,
    },
    dispatcher::{self, UiWakeReceiver, UiWakeSender},
    host_capabilities::{
        AuthDecision, AuthRequest, AuthService, DownloadProgress, DownloadRequest,
        DownloadService, HostCapabilities, JsDialogDecision, JsDialogKind, JsDialogRequest,
        JsDialogService, PdfExportOptions, PopupDecision, PopupDisposition, PopupPolicy,
        PopupRequest, PopupService,
    },
    js_bridge::{EvalOutcome, EvalRequestId, JsBridgeConfig, JsFrameScope, MAX_JS_PAYLOAD_BYTES},
    menu::{MenuContextLimits, MenuLimits},
    outbound_drag_contracts::OutboundDragPolicy,
    ports::{
        BrowserEvent, BrowserEventEnvelope, BrowserEventSink, BrowserHandle, CloseMode, FrameSink,
        OwnedFrame, PublishResult, UiDispatcher as _,
    },
    runtime::{ShutdownStatus, StartError},
};

use assets::{
    BENCHMARK_SUITE_URL, CUSTOM_POST_URL, DEFAULT_HOME_URL, RESPONSE_FILTER_TEST_URL,
    ExampleResourceInterceptor, build_custom_schemes, format_runtime_summary,
};
use jsb_rpc::CefRsGpuiRpcHost;
use model::{
    ActiveInput, BitmapScalingKind, NoticeDialog, PendingJsDialog, ExampleAppModel, ExampleConfig,
    ExampleHostMode, normalize_user_url,
};
use ui::CefExampleRoot;

const MAX_EXAMPLE_TABS: usize = model::MAX_EXAMPLE_TABS;
const STARTUP_BUDGET: Duration = Duration::from_secs(30);
const DRAIN_BUDGET: Duration = Duration::from_secs(15);
const PUMP_PERIOD: Duration = Duration::from_millis(4);

#[derive(Clone, Debug)]
pub(super) enum ExampleAction {
    NewTab(Option<String>),
    NewTabLegacy(String),
    SwitchTab(u64),
    CloseTab(u64),
    NavigateHome,
    NavigateActive(String),
    Back,
    Forward,
    Reload,
    StopLoading,
    ZoomSteps(i32),
    ZoomReset,
    ToggleAudioMute,
    ToggleDevTools,
    ClearHttpAuthCredentials,
    ToggleSidebar,
    ToggleDownloadInfo,
    ExecuteJs(String),
    EvaluateJs(String),
    PostJsToTab { tab_id: u64, payload_json: String },
    SetRenderAngle(f32),
    SetWatermarkOpacity(f32),
    SetBitmapScaling(BitmapScalingKind),
    LoadCustomPost,
    RunBindingStressTest,
    ResizeHackTests,
    ExportPdfPrompt,
    ExportPdfToPath(PathBuf),
    DismissNoticeDialog,
    ResolveJsDialog { accept: bool },
    ExitApp,
}

#[derive(Clone)]
pub(super) enum ExampleView {
    Osr(Entity<CefBrowser>),
    Native(Entity<NativeCefBrowser>),
}

impl ExampleView {
    pub(super) fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            Self::Osr(v) => v.read(cx).focus_handle(cx),
            Self::Native(v) => v.read(cx).focus_handle(cx),
        }
    }

    pub(super) fn set_visible(&self, visible: bool, cx: &mut App) {
        if let Self::Native(v) = self {
            v.update(cx, |view, cx| view.set_visible(visible, cx));
        }
    }

    pub(super) fn consume_and_sample(
        &self,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<ViewDiagnostics> {
        match self {
            Self::Osr(v) => v.update(cx, |view, cx| {
                let _ = view.consume(window, cx);
                Some(view.diagnostics())
            }),
            Self::Native(v) => v.update(cx, |view, cx| {
                let _ = view.consume(window, cx);
                None
            }),
        }
    }

    pub(super) fn release_or_retire(
        self,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<ViewDiagnostics> {
        match self {
            Self::Osr(v) => Some(v.update(cx, |view, cx| {
                let _ = view.release_image(window, cx);
                view.diagnostics()
            })),
            Self::Native(v) => {
                v.update(cx, |view, _| view.retire());
                None
            }
        }
    }

    pub(super) fn zoom_level(&self, cx: &App) -> f64 {
        match self {
            Self::Osr(v) => v.read(cx).zoom_level(),
            Self::Native(v) => v.read(cx).zoom_level(),
        }
    }

    pub(super) fn step_zoom(&self, steps: i32, window: &mut Window, cx: &mut App) {
        match self {
            Self::Osr(v) => v.update(cx, |view, cx| view.step_zoom(steps, window, cx)),
            Self::Native(v) => v.update(cx, |view, _| {
                let next = step_canonical_zoom_level(view.zoom_level(), steps);
                let _ = view.set_zoom_level(next);
            }),
        }
    }

    pub(super) fn reset_zoom(&self, window: &mut Window, cx: &mut App) {
        match self {
            Self::Osr(v) => v.update(cx, |view, cx| view.reset_zoom(window, cx)),
            Self::Native(v) => v.update(cx, |view, _| {
                let _ = view.set_zoom_level(0.0);
            }),
        }
    }

    pub(super) fn toggle_audio_muted(&self, cx: &mut App) {
        match self {
            Self::Osr(v) => v.update(cx, |view, _| {
                let _ = view.set_audio_muted(!view.audio_muted());
            }),
            Self::Native(v) => v.update(cx, |view, _| {
                let _ = view.set_audio_muted(!view.audio_muted());
            }),
        }
    }

    pub(super) fn load_url(&self, url: String, cx: &mut App) {
        match self {
            Self::Osr(v) => v.update(cx, |view, _| {
                let _ = view.load_url(url);
            }),
            Self::Native(v) => v.update(cx, |view, _| {
                let _ = view.load_url(url);
            }),
        }
    }

    pub(super) fn load_url_with_post_data(
        &self,
        url: String,
        post_data: Vec<u8>,
        content_type: Option<String>,
        cx: &mut App,
    ) {
        match self {
            Self::Osr(v) => v.update(cx, |view, _| {
                let _ = view.load_url_with_post_data(url, post_data, content_type);
            }),
            Self::Native(v) => v.update(cx, |view, _| {
                let _ = view.load_url_with_post_data(url, post_data, content_type);
            }),
        }
    }

    pub(super) fn sync_osr_rendering(
        &self,
        angle_deg: f32,
        opacity: f32,
        scaling: BitmapScalingKind,
        cx: &mut App,
    ) {
        if let Self::Osr(v) = self {
            v.update(cx, |view, cx| {
                view.set_rotation_angle_deg(angle_deg);
                view.set_surface_opacity(opacity);
                view.set_nearest_neighbor_scaling(scaling == BitmapScalingKind::NearestNeighbor);
                view.set_transparent_backdrop(true);
                cx.notify();
            });
        }
    }

    pub(super) fn go_back(&self, cx: &mut App) {
        match self {
            Self::Osr(v) => v.update(cx, |view, _| {
                let _ = view.go_back();
            }),
            Self::Native(v) => v.update(cx, |view, _| {
                let _ = view.go_back();
            }),
        }
    }

    pub(super) fn go_forward(&self, cx: &mut App) {
        match self {
            Self::Osr(v) => v.update(cx, |view, _| {
                let _ = view.go_forward();
            }),
            Self::Native(v) => v.update(cx, |view, _| {
                let _ = view.go_forward();
            }),
        }
    }

    pub(super) fn reload(&self, cx: &mut App) {
        match self {
            Self::Osr(v) => v.update(cx, |view, _| {
                let _ = view.reload();
            }),
            Self::Native(v) => v.update(cx, |view, _| {
                let _ = view.reload();
            }),
        }
    }

    pub(super) fn stop_loading(&self, cx: &mut App) {
        match self {
            Self::Osr(v) => v.update(cx, |view, _| {
                let _ = view.stop_loading();
            }),
            Self::Native(v) => v.update(cx, |view, _| {
                let _ = view.stop_loading();
            }),
        }
    }

    pub(super) fn execute_javascript(&self, script: String, cx: &mut App) {
        match self {
            Self::Osr(v) => v.update(cx, |view, _| {
                let _ = view.execute_javascript(script, JsFrameScope::MainFrame);
            }),
            Self::Native(v) => v.update(cx, |view, _| {
                let _ = view.execute_javascript(script, JsFrameScope::MainFrame);
            }),
        }
    }

    pub(super) fn evaluate_script(&self, script: String, timeout_ms: u64, cx: &mut App) -> Option<EvalRequestId> {
        match self {
            Self::Osr(v) => v.update(cx, |view, _| {
                view.evaluate_script(script, JsFrameScope::MainFrame, timeout_ms).ok()
            }),
            Self::Native(v) => v.update(cx, |view, _| {
                view.evaluate_script(script, JsFrameScope::MainFrame, timeout_ms).ok()
            }),
        }
    }

    pub(super) fn post_js_message(&self, payload_json: String, cx: &mut App) {
        match self {
            Self::Osr(v) => v.update(cx, |view, _| {
                let _ = view.post_js_message(payload_json);
            }),
            Self::Native(v) => v.update(cx, |view, _| {
                let _ = view.post_js_message(payload_json);
            }),
        }
    }

    pub(super) fn into_any_element(self) -> AnyElement {
        match self {
            Self::Osr(v) => v.into_any_element(),
            Self::Native(v) => v.into_any_element(),
        }
    }
}

#[derive(Default)]
struct ExampleCapQueues {
    downloads: Mutex<VecDeque<(u64, DownloadRequest)>>,
    progress: Mutex<VecDeque<DownloadProgress>>,
    js_dialogs: Mutex<VecDeque<JsDialogRequest>>,
    auths: Mutex<VecDeque<AuthRequest>>,
    popups: Mutex<VecDeque<PopupRequest>>,
    js_rpcs: Mutex<VecDeque<(u64, String)>>,
    evals: Mutex<VecDeque<(u64, u64, EvalOutcome)>>,
}

struct ExampleCapAdapter {
    queues: Arc<ExampleCapQueues>,
    wake: Arc<UiWakeSender>,
}

impl DownloadService for ExampleCapAdapter {
    fn on_before_download(&self, request: DownloadRequest) {
        let seq = request.id.sequence;
        self.queues
            .downloads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back((seq, request));
        let _ = self.wake.wake();
    }

    fn on_download_updated(&self, progress: DownloadProgress) {
        self.queues
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(progress);
        let _ = self.wake.wake();
    }
}

impl JsDialogService for ExampleCapAdapter {
    fn on_js_dialog(&self, request: JsDialogRequest) {
        self.queues
            .js_dialogs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(request);
        let _ = self.wake.wake();
    }
}

impl PopupService for ExampleCapAdapter {
    fn on_popup_request(&self, request: PopupRequest) {
        self.queues
            .popups
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(request);
        let _ = self.wake.wake();
    }
}

impl AuthService for ExampleCapAdapter {
    fn on_auth_credentials(&self, request: AuthRequest) {
        self.queues
            .auths
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(request);
        let _ = self.wake.wake();
    }
}

struct ExampleObservations {
    strict_capture: bool,
    created: AtomicU64,
    closed: AtomicU64,
    first_fault: Mutex<Option<String>>,
}

impl ExampleObservations {
    fn new(strict_capture: bool) -> Self {
        Self {
            strict_capture,
            created: AtomicU64::new(0),
            closed: AtomicU64::new(0),
            first_fault: Mutex::new(None),
        }
    }
}

impl BrowserEventSink for ExampleObservations {
    fn publish(&self, event: BrowserEventEnvelope) -> PublishResult {
        match event.event {
            BrowserEvent::Created => {
                self.created.fetch_add(1, Ordering::Release);
            }
            BrowserEvent::Closed => {
                self.closed.fetch_add(1, Ordering::Release);
            }
            BrowserEvent::CaptureFailed(ref err)
                if !self.strict_capture && err.is_recoverable_capture() => {}
            BrowserEvent::RendererTerminated { .. } if !self.strict_capture => {}
            fault @ (BrowserEvent::CreateFailed(_)
            | BrowserEvent::CaptureFailed(_)
            | BrowserEvent::RendererTerminated { .. }) => {
                let message = format!("cef example Browser {:?}: {fault:?}", event.browser);
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

struct TabRuntime {
    browser_id: BrowserId,
    handle: BrowserHandle,
    first_paint: bool,
    menu_reg: Option<ContextMenuRegistration>,
    smoke_eval_id: Option<EvalRequestId>,
    smoke_rf_eval_id: Option<EvalRequestId>,
    smoke_bench_eval_id: Option<EvalRequestId>,
    smoke_post_eval_id: Option<EvalRequestId>,
}

#[derive(Default)]
struct ExampleAccounting {
    tabs_opened: u64,
    tabs_closed: u64,
    created_images: u64,
    released_images: u64,
}

macro_rules! wire_view_callbacks {
    ($view:expr, $tab_id:expr, $root_weak:expr, $cap_queues:expr, $wake:expr, $log_console:expr) => {{
        let root_w = $root_weak.clone();
        $view.set_on_title_changed(Some(Box::new(move |title, _, _, app| {
            let rw = root_w.clone();
            app.defer(move |app| {
                let _ = rw.update(app, |root, cx| {
                    if let Some(t) = root.model.tab_mut($tab_id)
                        && !title.is_empty()
                    {
                        t.title = title;
                        cx.notify();
                    }
                });
            });
        })));

        let root_w = $root_weak.clone();
        $view.set_on_address_changed(Some(Box::new(move |addr, _, _, app| {
            let rw = root_w.clone();
            app.defer(move |app| {
                let _ = rw.update(app, |root, cx| {
                    let editing = root.model.active_tab == Some($tab_id)
                        && root.model.active_input == ActiveInput::Address;
                    if let Some(t) = root.model.tab_mut($tab_id) {
                        t.address = addr.clone();
                        if !editing {
                            t.address_input.set_from_external(&addr);
                        }
                        cx.notify();
                    }
                });
            });
        })));

        let root_w = $root_weak.clone();
        $view.set_on_loading_state_changed(Some(Box::new(move |state, _, _, app| {
            let rw = root_w.clone();
            app.defer(move |app| {
                let _ = rw.update(app, |root, cx| {
                    if let Some(t) = root.model.tab_mut($tab_id) {
                        t.is_loading = state.is_loading;
                        t.can_go_back = state.can_go_back;
                        t.can_go_forward = state.can_go_forward;
                        cx.notify();
                    }
                });
            });
        })));

        let root_w = $root_weak.clone();
        $view.set_on_loading_progress_changed(Some(Box::new(move |pct, _, _, app| {
            let rw = root_w.clone();
            app.defer(move |app| {
                let _ = rw.update(app, |root, cx| {
                    if let Some(t) = root.model.tab_mut($tab_id) {
                        t.loading_progress_per_mille = ((pct.clamp(0.0, 1.0)) * 1000.0).round() as u16;
                        cx.notify();
                    }
                });
            });
        })));

        let root_w = $root_weak.clone();
        $view.set_on_status_message(Some(Box::new(move |msg, _, _, app| {
            let rw = root_w.clone();
            app.defer(move |app| {
                let _ = rw.update(app, |root, cx| {
                    if let Some(t) = root.model.tab_mut($tab_id) {
                        t.status_message = msg;
                        cx.notify();
                    }
                });
            });
        })));

        let root_w = $root_weak.clone();
        let log_console = $log_console;
        $view.set_on_console_message(Some(Box::new(move |info, _, _, app| {
            if log_console {
                println!(
                    "CEF_EXAMPLE_CONSOLE tab={} {:?}:{} {}",
                    $tab_id, info.severity, info.line, info.message
                );
            }
            let rw = root_w.clone();
            app.defer(move |app| {
                let _ = rw.update(app, |root, cx| {
                    if let Some(t) = root.model.tab_mut($tab_id) {
                        t.output_message = format!("Console ({:?}:{}): {}", info.severity, info.line, info.message);
                        cx.notify();
                    }
                });
            });
        })));

        let root_w = $root_weak.clone();
        $view.set_on_tooltip_changed(Some(Box::new(move |tip, _, _, app| {
            if tip.is_empty() {
                return;
            }
            let rw = root_w.clone();
            app.defer(move |app| {
                let _ = rw.update(app, |root, cx| {
                    if let Some(t) = root.model.tab_mut($tab_id) {
                        t.status_message = format!("Tooltip: {tip}");
                        cx.notify();
                    }
                });
            });
        })));

        let root_w = $root_weak.clone();
        $view.set_on_load_error(Some(Box::new(move |err, _, _, app| {
            if !err.is_main_frame || err.cancelled {
                return;
            }
            let rw = root_w.clone();
            app.defer(move |app| {
                let _ = rw.update(app, |root, cx| {
                    if let Some(t) = root.model.tab_mut($tab_id) {
                        t.output_message = format_runtime_summary(&format!("LoadError code={}", err.error_code));
                        cx.notify();
                    }
                });
            });
        })));

        let root_w = $root_weak.clone();
        $view.set_on_renderer_terminated(Some(Box::new(move |info, _, _, app| {
            let rw = root_w.clone();
            app.defer(move |app| {
                let _ = rw.update(app, |root, cx| {
                    if let Some(t) = root.model.tab_mut($tab_id) {
                        t.is_loading = false;
                        t.output_message = format!(
                            "Renderer terminated (status={}, code={:?}) — click Reload or navigate to recover",
                            info.native_status, info.error_code
                        );
                        cx.notify();
                    }
                });
            });
        })));

        let root_w = $root_weak.clone();
        $view.set_on_draggable_regions_changed(Some(Box::new(move |regions, _, _, app| {
            let count = regions.len();
            let rw = root_w.clone();
            app.defer(move |app| {
                let _ = rw.update(app, |root, cx| {
                    if let Some(t) = root.model.tab_mut($tab_id) {
                        t.draggable_region_count = count;
                        t.draggable_regions = regions;
                        t.output_message = format_runtime_summary(&format!("DraggableRegions: {count}"));
                        cx.notify();
                    }
                });
            });
        })));

        let root_w = $root_weak.clone();
        $view.set_on_uncaught_exception(Some(Box::new(move |ex, _, _, app| {
            let rw = root_w.clone();
            app.defer(move |app| {
                let _ = rw.update(app, |root, cx| {
                    if let Some(t) = root.model.tab_mut($tab_id) {
                        t.output_message = format!("Uncaught JS error ({}:{}): {}", ex.source_url, ex.line, ex.message);
                        cx.notify();
                    }
                });
            });
        })));

        let root_w = $root_weak.clone();
        $view.set_on_context_created(Some(Box::new(move |_, _, _, app| {
            let rw = root_w.clone();
            app.defer(move |app| {
                let _ = rw.update(app, |root, cx| {
                    if let Some(t) = root.model.tab_mut($tab_id) {
                        t.v8_context_count = t.v8_context_count.saturating_add(1);
                        cx.notify();
                    }
                });
            });
        })));

        let root_w = $root_weak.clone();
        $view.set_on_context_released(Some(Box::new(move |_, _, _, app| {
            let rw = root_w.clone();
            app.defer(move |app| {
                let _ = rw.update(app, |root, cx| {
                    if let Some(t) = root.model.tab_mut($tab_id) {
                        t.v8_context_count = t.v8_context_count.saturating_sub(1);
                        cx.notify();
                    }
                });
            });
        })));

        let root_w = $root_weak.clone();
        $view.set_on_focused_node_changed(Some(Box::new(move |node, _, _, app| {
            let desc = node
                .map(|n| format!("{}#{}", n.tag_name, n.element_id))
                .unwrap_or_default();
            let rw = root_w.clone();
            app.defer(move |app| {
                let _ = rw.update(app, |root, cx| {
                    if let Some(t) = root.model.tab_mut($tab_id) {
                        t.focused_node_tag = desc;
                        cx.notify();
                    }
                });
            });
        })));

        let root_w = $root_weak.clone();
        $view.set_on_policy_denied(Some(Box::new(move |action, _, _, app| {
            let rw = root_w.clone();
            app.defer(move |app| {
                let _ = rw.update(app, |root, cx| {
                    if let Some(t) = root.model.tab_mut($tab_id) {
                        t.output_message = format!("Policy denied by runtime confinement: {action:?}");
                        cx.notify();
                    }
                });
            });
        })));

        let root_w = $root_weak.clone();
        let rpc_queues = $cap_queues.clone();
        let rpc_wake = $wake.clone();
        $view.set_on_js_message(Some(Box::new(move |_origin, payload_json, _view, _, app| {
            rpc_queues
                .js_rpcs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(($tab_id, payload_json.clone()));
            let _ = rpc_wake.wake();
            let rw = root_w.clone();
            let msg_copy = payload_json;
            app.defer(move |app| {
                let _ = rw.update(app, |root, cx| {
                    if let Some(t) = root.model.tab_mut($tab_id) {
                        t.output_message = format_runtime_summary(&format!("JS msg: {msg_copy}"));
                        cx.notify();
                    }
                });
            });
        })));

        let root_w = $root_weak.clone();
        let eval_queues = $cap_queues.clone();
        let eval_wake = $wake.clone();
        $view.set_on_eval_completed(Some(Box::new(move |eval_id, outcome, _, _, app| {
            eval_queues
                .evals
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(($tab_id, eval_id.sequence, outcome.clone()));
            let _ = eval_wake.wake();
            let rw = root_w.clone();
            app.defer(move |app| {
                let _ = rw.update(app, |root, cx| {
                    let result_str = match &outcome {
                        EvalOutcome::Value(v) => v.clone(),
                        EvalOutcome::Err(e) => format!("Error: {e:?}"),
                    };
                    if let Some(t) = root.model.tab_mut($tab_id) {
                        t.evaluate_js_result = result_str.clone();
                    }
                    if $tab_id == 1 && result_str == "2" {
                        root.smoke_eval_ok = true;
                        println!("CEF_EXAMPLE_EVAL id={} result={}", eval_id.sequence, result_str);
                    }
                    if $tab_id == 2 && result_str == "\"FILTER_VERIFIED\"" {
                        root.smoke_response_filter_ok = true;
                    }
                    if $tab_id == 3 && result_str == "\"BENCHMARK_OK\"" {
                        root.smoke_benchmark_ok = true;
                    }
                    if $tab_id == 3 && result_str == "\"POST_OK\"" {
                        root.smoke_custom_post_ok = true;
                    }
                    cx.notify();
                });
            });
        })));
    }};
}

async fn wait(
    cx: &mut AsyncApp,
    receiver: &UiWakeReceiver,
    timeout: Option<Duration>,
) -> Result<bool, String> {
    let Some(timeout) = timeout else {
        return if receiver.recv().await {
            Ok(true)
        } else {
            Err("cef example wake channel closed".into())
        };
    };
    let mut wake = std::pin::pin!(receiver.recv());
    let mut timer = std::pin::pin!(cx.background_executor().timer(timeout));
    std::future::poll_fn(|cx| {
        if let Poll::Ready(ok) = wake.as_mut().poll(cx) {
            return Poll::Ready(if ok {
                Ok(true)
            } else {
                Err("cef example wake channel closed".into())
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
    rt_cell: &Rc<RefCell<Option<Runtime>>>,
    mode: ThreadMode,
    receiver: &UiWakeReceiver,
    deadline: Option<Instant>,
) -> Result<bool, String> {
    let mut timeout = deadline.map(|t| t.saturating_duration_since(Instant::now()));
    if mode == ThreadMode::Integrated {
        timeout = Some(timeout.map_or(PUMP_PERIOD, |v| v.min(PUMP_PERIOD)));
    }
    const MAX_TURN: Duration = Duration::from_millis(50);
    timeout = Some(timeout.map_or(MAX_TURN, |v| v.min(MAX_TURN)));
    let woke = wait(cx, receiver, timeout).await?;
    if mode == ThreadMode::Integrated
        && let Ok(mut guard) = rt_cell.try_borrow_mut()
        && let Some(rt) = guard.as_mut()
    {
        rt.pump().map_err(|e| format!("cef example pump: {e:?}"))?;
    }
    Ok(woke)
}

#[allow(clippy::too_many_arguments)]
fn spawn_example_tab(
    cx: &mut AsyncApp,
    rt_cell: &Rc<RefCell<Option<Runtime>>>,
    menus: Option<&Rc<RefCell<ContextMenuServices>>>,
    cap_adapter: &Arc<ExampleCapAdapter>,
    root: WindowHandle<CefExampleRoot>,
    config: &ExampleConfig,
    tab_id: u64,
    url: String,
    show_sidebar: bool,
    legacy_binding: bool,
    accounting: &mut ExampleAccounting,
    tabs: &mut BTreeMap<u64, TabRuntime>,
) -> Result<(), String> {
    let (available, scale, parent_hwnd) = root
        .update(cx, |_, window, _| {
            (
                window.viewport_size(),
                window.scale_factor(),
                rust_cef_gpui::window_hwnd(window),
            )
        })
        .map_err(|e| e.to_string())?;
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let width = (f32::from(available.width) - if show_sidebar { 304.0 } else { 0.0 })
        .round()
        .max(1.0) as i32;
    let height = (f32::from(available.height) - 122.0).round().max(1.0) as i32;
    let viewport = Viewport {
        width,
        height,
        scale,
    };

    let mut options = BrowserOptions::default()
        .with_frame_rate(rust_cef_gpui::recommended_windowless_frame_rate())
        .with_tooltip_events(true)
        .with_outbound_drag_policy(OutboundDragPolicy::AllowAsync)
        .with_accessibility_state(CefFeatureState::Enabled)
        .with_js_bridge(JsBridgeConfig {
            enabled: true,
            allowed_origins: vec![
                "https://*".into(),
                "http://*".into(),
                "custom://cef-rs-gpui".into(),
                "test://cef-rs-gpui".into(),
            ],
            max_message_bytes: MAX_JS_PAYLOAD_BYTES,
            ..Default::default()
        });
    match config.host_mode {
        ExampleHostMode::Osr => {
            let parent_hwnd = parent_hwnd.ok_or("parent HWND unavailable for OSR mode")?;
            options = options.with_parent_hwnd(parent_hwnd);
        }
        ExampleHostMode::NativeHwnd => {
            let parent_hwnd = parent_hwnd.ok_or("parent HWND unavailable for NativeHwnd mode")?;
            options = options.with_surface_mode(BrowserSurfaceMode::NativeChildHwnd(
                NativeChildHwndConfig {
                    parent_hwnd,
                    initial_bounds: PixelRect {
                        x: 0,
                        y: (98.0_f32 * scale).round() as i32,
                        width: (width as f32 * scale).round().max(1.0) as u32,
                        height: (height as f32 * scale).round().max(1.0) as u32,
                    },
                    activate_on_creation: false,
                },
            ));
        }
    }

    let (handle, attachment) = {
        let mut guard = rt_cell.borrow_mut();
        let rt = guard.as_mut().ok_or("cef example Runtime absent")?;
        if legacy_binding {
            rt.set_next_browser_legacy_binding(true);
        }
        let profile = Some(rt.default_profile_handle());
        let handle = rt
            .create_browser(BrowserConfig {
                initial_url: url.clone(),
                profile,
                render_preference: config.backend,
                viewport,
                options,
            })
            .map_err(|e| format!("create tab {tab_id}: {e:?}"))?;
        let attachment = rt
            .attach_browser(&handle)
            .map_err(|e| format!("attach tab {tab_id}: {e:?}"))?;
        (handle, attachment)
    };
    let browser_id = handle.snapshot().browser;

    let menu_reg = root
        .update(cx, |root, window, cx| {
            let root_weak = cx.entity().downgrade();
            let mut reg_out = None;
            let popup_cap_osr = cap_adapter.clone();
            let popup_cap_native = cap_adapter.clone();
            let cap_queues = cap_adapter.queues.clone();
            let cap_wake = cap_adapter.wake.clone();
            let example_view = match config.host_mode {
                ExampleHostMode::Osr => {
                    let view =
                        CefBrowser::new(attachment, window, cx).map_err(|e| e.to_string())?;
                    view.update(cx, |v, _| {
                        v.set_pinch_zoom_enabled(true);
                        v.set_ctrl_wheel_zoom_enabled(true);
                        v.set_transparent_backdrop(true);
                        v.set_surface_opacity(0.95);
                        v.set_rotation_angle_deg(0.0);
                        v.set_nearest_neighbor_scaling(false);
                        v.set_popup_policy(PopupPolicy::AllowNativePopup);
                        v.set_popup_filter(Some(Arc::new(move |req| {
                            if !req.policy_allowed {
                                return PopupDecision::Cancel;
                            }
                            match req.disposition {
                                PopupDisposition::NewForegroundTab
                                | PopupDisposition::NewBackgroundTab => {
                                    popup_cap_osr.on_popup_request(req.clone());
                                    PopupDecision::Cancel
                                }
                                _ => PopupDecision::AllowNativePopup {
                                    no_javascript_access: false,
                                },
                            }
                        })));
                        wire_view_callbacks!(v, tab_id, root_weak, cap_queues, cap_wake, config.verify_tests);
                    });
                    if let Some(menus) = menus {
                        let menu_limits = MenuLimits {
                            max_entries: std::num::NonZeroUsize::new(32).unwrap(),
                            max_label_bytes: std::num::NonZeroUsize::new(256).unwrap(),
                            max_total_label_bytes: std::num::NonZeroUsize::new(2048).unwrap(),
                        };
                        let ctx_limits = MenuContextLimits {
                            max_frame_id_bytes: std::num::NonZeroUsize::new(512).unwrap(),
                            max_text_bytes: std::num::NonZeroUsize::new(4096).unwrap(),
                            max_suggestions: std::num::NonZeroUsize::new(5).unwrap(),
                            max_total_bytes: std::num::NonZeroUsize::new(8192).unwrap(),
                        };
                        if let Ok(target) = menus.borrow_mut().register(&view, window, cx)
                            && let Some(rt) = rt_cell.borrow().as_ref()
                            && let Ok(reg) = rt.bind_context_menu(target)
                            && let Ok(presenter) =
                                DefaultMenuPresenter::new(&view, window, menu_limits, ctx_limits, cx)
                        {
                            root.menu_presenters
                                .insert(tab_id, Rc::new(RefCell::new(presenter)));
                            reg_out = Some(reg);
                        }
                    }
                    ExampleView::Osr(view)
                }
                ExampleHostMode::NativeHwnd => {
                    let view =
                        NativeCefBrowser::new(attachment, window, cx).map_err(|e| e.to_string())?;
                    view.update(cx, |v, _| {
                        v.set_popup_policy(PopupPolicy::AllowNativePopup);
                        v.set_popup_filter(Some(Arc::new(move |req| {
                            if !req.policy_allowed {
                                return PopupDecision::Cancel;
                            }
                            match req.disposition {
                                PopupDisposition::NewForegroundTab
                                | PopupDisposition::NewBackgroundTab => {
                                    popup_cap_native.on_popup_request(req.clone());
                                    PopupDecision::Cancel
                                }
                                _ => PopupDecision::AllowNativePopup {
                                    no_javascript_access: false,
                                },
                            }
                        })));
                        wire_view_callbacks!(v, tab_id, root_weak, cap_queues, cap_wake, config.verify_tests);
                    });
                    ExampleView::Native(view)
                }
            };
            root.model.add_tab(tab_id, url, show_sidebar);
            root.views.insert(tab_id, example_view);
            root.sync_native_tab_visibility(cx);
            cx.notify();
            Ok::<_, String>(reg_out)
        })
        .map_err(|e| e.to_string())??;

    accounting.tabs_opened += 1;
    tabs.insert(
        tab_id,
        TabRuntime {
            browser_id,
            handle,
            first_paint: false,
            menu_reg,
            smoke_eval_id: None,
            smoke_rf_eval_id: None,
            smoke_bench_eval_id: None,
            smoke_post_eval_id: None,
        },
    );
    Ok(())
}

fn close_example_tab(
    cx: &mut AsyncApp,
    rt_cell: &Rc<RefCell<Option<Runtime>>>,
    menus: Option<&Rc<RefCell<ContextMenuServices>>>,
    root: WindowHandle<CefExampleRoot>,
    tab_id: u64,
    accounting: &mut ExampleAccounting,
    tabs: &mut BTreeMap<u64, TabRuntime>,
) -> Result<(), String> {
    let Some(tab_rt) = tabs.remove(&tab_id) else {
        return Ok(());
    };
    let devtools_open = root
        .update(cx, |root, _, _| {
            root.model
                .tabs
                .iter()
                .find(|t| t.id == tab_id)
                .is_some_and(|t| t.devtools_open)
        })
        .unwrap_or(false);
    if devtools_open && let Some(rt) = rt_cell.borrow().as_ref() {
        let _ = rt.close_dev_tools(tab_rt.browser_id);
    }
    drop(tab_rt.menu_reg);
    if let Some(menus) = menus {
        let _ = menus.borrow_mut().unregister(tab_rt.browser_id);
    }

    let removed = root
        .update(cx, |root, _, cx| {
            root.menu_presenters.remove(&tab_id);
            root.model.close_tab(tab_id);
            let v = root.views.remove(&tab_id);
            root.sync_native_tab_visibility(cx);
            cx.notify();
            v
        })
        .map_err(|e| e.to_string())?;
    let diag = if let Some(view) = removed {
        cx.update_window(root.into(), |_, window, cx| view.release_or_retire(window, cx))
            .map_err(|e| e.to_string())?
    } else {
        None
    };
    if let Some(d) = diag {
        accounting.created_images += d.created_images;
        accounting.released_images += d.released_images;
    }
    let _ = tab_rt.handle.request_close(CloseMode::Graceful);
    accounting.tabs_closed += 1;
    Ok(())
}

async fn drive_cef_example(
    cx: &mut AsyncApp,
    root: WindowHandle<CefExampleRoot>,
    config: ExampleConfig,
    close: Rc<Cell<bool>>,
    sender: Arc<UiWakeSender>,
    receiver: UiWakeReceiver,
    actions: Rc<RefCell<Vec<ExampleAction>>>,
) -> i32 {
    let obs = Arc::new(ExampleObservations::new(config.smoke.is_some()));
    let cap_queues = Arc::new(ExampleCapQueues::default());
    let cap_adapter = Arc::new(ExampleCapAdapter {
        queues: cap_queues.clone(),
        wake: sender.clone(),
    });
    let output_dir = config
        .cache_root
        .parent()
        .unwrap_or(&config.cache_root)
        .join("cef-example-downloads");
    let _ = std::fs::create_dir_all(&output_dir);

    let mut settings = RuntimeSettings::default();
    settings.windowless_rendering_enabled = config.host_mode == ExampleHostMode::Osr;
    settings.uncaught_exception_stack_size = Some(10);
    settings.custom_schemes = build_custom_schemes();

    let mut policy = TrustedContentPolicy::allow_all_http_https(true);
    policy.allowed_origins.push("custom://cef-rs-gpui".into());
    policy.allowed_origins.push("test://cef-rs-gpui".into());

    let default_profile = if std::fs::create_dir_all(&config.cache_root).is_ok() {
        ProfileStorage::Persistent {
            directory: config.cache_root.clone(),
        }
    } else {
        ProfileStorage::Memory
    };

    let rt = match Runtime::start(
        RuntimeConfig {
            files: RuntimeFiles {
                directory: config.bundle.clone(),
                helper_executable: config.helper.clone(),
            },
            root_cache_path: config.cache_root.clone(),
            thread_mode: config.mode,
            default_profile,
            content_policy: policy,
            settings,
        },
        RuntimeServices {
            events: obs.clone(),
            frames: Arc::new(IgnoreFrame),
            ui: sender.clone(),
        },
    ) {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("cef-example runtime start failed: {e:?}");
            return match e {
                StartError::ProcessExitRequired { exit_code }
                | StartError::InitializeFailed { exit_code } => {
                    exit_code.filter(|c| *c != 0).unwrap_or(1)
                }
                _ => 1,
            };
        }
    };

    let nz = |n: usize| std::num::NonZeroUsize::new(n).expect("non-zero");
    let _ = rt.set_resource_interceptor(Some(Arc::new(ExampleResourceInterceptor)));
    rt.set_allow_chrome_internal_urls(true);
    rt.set_js_bridge_allow_subframes(true);
    let _ = rt.set_host_capabilities(HostCapabilities {
        download: Some(cap_adapter.clone()),
        js_dialog: Some(cap_adapter.clone()),
        auth: Some(cap_adapter.clone()),
        ..Default::default()
    });
    let _ = rt.configure_devtools(nz(MAX_EXAMPLE_TABS));
    let menus = (config.context_menu && config.host_mode == ExampleHostMode::Osr).then(|| {
        let menu_limits = MenuLimits {
            max_entries: nz(32),
            max_label_bytes: nz(256),
            max_total_label_bytes: nz(2048),
        };
        let ctx_limits = MenuContextLimits {
            max_frame_id_bytes: nz(512),
            max_text_bytes: nz(4096),
            max_suggestions: nz(5),
            max_total_bytes: nz(8192),
        };
        let _ = rt.configure_context_menus(ContextMenuConfig {
            capacity: nz(MAX_EXAMPLE_TABS),
            model_limits: menu_limits,
            context_limits: ctx_limits,
        });
        let _ = rt.configure_menu_navigation(true);
        let _ = rt.configure_menu_structure(true);
        Rc::new(RefCell::new(ContextMenuServices::new(
            nz(MAX_EXAMPLE_TABS),
            nz(MAX_EXAMPLE_TABS),
            menu_limits,
            ctx_limits,
        )))
    });
    let rt_cell = Rc::new(RefCell::new(Some(rt)));
    let mut tabs: BTreeMap<u64, TabRuntime> = BTreeMap::new();
    let mut accounting = ExampleAccounting::default();
    let mut next_tab_id = 1u64;
    let mut active_pdf: Option<(u64, PathBuf, PdfExportTicket)> = None;
    let mut drop_target_owned: Option<(NativeDropTargetServices, u64)> = None;
    let mut rpc_host = CefRsGpuiRpcHost::default();
    let mut verify_driver = config.verify_tests.then(verify::VerifyDriver::new);
    let mut smoke_stage = 0u8;
    let smoke_deadline = config.smoke.map(|d| Instant::now() + d + Duration::from_secs(20));

    let startup = Instant::now() + STARTUP_BUDGET;
    while !close.get() && Instant::now() < startup {
        if rt_cell
            .borrow()
            .as_ref()
            .is_some_and(|r| r.state() == RuntimeState::Ready)
        {
            break;
        }
        if let Err(err) = service(cx, &rt_cell, config.mode, &receiver, Some(startup)).await {
            eprintln!("{err}");
            break;
        }
    }

    let mut run_error: Option<String> = None;
    let initial_url = config
        .initial_url
        .clone()
        .unwrap_or_else(|| DEFAULT_HOME_URL.into());
    if !close.get() {
        if let Err(e) = spawn_example_tab(
            cx,
            &rt_cell,
            menus.as_ref(),
            &cap_adapter,
            root,
            &config,
            next_tab_id,
            initial_url,
            true,
            false,
            &mut accounting,
            &mut tabs,
        ) {
            run_error = Some(e);
        } else {
            next_tab_id += 1;
        }
    }

    let _ = receiver.try_recv();
    while run_error.is_none() && !close.get() {
        if let Some(err) = obs
            .first_fault
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            run_error = Some(err);
            break;
        }
        let quit_from_tabs = root
            .update(cx, |r, _, _| r.model.quit_requested)
            .unwrap_or(true);
        if quit_from_tabs {
            break;
        }

        // Process host capability queues (JS dialogs, auth, downloads, popups).
        for req in cap_queues
            .auths
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            if let Some(rt) = rt_cell.borrow().as_ref() {
                let _ = rt.resolve_auth(req.id, AuthDecision::Cancel);
            }
        }
        for req in cap_queues
            .js_dialogs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            if config.smoke.is_some() {
                if let Some(rt) = rt_cell.borrow().as_ref() {
                    let _ = rt.resolve_js_dialog(req.id, JsDialogDecision::Accept { prompt_input: None });
                }
            } else {
                let _ = cx.update_window(root.into(), |view, window, cx| {
                    let Ok(entity) = view.downcast::<CefExampleRoot>() else {
                        return;
                    };
                    entity.update(cx, |r, cx| {
                        let mut prompt_buf = model::TextBuffer::new(req.default_prompt_text);
                        prompt_buf.focus_select_all();
                        r.model.pending_js_dialog = Some(PendingJsDialog {
                            id: req.id,
                            kind: req.kind,
                            origin_url: req.origin_url,
                            message: req.message_text,
                            prompt_input: prompt_buf,
                        });
                        if req.kind == JsDialogKind::Prompt {
                            r.model.active_input = ActiveInput::JsPrompt;
                            text_input::ensure_parent_window_win32_focus(window);
                            let handle = r.input_focus.clone();
                            window.focus(&handle, cx);
                            window.notify_text_input_changed(&handle);
                        }
                        r.sync_native_tab_visibility(cx);
                        cx.notify();
                    });
                });
            }
        }
        for (seq, req) in cap_queues
            .downloads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            let clean_name = Path::new(&req.suggested_name)
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("download.bin")
                .to_owned();
            let dest = output_dir.join(format!("{seq}-{clean_name}"));
            let owner_id = tabs
                .iter()
                .find(|(_, t)| t.browser_id == req.id.browser)
                .map(|(id, _)| *id);
            let _ = root.update(cx, |r, _, cx| {
                let tid = owner_id.or(r.model.active_tab);
                if let Some(t) = tid.and_then(|id| r.model.tab_mut(id)) {
                    t.download.last_action = "OnBeforeDownload".into();
                    t.download.id = Some(seq);
                    t.download.url = req.url.clone();
                    t.download.original_url = req.original_url.clone();
                    t.download.suggested_file_name = clean_name.clone();
                    t.download.mime_type = req.mime_type.clone();
                    t.download.total_bytes = req.total_bytes;
                    t.download.full_path = dest.to_string_lossy().into_owned();
                    t.download.is_in_progress = true;
                    t.download.is_complete = false;
                    t.download.is_cancelled = false;
                    t.show_download_info = true;
                }
                cx.notify();
            });
            if let Some(rt) = rt_cell.borrow().as_ref() {
                let _ = rt.confirm_download(req.id, dest);
            }
        }
        for prog in cap_queues
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            let owner_id = tabs
                .iter()
                .find(|(_, t)| t.browser_id == prog.id.browser)
                .map(|(id, _)| *id);
            let _ = root.update(cx, |r, _, cx| {
                let tid = owner_id.or(r.model.active_tab);
                if let Some(t) = tid.and_then(|id| r.model.tab_mut(id)) {
                    t.apply_download_progress(&prog);
                    if t.download.is_complete {
                        t.output_message = format!("Download complete: {}", t.download.full_path);
                    }
                }
                cx.notify();
            });
        }
        for popup in cap_queues
            .popups
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            let id = next_tab_id;
            next_tab_id += 1;
            if let Err(e) = spawn_example_tab(
                cx,
                &rt_cell,
                menus.as_ref(),
                &cap_adapter,
                root,
                &config,
                id,
                popup.target_url,
                false,
                false,
                &mut accounting,
                &mut tabs,
            ) {
                run_error = Some(e);
                break;
            }
        }

        // Dispatch `cef-rs-gpui` JS Object Repository RPC messages and poll UrlRequest downloads.
        {
            let mut rpc_out = Vec::new();
            let mut incoming_rpcs = cap_queues
                .js_rpcs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain(..)
                .collect();
            {
                let rt_guard = rt_cell.borrow();
                rpc_host.handle_incoming_messages(
                    &mut incoming_rpcs,
                    rt_guard.as_ref(),
                    cx,
                    root,
                    &mut rpc_out,
                );
                rpc_host.poll_downloads(&mut rpc_out);
            }
            for (tab_id, payload_json) in rpc_out {
                let _ = root.update(cx, |r, _, cx| {
                    if let Some(view) = r.views.get(&tab_id).cloned() {
                        view.post_js_message(payload_json, cx);
                    }
                });
            }
        }

        // Poll active PDF export.
        if let Some((tab_id, ref dest, ref ticket)) = active_pdf {
            match ticket.poll() {
                PdfExportStatus::Pending => {}
                PdfExportStatus::Completed(outcome) => {
                    let path_str = dest.display().to_string();
                    let msg = if outcome.ok {
                        format!("Pdf was saved to {path_str}")
                    } else {
                        format!("Unable to save Pdf, check you have write permissions to {path_str}")
                    };
                    let _ = root.update(cx, |r, _, cx| {
                        if let Some(t) = r.model.tab_mut(tab_id) {
                            t.output_message = msg.clone();
                        }
                        r.model.notice_dialog = Some(NoticeDialog {
                            title: "Print Current tab to PDF".into(),
                            message: msg,
                        });
                        r.sync_native_tab_visibility(cx);
                        cx.notify();
                    });
                    active_pdf = None;
                }
                PdfExportStatus::Failed(err) => {
                    let msg = format!(
                        "Unable to save Pdf, check you have write permissions to {} ({err:?})",
                        dest.display()
                    );
                    let _ = root.update(cx, |r, _, cx| {
                        if let Some(t) = r.model.tab_mut(tab_id) {
                            t.output_message = msg.clone();
                        }
                        r.model.notice_dialog = Some(NoticeDialog {
                            title: "Print Current tab to PDF".into(),
                            message: msg,
                        });
                        r.sync_native_tab_visibility(cx);
                        cx.notify();
                    });
                    active_pdf = None;
                }
            }
        }

        // Process queued UI actions.
        let pending_actions: Vec<ExampleAction> = actions.borrow_mut().drain(..).collect();
        for action in pending_actions {
            match action {
                ExampleAction::ExitApp => {
                    close.set(true);
                    break;
                }
                ExampleAction::NewTab(opt_url) => {
                    let id = next_tab_id;
                    next_tab_id += 1;
                    let url = opt_url.unwrap_or_else(|| DEFAULT_HOME_URL.into());
                    if let Err(e) = spawn_example_tab(
                        cx,
                        &rt_cell,
                        menus.as_ref(),
                        &cap_adapter,
                        root,
                        &config,
                        id,
                        url,
                        false,
                        false,
                        &mut accounting,
                        &mut tabs,
                    ) {
                        run_error = Some(e);
                        break;
                    }
                }
                ExampleAction::NewTabLegacy(url) => {
                    let id = next_tab_id;
                    next_tab_id += 1;
                    if let Err(e) = spawn_example_tab(
                        cx,
                        &rt_cell,
                        menus.as_ref(),
                        &cap_adapter,
                        root,
                        &config,
                        id,
                        url,
                        false,
                        true,
                        &mut accounting,
                        &mut tabs,
                    ) {
                        run_error = Some(e);
                        break;
                    }
                }
                ExampleAction::SwitchTab(id) => {
                    let _ = root.update(cx, |r, window, cx| {
                        r.model.switch_tab(id);
                        r.dragging_input = None;
                        for cache in &r.input_caches[..3] {
                            cache.borrow_mut().reset_scroll();
                        }
                        if r.input_focus.is_focused(window) {
                            let f = r.focus.clone();
                            window.focus(&f, cx);
                        }
                        r.sync_native_tab_visibility(cx);
                        cx.notify();
                    });
                }
                ExampleAction::CloseTab(id) => {
                    let _ = close_example_tab(cx, &rt_cell, menus.as_ref(), root, id, &mut accounting, &mut tabs);
                }
                ExampleAction::NavigateHome => {
                    let _ = root.update(cx, |r, _, cx| {
                        if let Some(id) = r.model.active_tab
                            && let Some(view) = r.views.get(&id).cloned()
                        {
                            if let Some(t) = r.model.tab_mut(id) {
                                t.address_input.set_from_external(DEFAULT_HOME_URL);
                            }
                            r.input_caches[0].borrow_mut().reset_scroll();
                            view.load_url(DEFAULT_HOME_URL.into(), cx);
                        }
                    });
                }
                ExampleAction::NavigateActive(raw) => {
                    let _ = root.update(cx, |r, _, cx| {
                        if let Some(id) = r.model.active_tab {
                            if let Some(url) = normalize_user_url(&raw) {
                                if let Some(t) = r.model.tab_mut(id) {
                                    t.address_input.set_from_external(&url);
                                }
                                r.input_caches[0].borrow_mut().reset_scroll();
                                if let Some(view) = r.views.get(&id).cloned() {
                                    view.load_url(url, cx);
                                }
                            } else if let Some(t) = r.model.tab_mut(id) {
                                t.output_message = "Rejected unsupported or malformed URL".into();
                            }
                        }
                        cx.notify();
                    });
                }
                ExampleAction::Back => {
                    let _ = root.update(cx, |r, _, cx| {
                        if let Some(v) = r.model.active_tab.and_then(|id| r.views.get(&id)).cloned() {
                            v.go_back(cx);
                        }
                    });
                }
                ExampleAction::Forward => {
                    let _ = root.update(cx, |r, _, cx| {
                        if let Some(v) = r.model.active_tab.and_then(|id| r.views.get(&id)).cloned() {
                            v.go_forward(cx);
                        }
                    });
                }
                ExampleAction::Reload => {
                    let _ = root.update(cx, |r, _, cx| {
                        if let Some(v) = r.model.active_tab.and_then(|id| r.views.get(&id)).cloned() {
                            v.reload(cx);
                        }
                    });
                }
                ExampleAction::StopLoading => {
                    let _ = root.update(cx, |r, _, cx| {
                        if let Some(v) = r.model.active_tab.and_then(|id| r.views.get(&id)).cloned() {
                            v.stop_loading(cx);
                        }
                    });
                }
                ExampleAction::ZoomSteps(steps) => {
                    let _ = root.update(cx, |r, window, cx| {
                        if let Some(id) = r.model.active_tab
                            && let Some(v) = r.views.get(&id).cloned()
                        {
                            v.step_zoom(steps, window, cx);
                            let lvl = v.zoom_level(cx);
                            if let Some(t) = r.model.tab_mut(id) {
                                t.sync_zoom_from_view(lvl);
                            }
                            cx.notify();
                        }
                    });
                }
                ExampleAction::ZoomReset => {
                    let _ = root.update(cx, |r, window, cx| {
                        if let Some(id) = r.model.active_tab
                            && let Some(v) = r.views.get(&id).cloned()
                        {
                            v.reset_zoom(window, cx);
                            if let Some(t) = r.model.tab_mut(id) {
                                t.zoom_level_milli = 0;
                            }
                            cx.notify();
                        }
                    });
                }
                ExampleAction::ToggleAudioMute => {
                    let _ = root.update(cx, |r, _, cx| {
                        if let Some(v) = r.model.active_tab.and_then(|id| r.views.get(&id)).cloned() {
                            v.toggle_audio_muted(cx);
                        }
                    });
                }
                ExampleAction::ToggleDevTools => {
                    if let Some(active_id) = root.update(cx, |r, _, _| r.model.active_tab).ok().flatten()
                        && let Some(tab) = tabs.get(&active_id)
                    {
                        let open_now = root
                            .update(cx, |r, _, _| r.model.active_tab().is_some_and(|t| t.devtools_open))
                            .unwrap_or(false);
                        if let Some(rt) = rt_cell.borrow().as_ref() {
                            let ok = if open_now {
                                rt.close_dev_tools(tab.browser_id).is_ok()
                            } else {
                                rt.open_devtools(tab.browser_id).is_ok()
                            };
                            if ok {
                                let _ = root.update(cx, |r, _, cx| {
                                    if let Some(t) = r.model.tab_mut(active_id) {
                                        t.devtools_open = !open_now;
                                    }
                                    cx.notify();
                                });
                            }
                        }
                    }
                }
                ExampleAction::ClearHttpAuthCredentials => {
                    if let Some(rt) = rt_cell.borrow().as_ref() {
                        let res = rt.clear_profile_http_auth_credentials(rt.default_profile_handle());
                        let _ = root.update(cx, |r, _, cx| {
                            if let Some(t) = r.model.active_tab_mut() {
                                t.output_message = format!("ClearHttpAuthCredentials: {:?}", res.is_ok());
                            }
                            cx.notify();
                        });
                    }
                }
                ExampleAction::ToggleSidebar => {
                    let _ = root.update(cx, |r, _, cx| {
                        if let Some(t) = r.model.active_tab_mut() {
                            t.show_sidebar = !t.show_sidebar;
                        }
                        cx.notify();
                    });
                }
                ExampleAction::ToggleDownloadInfo => {
                    let _ = root.update(cx, |r, _, cx| {
                        if let Some(t) = r.model.active_tab_mut() {
                            t.show_download_info = !t.show_download_info;
                        }
                        cx.notify();
                    });
                }
                ExampleAction::ExecuteJs(script) => {
                    let _ = root.update(cx, |r, _, cx| {
                        if let Some(v) = r.model.active_tab.and_then(|id| r.views.get(&id)).cloned() {
                            v.execute_javascript(script, cx);
                        }
                    });
                }
                ExampleAction::EvaluateJs(script) => {
                    let _ = root.update(cx, |r, _, cx| {
                        if let Some(v) = r.model.active_tab.and_then(|id| r.views.get(&id)).cloned() {
                            let _ = v.evaluate_script(script, 5000, cx);
                        }
                    });
                }
                ExampleAction::PostJsToTab {
                    tab_id,
                    payload_json,
                } => {
                    let _ = root.update(cx, |r, _, cx| {
                        if let Some(v) = r.views.get(&tab_id).cloned() {
                            v.post_js_message(payload_json, cx);
                        }
                    });
                }
                ExampleAction::SetRenderAngle(angle) => {
                    let _ = root.update(cx, |r, window, cx| {
                        let clamped = if angle.is_finite() {
                            angle.clamp(-180.0, 180.0)
                        } else {
                            0.0
                        };
                        if let Some(id) = r.model.active_tab
                            && let Some(t) = r.model.tab_mut(id)
                        {
                            t.render_angle_deg = clamped;
                            let op = t.watermark_opacity;
                            let sc = t.bitmap_scaling;
                            if let Some(v) = r.views.get(&id).cloned() {
                                v.sync_osr_rendering(clamped, op, sc, cx);
                            }
                            window.refresh();
                            cx.notify();
                        }
                    });
                }
                ExampleAction::SetWatermarkOpacity(opacity) => {
                    let _ = root.update(cx, |r, window, cx| {
                        let clamped = if opacity.is_finite() {
                            opacity.clamp(0.0, 1.0)
                        } else {
                            0.95
                        };
                        if let Some(id) = r.model.active_tab
                            && let Some(t) = r.model.tab_mut(id)
                        {
                            t.watermark_opacity = clamped;
                            let ang = t.render_angle_deg;
                            let sc = t.bitmap_scaling;
                            if let Some(v) = r.views.get(&id).cloned() {
                                v.sync_osr_rendering(ang, clamped, sc, cx);
                            }
                            window.refresh();
                            cx.notify();
                        }
                    });
                }
                ExampleAction::SetBitmapScaling(scaling) => {
                    let _ = root.update(cx, |r, window, cx| {
                        if let Some(id) = r.model.active_tab
                            && let Some(t) = r.model.tab_mut(id)
                        {
                            t.bitmap_scaling = scaling;
                            let ang = t.render_angle_deg;
                            let op = t.watermark_opacity;
                            if let Some(v) = r.views.get(&id).cloned() {
                                v.sync_osr_rendering(ang, op, scaling, cx);
                            }
                            window.refresh();
                            cx.notify();
                        }
                    });
                }
                ExampleAction::LoadCustomPost => {
                    let _ = root.update(cx, |r, _, cx| {
                        if let Some(id) = r.model.active_tab
                            && let Some(v) = r.views.get(&id).cloned()
                        {
                            if let Some(t) = r.model.tab_mut(id) {
                                t.address_input.set_from_external(CUSTOM_POST_URL);
                                t.output_message =
                                    format_runtime_summary("Submitting Load Custom Request POST...");
                            }
                            v.load_url_with_post_data(
                                CUSTOM_POST_URL.into(),
                                b"test=123&data=456".to_vec(),
                                Some("application/x-www-form-urlencoded".into()),
                                cx,
                            );
                            cx.notify();
                        }
                    });
                }
                ExampleAction::RunBindingStressTest => {
                    let target_tab = root
                        .update(cx, |r, _, cx| {
                            let id = r.model.active_tab?;
                            if let Some(t) = r.model.tab_mut(id) {
                                t.address_input.set_from_external(assets::BINDING_TEST_URL);
                                t.output_message = format_runtime_summary(
                                    "Running Javascript Binding Stress Test...",
                                );
                            }
                            if let Some(v) = r.views.get(&id).cloned() {
                                v.load_url(assets::BINDING_TEST_URL.into(), cx);
                            }
                            cx.notify();
                            Some(id)
                        })
                        .ok()
                        .flatten();
                    if let Some(tab_id) = target_tab {
                        cx.spawn(async move |cx: &mut AsyncApp| {
                            for round in 1..=4_u32 {
                                cx.background_executor()
                                    .timer(Duration::from_millis(2500))
                                    .await;
                                let keep_going = root
                                    .update(cx, |r, _, cx| {
                                        let on_binding = r.model.tabs.iter().any(|t| {
                                            t.id == tab_id
                                                && t.address.starts_with(assets::BINDING_TEST_URL)
                                        });
                                        if on_binding && let Some(v) = r.views.get(&tab_id).cloned()
                                        {
                                            v.reload(cx);
                                            if let Some(t) = r.model.tab_mut(tab_id) {
                                                t.output_message = format_runtime_summary(
                                                    &format!("Binding stress reload #{round}/4"),
                                                );
                                            }
                                            cx.notify();
                                            true
                                        } else {
                                            false
                                        }
                                    })
                                    .unwrap_or(false);
                                if !keep_going {
                                    break;
                                }
                            }
                        })
                        .detach();
                    }
                }
                ExampleAction::ResizeHackTests => {
                    let first_new = next_tab_id;
                    next_tab_id += 1;
                    let second_new = next_tab_id;
                    next_tab_id += 1;
                    let _ = spawn_example_tab(
                        cx,
                        &rt_cell,
                        menus.as_ref(),
                        &cap_adapter,
                        root,
                        &config,
                        first_new,
                        DEFAULT_HOME_URL.into(),
                        true,
                        false,
                        &mut accounting,
                        &mut tabs,
                    );
                    let _ = spawn_example_tab(
                        cx,
                        &rt_cell,
                        menus.as_ref(),
                        &cap_adapter,
                        root,
                        &config,
                        second_new,
                        assets::BINDING_TEST_URL.into(),
                        false,
                        false,
                        &mut accounting,
                        &mut tabs,
                    );
                    cx.spawn(async move |cx: &mut AsyncApp| {
                        let sizes = [(1180.0_f32, 740.0_f32), (1290.0, 810.0), (1240.0, 780.0)];
                        for step in 0..18_usize {
                            cx.background_executor()
                                .timer(Duration::from_millis(90))
                                .await;
                            let (w, h) = sizes[step % sizes.len()];
                            let cont = root
                                .update(cx, |r, window, cx| {
                                    window.resize(size(px(w), px(h)));
                                    let active = r.model.active_tab;
                                    if let Some(next_id) = r
                                        .model
                                        .tabs
                                        .iter()
                                        .map(|t| t.id)
                                        .find(|id| Some(*id) != active)
                                    {
                                        r.model.switch_tab(next_id);
                                        r.sync_native_tab_visibility(cx);
                                    }
                                    if let Some(t) = r.model.active_tab_mut() {
                                        t.output_message = format_runtime_summary(&format!(
                                            "Resize Hack Tests step {}/18",
                                            step + 1
                                        ));
                                    }
                                    cx.notify();
                                    !r.model.quit_requested
                                })
                                .unwrap_or(false);
                            if !cont {
                                break;
                            }
                        }
                        let _ = root.update(cx, |_, window, cx| {
                            window.resize(size(px(1240.0), px(780.0)));
                            cx.notify();
                        });
                    })
                    .detach();
                }
                ExampleAction::ExportPdfPrompt => {
                    let dir = output_dir.clone();
                    cx.spawn(async move |cx: &mut AsyncApp| {
                        let rx = root
                            .update(cx, |_, _, cx| {
                                cx.prompt_for_new_path(&dir, Some("PrintToPdf.pdf"))
                            })
                            .ok();
                        if let Some(rx) = rx
                            && let Ok(Ok(Some(mut path))) = rx.await
                        {
                            let has_pdf_ext = path
                                .extension()
                                .and_then(|e| e.to_str())
                                .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
                            if !has_pdf_ext {
                                let mut os = path.into_os_string();
                                os.push(".pdf");
                                path = PathBuf::from(os);
                            }
                            let _ = root.update(cx, |r, _, _| {
                                r.emit(ExampleAction::ExportPdfToPath(path));
                            });
                        }
                    })
                    .detach();
                }
                ExampleAction::ExportPdfToPath(dest) => {
                    if let Some(active_id) =
                        root.update(cx, |r, _, _| r.model.active_tab).ok().flatten()
                        && let Some(tab) = tabs.get(&active_id)
                        && let Some(rt) = rt_cell.borrow().as_ref()
                    {
                        match rt.print_to_pdf(
                            tab.browser_id,
                            dest.clone(),
                            PdfExportOptions::default(),
                        ) {
                            Ok(ticket) => {
                                active_pdf = Some((active_id, dest.clone(), ticket));
                                let _ = root.update(cx, |r, _, cx| {
                                    if let Some(t) = r.model.tab_mut(active_id) {
                                        t.output_message =
                                            format!("Printing PDF -> {}", dest.display());
                                    }
                                    cx.notify();
                                });
                            }
                            Err(err) => {
                                let msg = format!(
                                    "Unable to save Pdf, check you have write permissions to {} ({err:?})",
                                    dest.display()
                                );
                                let _ = root.update(cx, |r, _, cx| {
                                    r.model.notice_dialog = Some(NoticeDialog {
                                        title: "Print Current tab to PDF".into(),
                                        message: msg.clone(),
                                    });
                                    if let Some(t) = r.model.tab_mut(active_id) {
                                        t.output_message = msg;
                                    }
                                    r.sync_native_tab_visibility(cx);
                                    cx.notify();
                                });
                            }
                        }
                    }
                }
                ExampleAction::DismissNoticeDialog => {
                    let _ = root.update(cx, |r, _, cx| {
                        r.model.notice_dialog = None;
                        r.sync_native_tab_visibility(cx);
                        cx.notify();
                    });
                }
                ExampleAction::ResolveJsDialog { accept } => {
                    if let Ok(Some(dlg)) = root.update(cx, |r, _, cx| {
                        r.model.active_input = ActiveInput::None;
                        let taken = r.model.pending_js_dialog.take();
                        r.sync_native_tab_visibility(cx);
                        cx.notify();
                        taken
                    }) && let Some(rt) = rt_cell.borrow().as_ref()
                    {
                        let decision = if accept {
                            JsDialogDecision::Accept {
                                prompt_input: (dlg.kind == JsDialogKind::Prompt)
                                    .then_some(dlg.prompt_input.text),
                            }
                        } else {
                            JsDialogDecision::Cancel
                        };
                        let _ = rt.resolve_js_dialog(dlg.id, decision);
                    }
                }
            }
        }

        // Consume views, pump per-tab context menus, and sync drag target.
        let tab_ids: Vec<u64> = tabs.keys().copied().collect();
        for tab_id in tab_ids {
            let Some(tab) = tabs.get_mut(&tab_id) else {
                continue;
            };
            if tab.handle.snapshot().state == SessionState::Closed {
                let _ = close_example_tab(cx, &rt_cell, menus.as_ref(), root, tab_id, &mut accounting, &mut tabs);
                continue;
            }
            let view = root
                .update(cx, |r, _, _| r.views.get(&tab_id).cloned())
                .ok()
                .flatten();
            let diag = if let Some(view) = view {
                cx.update_window(root.into(), |_, window, cx| view.consume_and_sample(window, cx))
                    .ok()
                    .flatten()
            } else {
                None
            };
            if !tab.first_paint {
                match config.host_mode {
                    ExampleHostMode::Osr => {
                        if let Some(d) = diag
                            && d.paint_calls > 0
                            && d.painted_sequence > 0
                            && d.frame_width > 0
                        {
                            tab.first_paint = true;
                            println!(
                                "CEF_EXAMPLE_PAINT tab={} browser={} seq={}",
                                tab_id,
                                tab.browser_id.local(),
                                d.painted_sequence
                            );
                        }
                    }
                    ExampleHostMode::NativeHwnd => {
                        let snap = tab.handle.snapshot();
                        let child_hwnd = rt_cell
                            .borrow()
                            .as_ref()
                            .and_then(|rt| rt.native_browser_hwnd(tab.browser_id));
                        if snap.state == SessionState::Alive
                            && !snap.navigation.is_loading
                            && !snap.navigation.address.is_empty()
                            && let Some(hwnd) = child_hwnd
                        {
                            tab.first_paint = true;
                            println!(
                                "CEF_EXAMPLE_NATIVE_READY tab={} browser={} child_hwnd=0x{:x}",
                                tab_id,
                                tab.browser_id.local(),
                                hwnd.get()
                            );
                        }
                    }
                }
            }
            let nav = tab.handle.snapshot().navigation;
            let _ = root.update(cx, |r, _, cx| {
                let editing_addr = r.model.active_tab == Some(tab_id)
                    && r.model.active_input == ActiveInput::Address;
                if let Some(t) = r.model.tab_mut(tab_id) {
                    t.apply_snapshot(&nav, editing_addr);
                }
                cx.notify();
            });
            if tab.menu_reg.is_some()
                && let Some(ref menus) = menus
            {
                let _ = root.update(cx, |r, window, cx| {
                    if let Some(pres) = r.menu_presenters.get(&tab_id) {
                        let _ = menus.borrow_mut().dispatch(tab.browser_id, window, cx, |upd, w, cx| {
                            pres.borrow_mut().update(&upd, w, cx)
                        });
                    }
                });
            }
        }

        if config.host_mode == ExampleHostMode::Osr {
            let active_osr = root
                .update(cx, |r, _, _| {
                    let id = r.model.active_tab?;
                    match r.views.get(&id)? {
                        ExampleView::Osr(v) => Some((id, v.clone())),
                        ExampleView::Native(_) => None,
                    }
                })
                .ok()
                .flatten();
            match (&drop_target_owned, active_osr) {
                (Some((_, cur)), Some((next, _))) if *cur == next => {}
                (_, Some((next, view))) => {
                    if let Some((mut prev, _)) = drop_target_owned.take() {
                        let _ = prev.retire();
                    }
                    if let Ok(Ok(ds)) = root.update(cx, |_, window, cx| {
                        let nz1 = std::num::NonZeroUsize::new(1).expect("non-zero");
                        let mut ds = NativeDropTargetServices::new(nz1)?;
                        ds.attach(&view, window, cx)?;
                        Ok::<_, rust_cef_gpui::NativeDropTargetError>(ds)
                    }) {
                        drop_target_owned = Some((ds, next));
                    }
                }
                (Some(_), None) => {
                    if let Some((mut prev, _)) = drop_target_owned.take() {
                        let _ = prev.retire();
                    }
                }
                (None, None) => {}
            }
        }

        // Automated `--smoke-ms` self-check sequence.
        if config.smoke.is_some() {
            if smoke_stage == 0
                && tabs.get(&1).is_some_and(|t| {
                    t.first_paint && !t.handle.snapshot().navigation.is_loading
                })
            {
                let _ = root.update(cx, |r, window, cx| {
                    if let Some(t) = r.model.tab_mut(1) {
                        t.render_angle_deg = -28.0;
                        t.watermark_opacity = 0.55;
                        t.bitmap_scaling = BitmapScalingKind::NearestNeighbor;
                    }
                    if let Some(view) = r.views.get(&1).cloned() {
                        view.sync_osr_rendering(
                            -28.0,
                            0.55,
                            BitmapScalingKind::NearestNeighbor,
                            cx,
                        );
                    }
                    window.refresh();
                    cx.notify();
                });
                if config.host_mode == ExampleHostMode::Osr {
                    println!(
                        "CEF_EXAMPLE_TRANSFORM angle=-28.0 opacity=0.55 scaling=NearestNeighbor"
                    );
                }
                let eval_id = root
                    .update(cx, |r, _, cx| {
                        r.views.get(&1)?.evaluate_script(
                            "(() => ({ sum: 1 + 1, skipFn: () => 99 }))().sum".into(),
                            5000,
                            cx,
                        )
                    })
                    .ok()
                    .flatten();
                if let Some(t1) = tabs.get_mut(&1) {
                    t1.smoke_eval_id = eval_id;
                }
                smoke_stage = 1;
            } else if smoke_stage == 1
                && root.update(cx, |r, _, _| r.smoke_eval_ok).unwrap_or(false)
            {
                let id = next_tab_id;
                next_tab_id += 1;
                if let Err(e) = spawn_example_tab(
                    cx,
                    &rt_cell,
                    menus.as_ref(),
                    &cap_adapter,
                    root,
                    &config,
                    id,
                    RESPONSE_FILTER_TEST_URL.into(),
                    false,
                    false,
                    &mut accounting,
                    &mut tabs,
                ) {
                    run_error = Some(e);
                    break;
                }
                smoke_stage = 2;
            } else if smoke_stage == 2
                && tabs.get(&2).is_some_and(|t| {
                    let s = t.handle.snapshot();
                    t.first_paint
                        && !s.navigation.is_loading
                        && s.navigation.address.starts_with(RESPONSE_FILTER_TEST_URL)
                })
            {
                let rf_eval_id = root
                    .update(cx, |r, _, cx| {
                        r.views.get(&2)?.evaluate_script(
                            "(document.body.innerText.includes('This is the replaced string!') && !document.body.innerText.includes('REPLACE_THIS_STRING')) ? 'FILTER_VERIFIED' : 'FILTER_MISSING'".into(),
                            5000,
                            cx,
                        )
                    })
                    .ok()
                    .flatten();
                if let Some(t2) = tabs.get_mut(&2) {
                    t2.smoke_rf_eval_id = rf_eval_id;
                }
                smoke_stage = 3;
            } else if smoke_stage == 3
                && root
                    .update(cx, |r, _, _| r.smoke_response_filter_ok)
                    .unwrap_or(false)
            {
                println!("CEF_EXAMPLE_RESPONSE_FILTER ok=true");
                let id = next_tab_id;
                next_tab_id += 1;
                if let Err(e) = spawn_example_tab(
                    cx,
                    &rt_cell,
                    menus.as_ref(),
                    &cap_adapter,
                    root,
                    &config,
                    id,
                    BENCHMARK_SUITE_URL.into(),
                    false,
                    false,
                    &mut accounting,
                    &mut tabs,
                ) {
                    run_error = Some(e);
                    break;
                }
                smoke_stage = 4;
            } else if smoke_stage == 4
                && tabs.get(&3).is_some_and(|t| {
                    let s = t.handle.snapshot();
                    t.first_paint
                        && !s.navigation.is_loading
                        && s.navigation.address.starts_with(BENCHMARK_SUITE_URL)
                        && s.navigation.title.starts_with("BenchmarkSuite:")
                })
            {
                let bench_eval_id = root
                    .update(cx, |r, _, cx| {
                        r.views.get(&3)?.evaluate_script(
                            "(window.__benchmarkSummary && window.__benchmarkSummary.ok === true) ? 'BENCHMARK_OK' : JSON.stringify(window.__benchmarkSummary)".into(),
                            2500,
                            cx,
                        )
                    })
                    .ok()
                    .flatten();
                if let Some(t3) = tabs.get_mut(&3) {
                    t3.smoke_bench_eval_id = bench_eval_id;
                }
                smoke_stage = 5;
            } else if smoke_stage == 5
                && root
                    .update(cx, |r, _, _| r.smoke_benchmark_ok)
                    .unwrap_or(false)
            {
                println!("CEF_EXAMPLE_BENCHMARK ok=true");
                let _ = root.update(cx, |r, _, cx| {
                    if let Some(t3) = r.model.tab_mut(3) {
                        t3.address_input.set_from_external(CUSTOM_POST_URL);
                    }
                    if let Some(v) = r.views.get(&3).cloned() {
                        v.load_url_with_post_data(
                            CUSTOM_POST_URL.into(),
                            b"test=123&data=456".to_vec(),
                            Some("application/x-www-form-urlencoded".into()),
                            cx,
                        );
                    }
                    cx.notify();
                });
                smoke_stage = 6;
            } else if smoke_stage == 6
                && tabs.get(&3).is_some_and(|t| {
                    let s = t.handle.snapshot();
                    !s.navigation.is_loading && s.navigation.address == CUSTOM_POST_URL
                })
            {
                let post_eval_id = root
                    .update(cx, |r, _, cx| {
                        r.views.get(&3)?.evaluate_script(
                            "(document.body && document.body.innerText.includes('Post Data: test=123&data=456')) ? 'POST_OK' : (document.body ? document.body.innerText : 'NO_BODY')".into(),
                            2500,
                            cx,
                        )
                    })
                    .ok()
                    .flatten();
                if let Some(t3) = tabs.get_mut(&3) {
                    t3.smoke_post_eval_id = post_eval_id;
                }
                smoke_stage = 7;
            } else if smoke_stage == 7
                && root
                    .update(cx, |r, _, _| r.smoke_custom_post_ok)
                    .unwrap_or(false)
            {
                println!("CEF_EXAMPLE_CUSTOM_POST ok=true");
                break;
            }

            if smoke_deadline.is_some_and(|dl| Instant::now() >= dl) {
                let eval_ok = root.update(cx, |r, _, _| r.smoke_eval_ok).unwrap_or(false);
                let rf_ok = root
                    .update(cx, |r, _, _| r.smoke_response_filter_ok)
                    .unwrap_or(false);
                let bench_ok = root
                    .update(cx, |r, _, _| r.smoke_benchmark_ok)
                    .unwrap_or(false);
                let post_ok = root
                    .update(cx, |r, _, _| r.smoke_custom_post_ok)
                    .unwrap_or(false);
                run_error = Some(format!(
                    "cef-example smoke timed out (stage={smoke_stage} eval_ok={eval_ok} rf_ok={rf_ok} bench_ok={bench_ok} post_ok={post_ok})"
                ));
                break;
            }
        }

        if let Some(driver) = verify_driver.as_mut() {
            for (tab_id, seq, outcome) in cap_queues
                .evals
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain(..)
            {
                driver.record_eval(tab_id, seq, outcome);
            }
            if let Err(err) = driver.step(
                cx,
                &rt_cell,
                menus.as_ref(),
                &cap_adapter,
                root,
                &config,
                &output_dir,
                &mut next_tab_id,
                &mut accounting,
                &mut tabs,
                &mut rpc_host,
            ) {
                run_error = Some(err);
                break;
            }
            if driver.is_done() {
                break;
            }
        } else {
            cap_queues
                .evals
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
        }

        if let Err(err) = service(cx, &rt_cell, config.mode, &receiver, None).await {
            run_error = Some(err);
            break;
        }
    }

    if let Some(err) = run_error.as_ref() {
        eprintln!("{err}");
    }
    if let Some((mut ds, _)) = drop_target_owned.take() {
        let _ = ds.retire();
    }
    let remaining: Vec<u64> = tabs.keys().copied().collect();
    for id in remaining {
        let _ = close_example_tab(cx, &rt_cell, menus.as_ref(), root, id, &mut accounting, &mut tabs);
    }

    let mut rt = match rt_cell.borrow_mut().take() {
        Some(rt) => rt,
        None => return 1,
    };
    let deadline = Instant::now() + DRAIN_BUDGET;
    let ticket = rt.request_shutdown(CloseMode::Graceful, deadline);
    loop {
        match rt.poll_shutdown(&ticket) {
            ShutdownStatus::Ready => {
                if let Err(err) = rt.finish_shutdown() {
                    eprintln!("cef-example finish_shutdown: {err:?}");
                    return 1;
                }
                break;
            }
            status @ (ShutdownStatus::Draining { .. } | ShutdownStatus::Blocked { .. }) => {
                if Instant::now() >= deadline {
                    eprintln!("cef-example shutdown timed out: {status:?}");
                    std::mem::forget(rt);
                    return 1;
                }
            }
            other => {
                eprintln!("cef-example shutdown failed: {other:?}");
                std::mem::forget(rt);
                return 1;
            }
        }
        let step = if config.mode == ThreadMode::Integrated {
            PUMP_PERIOD
        } else {
            Duration::from_millis(25)
        };
        let _ = wait(cx, &receiver, Some(deadline.saturating_duration_since(Instant::now()).min(step))).await;
        if config.mode == ThreadMode::Integrated {
            let _ = rt.pump();
        }
    }

    if run_error.is_some() {
        return 1;
    }
    let d = rt.diagnostics();
    let created = obs.created.load(Ordering::Acquire);
    let closed = obs.closed.load(Ordering::Acquire);
    if d.cef_shutdown_calls != 1
        || d.native_calls_after_shutdown != 0
        || created != closed
        || accounting.created_images != accounting.released_images
    {
        eprintln!("cef-example shutdown audit mismatch: {d:?} created={created} closed={closed}");
        return 1;
    }
    println!(
        "CEF_EXAMPLE_STOP mode={:?} backend={:?} tabs={} created={} closed={} images={} final_cpu=0 shutdown={} native_after={}",
        d.mode,
        d.observed_backend,
        accounting.tabs_opened,
        created,
        closed,
        accounting.created_images,
        d.cef_shutdown_calls,
        d.native_calls_after_shutdown
    );
    0
}

pub fn run(config: ExampleConfig) -> i32 {
    let result = Rc::new(RefCell::new(None));
    let final_result = result.clone();
    gpui_platform::application().run(move |cx| {
        if config.context_menu && config.host_mode == ExampleHostMode::Osr {
            rust_cef_gpui::init_default_menu_ui(cx);
        }
        let (sender, receiver) = dispatcher::channel();
        let sender = Arc::new(sender);
        let close = Rc::new(Cell::new(false));
        let allow_close = Rc::new(Cell::new(false));
        let actions = Rc::new(RefCell::new(Vec::new()));
        let (requested, wake, can_close) = (close.clone(), sender.clone(), allow_close.clone());

        let actions_root = actions.clone();
        let wake_root = sender.clone();
        let host_mode = config.host_mode;
        let backend_label = match (config.host_mode, config.backend) {
            (ExampleHostMode::NativeHwnd, _) => "NativeHWND",
            (ExampleHostMode::Osr, RenderPreference::D3d11Required) => "OSR · D3D11",
            (ExampleHostMode::Osr, _) => "OSR · Software",
        };
        let title_str = match config.host_mode {
            ExampleHostMode::Osr => "GpuiCef.Example - x64 (OSR)",
            ExampleHostMode::NativeHwnd => "GpuiCef.Example - x64 (Native HWND)",
        };

        let root = match cx.open_window(
            WindowOptions {
                focus: false,
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1240.), px(780.)),
                    cx,
                ))),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some(title_str.into()),
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
                cx.new(|cx| {
                    let input_focus = cx.focus_handle();
                    let focus_out_sub =
                        cx.on_focus_out(&input_focus, window, |this: &mut CefExampleRoot, _, _, cx| {
                            this.blur_active_input(cx);
                        });
                    CefExampleRoot {
                        model: ExampleAppModel::default(),
                        host_mode,
                        backend_label,
                        views: BTreeMap::new(),
                        menu_presenters: BTreeMap::new(),
                        actions: actions_root,
                        wake: wake_root,
                        focus: cx.focus_handle(),
                        input_focus,
                        input_caches: std::array::from_fn(|_| {
                            Rc::new(RefCell::new(text_input::TextFieldLayoutCache::default()))
                        }),
                        dragging_input: None,
                        focus_out_sub: Some(focus_out_sub),
                        smoke_eval_ok: false,
                        smoke_response_filter_ok: false,
                        smoke_benchmark_ok: false,
                        smoke_custom_post_ok: false,
                        active_slider: None,
                        angle_track_bounds: Rc::new(Cell::new(Bounds::default())),
                        opacity_track_bounds: Rc::new(Cell::new(Bounds::default())),
                    }
                })
            },
        ) {
            Ok(root) => root,
            Err(e) => {
                eprintln!("cef-example window: {e}");
                *final_result.borrow_mut() = Some(1);
                cx.quit();
                return;
            }
        };

        cx.spawn(async move |cx: &mut AsyncApp| {
            let code = drive_cef_example(cx, root, config, close, sender, receiver, actions).await;
            *final_result.borrow_mut() = Some(code);
            allow_close.set(true);
            cx.update(|app: &mut App| app.quit());
        })
        .detach();
    });
    result.take().unwrap_or(1)
}
