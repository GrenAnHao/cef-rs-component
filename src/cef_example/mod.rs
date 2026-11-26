//! GPUI desktop browser example built over public
//! `rust-cef-gpui` and `rust-cef-core` APIs.

pub mod assets;
pub(crate) mod dialogs;
pub(crate) mod find_bar;
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
use rust_cef_core::{
    contracts::{BrowserKeyEventKind, ShortcutFilter, ShortcutId, ShortcutRule},
    keymap::{EVENTFLAG_ALT_DOWN, EVENTFLAG_COMMAND_DOWN, EVENTFLAG_CONTROL_DOWN},
    ports::RequestId as BeforeUnloadRequestId,
};
use rust_cef_gpui::{
    CefBrowser, ContextMenuConfig, ContextMenuRegistration, ContextMenuServices,
    DefaultMenuPresenter, FindServices, FindUiUpdate, NativeCefBrowser, NativeDropTargetServices,
    PdfExportStatus, PdfExportTicket, Runtime, RuntimeServices, SourceViewOptions,
    SourceViewServices, ViewDiagnostics,
    contracts::{
        BrowserConfig, BrowserId, BrowserOptions, BrowserSurfaceMode, CefFeatureState,
        NativeChildHwndConfig, PixelRect, ProfileStorage, RenderPreference, RuntimeConfig,
        RuntimeFiles, RuntimeSettings, RuntimeState, SessionState, ThreadMode,
        TrustedContentPolicy, Viewport, step_canonical_zoom_level,
    },
    dispatcher::{self, UiWakeReceiver, UiWakeSender},
    find::FindLimits,
    host_capabilities::{
        AuthDecision, AuthRequest, AuthService, CertificateDecision, CertificateErrorRequest,
        CertificateService, ClientCertificateRequest, DownloadId, DownloadProgress,
        DownloadRequest, DownloadService, FileDialogDecision, FileDialogKind, FileDialogRequest,
        FileDialogService, HostCapabilities, HostRequestId, JsDialogDecision, JsDialogKind,
        JsDialogRequest, JsDialogService, PdfExportOptions, PermissionDecision, PermissionRequest,
        PermissionService, PopupDecision, PopupDisposition, PopupPolicy, PopupRequest,
        PopupService, PreKeyDecision,
    },
    js_bridge::{EvalOutcome, EvalRequestId, JsBridgeConfig, JsFrameScope, MAX_JS_PAYLOAD_BYTES},
    menu::{MenuContextLimits, MenuLimits},
    outbound_drag_contracts::OutboundDragPolicy,
    ports::{
        BrowserEvent, BrowserEventEnvelope, BrowserEventSink, BrowserHandle, CloseMode, FrameSink,
        OwnedFrame, PublishResult, UiDispatcher as _,
    },
    runtime::{ShutdownStatus, StartError},
    source::SourceViewLimits,
};

use assets::{
    BENCHMARK_SUITE_URL, CUSTOM_POST_URL, DEFAULT_HOME_URL, ExampleResourceInterceptor,
    RESPONSE_FILTER_TEST_URL, build_custom_schemes, format_runtime_summary,
};
use dialogs::{
    FileDialogTracker, build_auth_decision_and_wipe, build_path_prompt_options,
    cancel_active_download_on_close, cap_selected_paths, resolve_download_destination,
    split_save_default_path,
};
use find_bar::{
    DirectFindBook, DirectSourceBook, FindCommandKind, apply_find_status, format_source_failure,
    handle_find_dispatch_error, plan_close_find_bar, plan_find_command,
};
use jsb_rpc::CefRsGpuiRpcHost;
use model::{
    ActiveInput, BitmapScalingKind, ExampleAppModel, ExampleConfig, ExampleHostMode, NoticeDialog,
    PendingBeforeUnloadModal, PendingCertKind, PendingCertModal, PendingJsDialog,
    PendingPermissionModal, TextBuffer, normalize_user_url,
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
    PostJsToTab {
        tab_id: u64,
        payload_json: String,
    },
    SetRenderAngle(f32),
    SetWatermarkOpacity(f32),
    SetBitmapScaling(BitmapScalingKind),
    LoadCustomPost,
    RunBindingStressTest,
    ResizeHackTests,
    ExportPdfPrompt,
    ExportPdfToPath(PathBuf),
    DismissNoticeDialog,
    ResolveJsDialog {
        accept: bool,
    },
    OpenFindBar,
    CloseFindBar,
    FindToggleCase,
    FindDispatch {
        forward: bool,
        force_find_next: bool,
    },
    ViewSource,
    ToggleAskSaveAs,
    CancelDownload(u64),
    OpenContainingFolder(PathBuf),
    CompleteFileDialog {
        request_id: HostRequestId,
        paths: Option<Vec<PathBuf>>,
    },
    CompleteSaveAsDownload {
        request_id: DownloadId,
        path: Option<PathBuf>,
    },
    ResolvePermission {
        allow: bool,
    },
    ResolveAuth {
        submit: bool,
    },
    ResolveCertificate {
        accept: bool,
    },
    ResolveBeforeUnload(bool),
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

    pub(super) fn resolve_before_unload(
        &self,
        request: BeforeUnloadRequestId,
        allow: bool,
        cx: &mut App,
    ) {
        match self {
            Self::Osr(v) => v.update(cx, |view, _| {
                let _ = view.resolve_before_unload(request, allow);
            }),
            Self::Native(v) => v.update(cx, |view, _| {
                let _ = view.resolve_before_unload(request, allow);
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

    pub(super) fn evaluate_script(
        &self,
        script: String,
        timeout_ms: u64,
        cx: &mut App,
    ) -> Option<EvalRequestId> {
        match self {
            Self::Osr(v) => v.update(cx, |view, _| {
                view.evaluate_script(script, JsFrameScope::MainFrame, timeout_ms)
                    .ok()
            }),
            Self::Native(v) => v.update(cx, |view, _| {
                view.evaluate_script(script, JsFrameScope::MainFrame, timeout_ms)
                    .ok()
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
    file_dialogs: Mutex<VecDeque<FileDialogRequest>>,
    file_dialog_cancellations: Mutex<VecDeque<HostRequestId>>,
    js_dialogs: Mutex<VecDeque<JsDialogRequest>>,
    js_dialog_cancellations: Mutex<VecDeque<HostRequestId>>,
    permissions: Mutex<VecDeque<PermissionRequest>>,
    permission_cancellations: Mutex<VecDeque<HostRequestId>>,
    auths: Mutex<VecDeque<AuthRequest>>,
    auth_cancellations: Mutex<VecDeque<HostRequestId>>,
    cert_errors: Mutex<VecDeque<CertificateErrorRequest>>,
    client_certs: Mutex<VecDeque<ClientCertificateRequest>>,
    cert_cancellations: Mutex<VecDeque<HostRequestId>>,
    popups: Mutex<VecDeque<PopupRequest>>,
    js_rpcs: Mutex<VecDeque<(u64, String)>>,
    evals: Mutex<VecDeque<(u64, u64, EvalOutcome)>>,
    escape_find_tabs: Mutex<VecDeque<u64>>,
    shortcut_events: Mutex<VecDeque<(u64, ShortcutId)>>,
    before_unload_events: Mutex<VecDeque<(u64, BeforeUnloadRequestId, bool)>>,
    close_cancelled_tabs: Mutex<VecDeque<u64>>,
}

struct ExampleCapAdapter {
    queues: Arc<ExampleCapQueues>,
    wake: Arc<UiWakeSender>,
}

fn push_queue<T>(queue: &Mutex<VecDeque<T>>, wake: &UiWakeSender, item: T) {
    queue
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_back(item);
    let _ = wake.wake();
}

impl DownloadService for ExampleCapAdapter {
    fn on_before_download(&self, request: DownloadRequest) {
        let seq = request.id.sequence;
        push_queue(&self.queues.downloads, &self.wake, (seq, request));
    }

    fn on_download_updated(&self, progress: DownloadProgress) {
        push_queue(&self.queues.progress, &self.wake, progress);
    }
}

impl FileDialogService for ExampleCapAdapter {
    fn on_file_dialog(&self, request: FileDialogRequest) {
        push_queue(&self.queues.file_dialogs, &self.wake, request);
    }

    fn on_file_dialog_cancelled(&self, id: HostRequestId) {
        push_queue(&self.queues.file_dialog_cancellations, &self.wake, id);
    }
}

impl JsDialogService for ExampleCapAdapter {
    fn on_js_dialog(&self, request: JsDialogRequest) {
        push_queue(&self.queues.js_dialogs, &self.wake, request);
    }

    fn on_js_dialog_cancelled(&self, id: HostRequestId) {
        push_queue(&self.queues.js_dialog_cancellations, &self.wake, id);
    }
}

impl PermissionService for ExampleCapAdapter {
    fn on_permission_request(&self, request: PermissionRequest) {
        push_queue(&self.queues.permissions, &self.wake, request);
    }

    fn on_permission_cancelled(&self, id: HostRequestId) {
        push_queue(&self.queues.permission_cancellations, &self.wake, id);
    }
}

impl PopupService for ExampleCapAdapter {
    fn on_popup_request(&self, request: PopupRequest) {
        push_queue(&self.queues.popups, &self.wake, request);
    }
}

impl AuthService for ExampleCapAdapter {
    fn on_auth_credentials(&self, request: AuthRequest) {
        push_queue(&self.queues.auths, &self.wake, request);
    }

    fn on_auth_cancelled(&self, id: HostRequestId) {
        push_queue(&self.queues.auth_cancellations, &self.wake, id);
    }
}

impl CertificateService for ExampleCapAdapter {
    fn on_certificate_error(&self, request: CertificateErrorRequest) {
        push_queue(&self.queues.cert_errors, &self.wake, request);
    }

    fn on_select_client_certificate(&self, request: ClientCertificateRequest) {
        push_queue(&self.queues.client_certs, &self.wake, request);
    }

    fn on_certificate_cancelled(&self, id: HostRequestId) {
        push_queue(&self.queues.cert_cancellations, &self.wake, id);
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
    closing_pending: bool,
    menu_reg: Option<Arc<ContextMenuRegistration>>,
    smoke_eval_id: Option<EvalRequestId>,
    smoke_rf_eval_id: Option<EvalRequestId>,
    smoke_bench_eval_id: Option<EvalRequestId>,
    smoke_post_eval_id: Option<EvalRequestId>,
}

struct UxServices {
    finds: Option<FindServices>,
    sources: Option<SourceViewServices>,
    direct_finds: DirectFindBook,
    direct_sources: DirectSourceBook,
    retired_browsers: Vec<BrowserId>,
    reported_source_ops: Vec<(BrowserId, u64)>,
}

impl UxServices {
    fn new(
        finds: Option<FindServices>,
        sources: Option<SourceViewServices>,
        wake: Arc<UiWakeSender>,
        max_source_bytes: usize,
    ) -> Self {
        Self {
            finds,
            sources,
            direct_finds: DirectFindBook::new(wake.clone()),
            direct_sources: DirectSourceBook::new(wake, max_source_bytes),
            retired_browsers: Vec::new(),
            reported_source_ops: Vec::new(),
        }
    }

    fn sweep_retired(&mut self, cx: &mut AsyncApp) {
        if self.retired_browsers.is_empty() {
            return;
        }
        let mut remaining = Vec::new();
        let _ = cx.update(|_| {
            for browser in self.retired_browsers.drain(..) {
                let find_busy = self
                    .finds
                    .as_mut()
                    .is_some_and(|f| f.unregister(browser).is_err());
                let source_busy = self
                    .sources
                    .as_mut()
                    .is_some_and(|s| s.unregister(browser).is_err());
                if find_busy || source_busy {
                    remaining.push(browser);
                }
            }
        });
        self.retired_browsers = remaining;
    }

    fn shutdown(&mut self, cx: &mut AsyncApp) {
        let _ = cx.update(|app| {
            if let Some(finds) = self.finds.as_mut() {
                finds.shutdown(app);
            }
            if let Some(sources) = self.sources.as_mut() {
                sources.shutdown(app);
            }
        });
    }
}

#[derive(Default)]
struct ExampleAccounting {
    tabs_opened: u64,
    tabs_closed: u64,
    created_images: u64,
    released_images: u64,
}

macro_rules! wire_view_callbacks {
    ($view:expr, $tab_id:expr, $root_weak:expr, $cap_queues:expr, $wake:expr, $log_console:expr, $find_open_flag:expr) => {{
        let esc_queues = $cap_queues.clone();
        let esc_wake = $wake.clone();
        let find_flag = $find_open_flag;
        $view.set_pre_key_filter(Some(Arc::new(move |_browser, key| {
            if key.kind != BrowserKeyEventKind::KeyUp
                && key.windows_key_code == 0x1B
                && (key.modifiers
                    & (EVENTFLAG_CONTROL_DOWN | EVENTFLAG_ALT_DOWN | EVENTFLAG_COMMAND_DOWN))
                    == 0
                && find_flag.load(Ordering::Acquire)
            {
                push_queue(&esc_queues.escape_find_tabs, &esc_wake, $tab_id);
                return PreKeyDecision::Handled;
            }
            PreKeyDecision::PassThrough
        })));

        let sc_queues = $cap_queues.clone();
        let sc_wake = $wake.clone();
        $view.set_on_shortcut(Some(Box::new(move |id, _, _, _app| {
            push_queue(&sc_queues.shortcut_events, &sc_wake, ($tab_id, id));
        })));

        let bu_queues = $cap_queues.clone();
        let bu_wake = $wake.clone();
        $view.set_on_before_unload(Some(Box::new(move |info, _, _, _app| {
            push_queue(
                &bu_queues.before_unload_events,
                &bu_wake,
                ($tab_id, info.request, info.is_reload),
            );
        })));

        let cc_queues = $cap_queues.clone();
        let cc_wake = $wake.clone();
        $view.set_on_close_cancelled(Some(Box::new(move |(), _, _, _app| {
            push_queue(&cc_queues.close_cancelled_tabs, &cc_wake, $tab_id);
        })));

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
                        let doc_changed = t.address.split('#').next() != addr.split('#').next();
                        t.address = addr.clone();
                        if doc_changed {
                            t.find.on_document_navigated();
                        }
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

fn example_shortcut_filter() -> ShortcutFilter {
    ShortcutFilter::new(vec![
        ShortcutRule {
            id: ShortcutId(1),
            windows_key_code: 0x46, // Ctrl+F
            shift: false,
            control: true,
            alt: false,
            platform: false,
        },
        ShortcutRule {
            id: ShortcutId(2),
            windows_key_code: 0x72, // F3
            shift: false,
            control: false,
            alt: false,
            platform: false,
        },
        ShortcutRule {
            id: ShortcutId(3),
            windows_key_code: 0x72, // Shift+F3
            shift: true,
            control: false,
            alt: false,
            platform: false,
        },
        ShortcutRule {
            id: ShortcutId(4),
            windows_key_code: 0x55, // Ctrl+U
            shift: false,
            control: true,
            alt: false,
            platform: false,
        },
        ShortcutRule {
            id: ShortcutId(5),
            windows_key_code: 0x54, // Ctrl+T
            shift: false,
            control: true,
            alt: false,
            platform: false,
        },
        ShortcutRule {
            id: ShortcutId(6),
            windows_key_code: 0x57, // Ctrl+W
            shift: false,
            control: true,
            alt: false,
            platform: false,
        },
    ])
    .expect("valid cef example shortcut filter")
}

#[allow(clippy::too_many_arguments)]
fn spawn_example_tab(
    cx: &mut AsyncApp,
    rt_cell: &Rc<RefCell<Option<Runtime>>>,
    menus: Option<&Rc<RefCell<ContextMenuServices>>>,
    ux: &mut UxServices,
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
        .with_shortcut_filter(example_shortcut_filter())
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
            root.model.add_tab(tab_id, url, show_sidebar);
            let find_open_flag = root
                .model
                .tab(tab_id)
                .expect("newly added tab exists")
                .find
                .open_flag
                .clone();
            let root_weak = cx.entity().downgrade();
            let mut reg_out: Option<Arc<ContextMenuRegistration>> = None;
            let popup_cap_osr = cap_adapter.clone();
            let popup_cap_native = cap_adapter.clone();
            let cap_queues = cap_adapter.queues.clone();
            let cap_wake = cap_adapter.wake.clone();
            let built = match config.host_mode {
                ExampleHostMode::Osr => {
                    CefBrowser::new(attachment, window, cx)
                        .map_err(|e| e.to_string())
                        .map(|view| {
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
                                wire_view_callbacks!(
                                    v,
                                    tab_id,
                                    root_weak,
                                    cap_queues,
                                    cap_wake,
                                    config.verify_tests,
                                    find_open_flag
                                );
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
                                let target_res = menus.borrow_mut().register(&view, window, cx);
                                if let Ok(target) = target_res
                                    && let Some(rt) = rt_cell.borrow().as_ref()
                                    && let Ok(reg) = rt.bind_context_menu(target)
                                    && let Ok(presenter) = DefaultMenuPresenter::new(
                                        &view,
                                        window,
                                        menu_limits,
                                        ctx_limits,
                                        cx,
                                    )
                                {
                                    let reg_arc = Arc::new(reg);
                                    if let Some(finds) = ux.finds.as_mut() {
                                        let menus_ref = menus.borrow();
                                        if let Err(err) = finds.register(
                                            &view,
                                            window,
                                            &menus_ref,
                                            reg_arc.clone(),
                                            cx,
                                        ) {
                                            eprintln!(
                                                "cef-example FindServices register tab={tab_id}: {err:?}"
                                            );
                                        }
                                    }
                                    if let Some(sources) = ux.sources.as_mut() {
                                        let menus_ref = menus.borrow();
                                        if let Err(err) = sources.register(
                                            &view,
                                            window,
                                            &menus_ref,
                                            reg_arc.clone(),
                                            cx,
                                        ) {
                                            eprintln!(
                                                "cef-example SourceViewServices register tab={tab_id}: {err:?}"
                                            );
                                        }
                                    }
                                    root.menu_presenters
                                        .insert(tab_id, Rc::new(RefCell::new(presenter)));
                                    reg_out = Some(reg_arc);
                                }
                            }
                            ExampleView::Osr(view)
                        })
                }
                ExampleHostMode::NativeHwnd => {
                    NativeCefBrowser::new(attachment, window, cx)
                        .map_err(|e| e.to_string())
                        .map(|view| {
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
                                wire_view_callbacks!(
                                    v,
                                    tab_id,
                                    root_weak,
                                    cap_queues,
                                    cap_wake,
                                    config.verify_tests,
                                    find_open_flag
                                );
                            });
                            ExampleView::Native(view)
                        })
                }
            };
            let example_view = match built {
                Ok(v) => v,
                Err(err) => {
                    root.model.close_tab(tab_id);
                    return Err(err);
                }
            };
            root.views.insert(tab_id, example_view);
            root.sync_native_tab_visibility(cx);
            cx.notify();
            Ok::<_, String>(reg_out)
        })
        .map_err(|e| e.to_string())??;

    if menu_reg.is_none() || ux.finds.is_none() {
        if let Some(rt) = rt_cell.borrow().as_ref() {
            let _ = rt.set_find_sink(browser_id, Some(ux.direct_finds.sink_for(browser_id)));
        }
    }
    if menu_reg.is_none() || ux.sources.is_none() {
        if let Some(rt) = rt_cell.borrow().as_ref() {
            let _ =
                rt.set_source_viewer(browser_id, Some(ux.direct_sources.viewer_for(browser_id)));
        }
    }

    accounting.tabs_opened += 1;
    tabs.insert(
        tab_id,
        TabRuntime {
            browser_id,
            handle,
            first_paint: false,
            closing_pending: false,
            menu_reg,
            smoke_eval_id: None,
            smoke_rf_eval_id: None,
            smoke_bench_eval_id: None,
            smoke_post_eval_id: None,
        },
    );
    Ok(())
}

fn begin_close_example_tab(
    cx: &mut AsyncApp,
    root: WindowHandle<CefExampleRoot>,
    tab_id: u64,
    tabs: &mut BTreeMap<u64, TabRuntime>,
) {
    let Some(tab_rt) = tabs.get_mut(&tab_id) else {
        return;
    };
    if tab_rt.closing_pending {
        return;
    }
    tab_rt.closing_pending = true;
    let _ = root.update(cx, |root, _, cx| {
        if let Some(tab) = root.model.tab_mut(tab_id) {
            tab.closing_pending = true;
        }
        cx.notify();
    });
    let _ = tab_rt.handle.request_close(CloseMode::Graceful);
}

#[allow(clippy::too_many_arguments)]
fn finalize_close_example_tab(
    cx: &mut AsyncApp,
    rt_cell: &Rc<RefCell<Option<Runtime>>>,
    menus: Option<&Rc<RefCell<ContextMenuServices>>>,
    ux: &mut UxServices,
    file_dialogs: &mut FileDialogTracker,
    pending_save_as: &mut BTreeMap<DownloadId, (u64, u64, String, DownloadRequest)>,
    root: WindowHandle<CefExampleRoot>,
    tab_id: u64,
    accounting: &mut ExampleAccounting,
    tabs: &mut BTreeMap<u64, TabRuntime>,
    force_close: bool,
) -> Result<(), String> {
    let Some(tab_rt) = tabs.remove(&tab_id) else {
        return Ok(());
    };
    let mut cancelled_downloads = Vec::new();
    if let Ok(Some(dl_id)) = root.update(cx, |root, _, _| {
        root.model
            .tab_mut(tab_id)
            .and_then(|t| cancel_active_download_on_close(&mut t.download, tab_rt.browser_id))
    }) {
        cancelled_downloads.push(dl_id);
    }
    let pending_ids: Vec<DownloadId> = pending_save_as
        .keys()
        .copied()
        .filter(|id| id.browser == tab_rt.browser_id)
        .collect();
    for dl_id in pending_ids {
        pending_save_as.remove(&dl_id);
        if !cancelled_downloads.contains(&dl_id) {
            cancelled_downloads.push(dl_id);
        }
    }
    if let Some(rt) = rt_cell.borrow().as_ref() {
        for dl_id in cancelled_downloads {
            let _ = rt.cancel_download(dl_id);
        }
        let _ = rt.set_find_sink(tab_rt.browser_id, None);
        let _ = rt.set_source_viewer(tab_rt.browser_id, None);
    }
    file_dialogs.clear_for_browser(tab_rt.browser_id);
    ux.direct_finds.unregister(tab_rt.browser_id);
    let _ = cx.update(|app| {
        ux.direct_sources.unregister_browser(tab_rt.browser_id, app);
    });

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
    let had_menu_reg = tab_rt.menu_reg.is_some();
    drop(tab_rt.menu_reg);
    if let Some(menus) = menus {
        let _ = menus.borrow_mut().unregister(tab_rt.browser_id);
    }

    let removed = root
        .update(cx, |root, _, cx| {
            root.menu_presenters.remove(&tab_id);
            root.model
                .clear_modals_for_tab(tab_id, Some(tab_rt.browser_id));
            if root.model.pending_cert.is_empty() {
                root.selected_cert_index = None;
            }
            if matches!(root.model.active_input, ActiveInput::FindQuery)
                && root.model.active_tab == Some(tab_id)
            {
                root.model.active_input = ActiveInput::None;
            }
            root.model.close_tab(tab_id);
            let v = root.views.remove(&tab_id);
            root.sync_native_tab_visibility(cx);
            cx.notify();
            v
        })
        .map_err(|e| e.to_string())?;
    let diag = if let Some(view) = removed {
        cx.update_window(root.into(), |_, window, cx| {
            view.release_or_retire(window, cx)
        })
        .map_err(|e| e.to_string())?
    } else {
        None
    };
    if let Some(d) = diag {
        accounting.created_images += d.created_images;
        accounting.released_images += d.released_images;
    }
    if had_menu_reg {
        ux.retired_browsers.push(tab_rt.browser_id);
        ux.sweep_retired(cx);
    }
    if force_close && tab_rt.handle.snapshot().state != SessionState::Closed {
        let _ = tab_rt.handle.request_close(CloseMode::Force);
    }
    accounting.tabs_closed += 1;
    Ok(())
}

fn find_owner_tab(tabs: &BTreeMap<u64, TabRuntime>, browser: BrowserId) -> Option<u64> {
    tabs.iter()
        .find(|(_, t)| t.browser_id == browser)
        .map(|(id, _)| *id)
}

fn dispatch_find_command_for_tab(
    cx: &mut AsyncApp,
    root: WindowHandle<CefExampleRoot>,
    tabs: &BTreeMap<u64, TabRuntime>,
    rt_cell: &Rc<RefCell<Option<Runtime>>>,
    tab_id: u64,
    cmd: FindCommandKind,
) {
    let Some(browser_id) = tabs.get(&tab_id).map(|t| t.browser_id) else {
        return;
    };
    let rt_guard = rt_cell.borrow();
    let Some(rt) = rt_guard.as_ref() else {
        return;
    };
    match cmd {
        FindCommandKind::Start {
            query,
            forward,
            match_case,
            find_next,
        } => {
            if let Err(err) = rt.find(browser_id, &query, forward, match_case, find_next) {
                let _ = root.update(cx, |r, _, cx| {
                    if let Some(t) = r.model.tab_mut(tab_id) {
                        handle_find_dispatch_error(&mut t.find, &err);
                    }
                    cx.notify();
                });
            }
        }
        FindCommandKind::StopClearSelection => {
            let _ = rt.stop_finding(browser_id, true);
        }
    }
}

fn trigger_find_for_tab(
    cx: &mut AsyncApp,
    root: WindowHandle<CefExampleRoot>,
    tabs: &BTreeMap<u64, TabRuntime>,
    rt_cell: &Rc<RefCell<Option<Runtime>>>,
    tab_id: u64,
    forward: bool,
    force_find_next: bool,
) {
    let planned = root
        .update(cx, |r, _, cx| {
            let t = r.model.tab_mut(tab_id)?;
            let cmd = plan_find_command(&mut t.find, forward, force_find_next);
            cx.notify();
            cmd
        })
        .ok()
        .flatten();
    if let Some(cmd) = planned {
        dispatch_find_command_for_tab(cx, root, tabs, rt_cell, tab_id, cmd);
    }
}

fn open_find_bar_for_tab(
    cx: &mut AsyncApp,
    root: WindowHandle<CefExampleRoot>,
    tab_id: u64,
) -> bool {
    cx.update_window(root.into(), |view, window, cx| {
        let Ok(entity) = view.downcast::<CefExampleRoot>() else {
            return false;
        };
        entity.update(cx, |r, cx| {
            if r.model.active_tab != Some(tab_id) {
                r.model.switch_tab(tab_id);
            }
            let mut should_search = false;
            if let Some(t) = r.model.tab_mut(tab_id) {
                let was_open = t.find.open;
                t.find.set_open(true);
                t.find.query.focus_select_all();
                if !t.find.quarantined {
                    t.find.last_error = None;
                }
                should_search = !was_open && !t.find.query.text.is_empty() && !t.find.quarantined;
            }
            r.model.sync_active_find_flag();
            r.model.active_input = ActiveInput::FindQuery;
            text_input::ensure_parent_window_win32_focus(window);
            let handle = r.input_focus.clone();
            window.focus(&handle, cx);
            window.request_virtual_keyboard();
            window.notify_text_input_changed(&handle);
            r.sync_native_tab_visibility(cx);
            cx.notify();
            should_search
        })
    })
    .unwrap_or(false)
}

fn close_find_bar_for_tab(
    cx: &mut AsyncApp,
    root: WindowHandle<CefExampleRoot>,
    tabs: &BTreeMap<u64, TabRuntime>,
    rt_cell: &Rc<RefCell<Option<Runtime>>>,
    tab_id: u64,
) {
    let planned = cx
        .update_window(root.into(), |view, window, cx| {
            let Ok(entity) = view.downcast::<CefExampleRoot>() else {
                return None;
            };
            entity.update(cx, |r, cx| {
                let mut closed_query = false;
                let mut cmd = None;
                if let Some(t) = r.model.tab_mut(tab_id) {
                    cmd = plan_close_find_bar(&mut t.find);
                    closed_query = r.model.active_tab == Some(tab_id)
                        && r.model.active_input == ActiveInput::FindQuery;
                }
                r.model.sync_active_find_flag();
                if closed_query {
                    r.model.active_input = ActiveInput::None;
                    if let Some(browser_view) = r.views.get(&tab_id).cloned() {
                        let handle = browser_view.focus_handle(cx);
                        window.focus(&handle, cx);
                    } else {
                        let root_focus = r.focus.clone();
                        window.focus(&root_focus, cx);
                    }
                }
                cx.notify();
                cmd
            })
        })
        .ok()
        .flatten();
    if let Some(cmd) = planned {
        dispatch_find_command_for_tab(cx, root, tabs, rt_cell, tab_id, cmd);
    }
}

fn apply_find_status_update(
    cx: &mut AsyncApp,
    root: WindowHandle<CefExampleRoot>,
    tabs: &BTreeMap<u64, TabRuntime>,
    rt_cell: &Rc<RefCell<Option<Runtime>>>,
    browser_id: BrowserId,
    status: &rust_cef_core::find::FindStatus,
) {
    let Some(tab_id) = find_owner_tab(tabs, browser_id) else {
        return;
    };
    let next_cmd = root
        .update(cx, |r, _, cx| {
            let t = r.model.tab_mut(tab_id)?;
            let cmd = apply_find_status(&mut t.find, status);
            cx.notify();
            cmd
        })
        .ok()
        .flatten();
    if let Some(cmd) = next_cmd {
        dispatch_find_command_for_tab(cx, root, tabs, rt_cell, tab_id, cmd);
    }
}

fn drain_find_updates(
    cx: &mut AsyncApp,
    root: WindowHandle<CefExampleRoot>,
    tabs: &BTreeMap<u64, TabRuntime>,
    rt_cell: &Rc<RefCell<Option<Runtime>>>,
    ux: &mut UxServices,
) {
    if let Some(finds) = ux.finds.as_mut() {
        let updates = cx.update(|app| finds.dispatch(app));
        for upd in updates {
            apply_find_status_update(cx, root, tabs, rt_cell, upd.browser, &upd.status);
        }
    }
    for FindUiUpdate { browser, status } in ux.direct_finds.drain_updates() {
        apply_find_status_update(cx, root, tabs, rt_cell, browser, &status);
    }
}

fn report_source_failure(
    cx: &mut AsyncApp,
    root: WindowHandle<CefExampleRoot>,
    tabs: &BTreeMap<u64, TabRuntime>,
    ux: &mut UxServices,
    browser_id: BrowserId,
    status: &rust_cef_core::source_view::SourceOperationStatus,
) {
    let Some(ref failure) = status.first_failure else {
        return;
    };
    let key = (status.operation.browser, status.operation.sequence);
    if ux.reported_source_ops.contains(&key) {
        return;
    }
    ux.reported_source_ops.push(key);
    let (title, message) = format_source_failure(status.operation, failure);
    let tab_id = find_owner_tab(tabs, browser_id);
    let _ = root.update(cx, |r, _, cx| {
        if let Some(tid) = tab_id
            && let Some(t) = r.model.tab_mut(tid)
        {
            t.output_message = message.clone();
        }
        r.model.notice_dialog = Some(NoticeDialog { title, message });
        r.sync_native_tab_visibility(cx);
        cx.notify();
    });
}

fn spawn_os_file_dialog(
    cx: &mut AsyncApp,
    root: WindowHandle<CefExampleRoot>,
    req: &FileDialogRequest,
    fallback_dir: &Path,
) {
    let request_id = req.id;
    let mode = req.mode;
    let default_path = req.default_file_path.clone();
    let dir_fallback = fallback_dir.to_path_buf();
    cx.spawn(async move |cx: &mut AsyncApp| {
        let paths = match mode {
            FileDialogKind::Save => {
                let (dir, suggested) = split_save_default_path(&default_path, &dir_fallback);
                let rx = root
                    .update(cx, |_, _, cx| {
                        cx.prompt_for_new_path(&dir, suggested.as_deref())
                    })
                    .ok();
                match rx {
                    Some(rx) => match rx.await {
                        Ok(Ok(Some(path))) => Some(vec![path]),
                        _ => None,
                    },
                    None => None,
                }
            }
            _ => {
                let opts = build_path_prompt_options(mode);
                let rx = root.update(cx, |_, _, cx| cx.prompt_for_paths(opts)).ok();
                match rx {
                    Some(rx) => match rx.await {
                        Ok(Ok(Some(selected))) => Some(selected),
                        _ => None,
                    },
                    None => None,
                }
            }
        };
        let _ = root.update(cx, |r, _, _| {
            r.emit(ExampleAction::CompleteFileDialog { request_id, paths });
        });
    })
    .detach();
}

fn spawn_download_save_as_dialog(
    cx: &mut AsyncApp,
    root: WindowHandle<CefExampleRoot>,
    req: &DownloadRequest,
    fallback_dir: &Path,
    suggested_name: &str,
) {
    let request_id = req.id;
    let dir = fallback_dir.to_path_buf();
    let name = suggested_name.to_owned();
    cx.spawn(async move |cx: &mut AsyncApp| {
        let rx = root
            .update(cx, |_, _, cx| cx.prompt_for_new_path(&dir, Some(&name)))
            .ok();
        let path = match rx {
            Some(rx) => match rx.await {
                Ok(Ok(chosen)) => chosen,
                _ => None,
            },
            None => None,
        };
        let _ = root.update(cx, |r, _, _| {
            r.emit(ExampleAction::CompleteSaveAsDownload { request_id, path });
        });
    })
    .detach();
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
        file_dialog: Some(cap_adapter.clone()),
        js_dialog: Some(cap_adapter.clone()),
        permission: Some(cap_adapter.clone()),
        auth: Some(cap_adapter.clone()),
        certificate: Some(cap_adapter.clone()),
        ..Default::default()
    });
    let _ = rt.configure_devtools(nz(MAX_EXAMPLE_TABS));
    let _ = rt.configure_finds(FindLimits {
        capacity: nz(MAX_EXAMPLE_TABS),
        max_query_bytes: nz(256),
    });
    let _ = rt.configure_source_views(SourceViewLimits {
        capacity: nz(MAX_EXAMPLE_TABS),
        max_source_bytes: nz(2 * 1024 * 1024),
    });
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
    let source_bounds = cx.update(|app| Bounds::centered(None, size(px(900.0), px(700.0)), app));
    let finds = menus
        .as_ref()
        .map(|_| FindServices::new(nz(MAX_EXAMPLE_TABS), nz(256)));
    let sources = menus.as_ref().map(|_| {
        SourceViewServices::new(
            nz(MAX_EXAMPLE_TABS),
            SourceViewOptions {
                max_operations: nz(4),
                max_source_bytes: nz(2 * 1024 * 1024),
                bounds: source_bounds,
                focus: true,
            },
        )
    });
    let mut ux = UxServices::new(finds, sources, sender.clone(), 2 * 1024 * 1024);
    let mut file_dialogs = FileDialogTracker::new();
    let mut pending_file_dialogs: BTreeMap<HostRequestId, FileDialogKind> = BTreeMap::new();
    let mut pending_save_as: BTreeMap<DownloadId, (u64, u64, String, DownloadRequest)> =
        BTreeMap::new();
    let rt_cell = Rc::new(RefCell::new(Some(rt)));
    let mut tabs: BTreeMap<u64, TabRuntime> = BTreeMap::new();
    let mut accounting = ExampleAccounting::default();
    let mut next_tab_id = 1u64;
    let mut active_pdf: Option<(u64, PathBuf, PdfExportTicket)> = None;
    let mut drop_target_owned: Option<(NativeDropTargetServices, u64)> = None;
    let mut rpc_host = CefRsGpuiRpcHost::default();
    let mut verify_driver = config.verify_tests.then(verify::VerifyDriver::new);
    let mut smoke_stage = 0u8;
    let smoke_deadline = config
        .smoke
        .map(|d| Instant::now() + d + Duration::from_secs(20));

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
            &mut ux,
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

    let mut window_close_pending = false;
    let mut force_quit = false;
    let _ = receiver.try_recv();
    while run_error.is_none() && !force_quit {
        if let Some(err) = obs
            .first_fault
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            run_error = Some(err);
            break;
        }
        if close.get() {
            if config.smoke.is_some() || config.verify_tests || tabs.is_empty() {
                break;
            }
            if !window_close_pending {
                window_close_pending = true;
                let _ = root.update(cx, |r, _, cx| {
                    r.model.window_close_pending = true;
                    cx.notify();
                });
                let open_ids: Vec<u64> = tabs.keys().copied().collect();
                for id in open_ids {
                    begin_close_example_tab(cx, root, id, &mut tabs);
                }
            }
        }
        let quit_from_tabs = root
            .update(cx, |r, _, _| r.model.quit_requested)
            .unwrap_or(true);
        if quit_from_tabs {
            break;
        }

        // Process host capability cancellations first so stale modals are pruned.
        for id in cap_queues
            .file_dialog_cancellations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            file_dialogs.cancel_request(id);
            pending_file_dialogs.remove(&id);
            let _ = root.update(cx, |r, _, cx| {
                if r.model.clear_active_file_dialog_if(id) {
                    r.sync_native_tab_visibility(cx);
                    cx.notify();
                }
            });
        }
        for id in cap_queues
            .js_dialog_cancellations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            let _ = root.update(cx, |r, _, cx| {
                if r.model
                    .pending_js_dialog
                    .as_ref()
                    .is_some_and(|d| d.id == id)
                {
                    r.model.pending_js_dialog = None;
                    if r.model.active_input == ActiveInput::JsPrompt {
                        r.model.active_input = ActiveInput::None;
                    }
                    r.sync_native_tab_visibility(cx);
                    cx.notify();
                }
            });
        }
        for id in cap_queues
            .permission_cancellations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            let _ = root.update(cx, |r, _, cx| {
                if r.model.cancel_permission(id) {
                    r.sync_native_tab_visibility(cx);
                    cx.notify();
                }
            });
        }
        for id in cap_queues
            .auth_cancellations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            let _ = cx.update_window(root.into(), |view, window, cx| {
                let Ok(entity) = view.downcast::<CefExampleRoot>() else {
                    return;
                };
                entity.update(cx, |r, cx| {
                    if r.model.cancel_auth(id) {
                        if !r.model.pending_auth.is_empty()
                            && r.model.active_input == ActiveInput::None
                        {
                            r.model.active_input = ActiveInput::AuthUser;
                            text_input::ensure_parent_window_win32_focus(window);
                            let handle = r.input_focus.clone();
                            window.focus(&handle, cx);
                        }
                        r.sync_native_tab_visibility(cx);
                        cx.notify();
                    }
                });
            });
        }
        for id in cap_queues
            .cert_cancellations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            let _ = root.update(cx, |r, _, cx| {
                if r.model.cancel_cert(id) {
                    r.selected_cert_index =
                        r.model.pending_cert.front().and_then(|c| match &c.kind {
                            PendingCertKind::ClientSelect(req) => {
                                req.certificates.first().map(|x| x.index)
                            }
                            PendingCertKind::ServerError(_) => None,
                        });
                    r.sync_native_tab_visibility(cx);
                    cx.notify();
                }
            });
        }

        // Drain finished Find updates before dispatching new shortcuts/actions in this turn.
        drain_find_updates(cx, root, &tabs, &rt_cell, &mut ux);

        // Process browser-originated shortcuts, PreKeyFilter Esc, and BeforeUnload events.
        for tab_id in cap_queues
            .escape_find_tabs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            close_find_bar_for_tab(cx, root, &tabs, &rt_cell, tab_id);
        }
        for (tab_id, shortcut_id) in cap_queues
            .shortcut_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            let blocked = root
                .update(cx, |r, _, _| r.model.has_blocking_modal())
                .unwrap_or(false);
            if blocked {
                continue;
            }
            match shortcut_id.0 {
                1 => {
                    if open_find_bar_for_tab(cx, root, tab_id) {
                        trigger_find_for_tab(cx, root, &tabs, &rt_cell, tab_id, true, false);
                    }
                }
                2 => {
                    let is_open = root
                        .update(cx, |r, _, _| {
                            r.model.tab(tab_id).is_some_and(|t| t.find.open)
                        })
                        .unwrap_or(false);
                    if !is_open {
                        if open_find_bar_for_tab(cx, root, tab_id) {
                            trigger_find_for_tab(cx, root, &tabs, &rt_cell, tab_id, true, false);
                        }
                    } else {
                        trigger_find_for_tab(cx, root, &tabs, &rt_cell, tab_id, true, true);
                    }
                }
                3 => {
                    let is_open = root
                        .update(cx, |r, _, _| {
                            r.model.tab(tab_id).is_some_and(|t| t.find.open)
                        })
                        .unwrap_or(false);
                    if !is_open {
                        if open_find_bar_for_tab(cx, root, tab_id) {
                            trigger_find_for_tab(cx, root, &tabs, &rt_cell, tab_id, true, false);
                        }
                    } else {
                        trigger_find_for_tab(cx, root, &tabs, &rt_cell, tab_id, false, true);
                    }
                }
                4 => {
                    if let Some(browser_id) = tabs.get(&tab_id).map(|t| t.browser_id)
                        && let Some(rt) = rt_cell.borrow().as_ref()
                        && let Err(err) = rt.view_source(browser_id)
                    {
                        let _ = root.update(cx, |r, _, cx| {
                            if let Some(t) = r.model.tab_mut(tab_id) {
                                t.output_message = format!("View Source failed: {err:?}");
                            }
                            cx.notify();
                        });
                    }
                }
                5 => {
                    let id = next_tab_id;
                    next_tab_id += 1;
                    if let Err(e) = spawn_example_tab(
                        cx,
                        &rt_cell,
                        menus.as_ref(),
                        &mut ux,
                        &cap_adapter,
                        root,
                        &config,
                        id,
                        DEFAULT_HOME_URL.into(),
                        false,
                        false,
                        &mut accounting,
                        &mut tabs,
                    ) {
                        run_error = Some(e);
                        break;
                    }
                }
                6 => begin_close_example_tab(cx, root, tab_id, &mut tabs),
                _ => {}
            }
        }
        for (tab_id, request, is_reload) in cap_queues
            .before_unload_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            if config.smoke.is_some() {
                let _ = root.update(cx, |r, _, cx| {
                    if let Some(v) = r.views.get(&tab_id).cloned() {
                        v.resolve_before_unload(request, true, cx);
                    }
                });
            } else if let Some(browser_id) = tabs.get(&tab_id).map(|t| t.browser_id) {
                let _ = root.update(cx, |r, _, cx| {
                    r.model.enqueue_before_unload(PendingBeforeUnloadModal {
                        tab_id,
                        browser_id,
                        request,
                        is_reload,
                    });
                    r.sync_native_tab_visibility(cx);
                    cx.notify();
                });
            }
        }
        for tab_id in cap_queues
            .close_cancelled_tabs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            if let Some(t) = tabs.get_mut(&tab_id) {
                t.closing_pending = false;
            }
            window_close_pending = false;
            close.set(false);
            let _ = root.update(cx, |r, _, cx| {
                if let Some(t) = r.model.tab_mut(tab_id) {
                    t.closing_pending = false;
                }
                r.model.window_close_pending = false;
                cx.notify();
            });
        }

        // Process host capability queues (file dialogs, permissions, auth, certs, JS dialogs, downloads, popups).
        for req in cap_queues
            .file_dialogs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            if config.smoke.is_some() {
                if let Some(rt) = rt_cell.borrow().as_ref() {
                    let _ = rt.resolve_file_dialog(req.id, FileDialogDecision::Cancel);
                }
            } else if !file_dialogs.record_request(req.id) {
                if let Some(rt) = rt_cell.borrow().as_ref() {
                    let _ = rt.resolve_file_dialog(req.id, FileDialogDecision::Cancel);
                }
            } else {
                pending_file_dialogs.insert(req.id, req.mode);
                let _ = root.update(cx, |r, _, cx| {
                    r.model.active_native_file_dialog = Some(req.id);
                    r.sync_native_tab_visibility(cx);
                    cx.notify();
                });
                spawn_os_file_dialog(cx, root, &req, &output_dir);
            }
        }
        for req in cap_queues
            .permissions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            if config.smoke.is_some() {
                if let Some(rt) = rt_cell.borrow().as_ref() {
                    let _ = rt.resolve_permission(req.id, PermissionDecision::Deny);
                }
            } else {
                let owner_id = find_owner_tab(&tabs, req.id.browser);
                let _ = root.update(cx, |r, _, cx| {
                    let tab_id = owner_id.or(r.model.active_tab).unwrap_or(0);
                    r.model.enqueue_permission(PendingPermissionModal {
                        tab_id,
                        browser_id: req.id.browser,
                        request: req,
                    });
                    r.sync_native_tab_visibility(cx);
                    cx.notify();
                });
            }
        }
        for req in cap_queues
            .auths
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            if config.smoke.is_some() {
                if let Some(rt) = rt_cell.borrow().as_ref() {
                    let _ = rt.resolve_auth(req.id, AuthDecision::Cancel);
                }
            } else {
                let owner_id = find_owner_tab(&tabs, req.id.browser);
                let _ = cx.update_window(root.into(), |view, window, cx| {
                    let Ok(entity) = view.downcast::<CefExampleRoot>() else {
                        return;
                    };
                    entity.update(cx, |r, cx| {
                        let tab_id = owner_id.or(r.model.active_tab).unwrap_or(0);
                        let was_empty = r.model.pending_auth.is_empty();
                        r.model.enqueue_auth(model::PendingAuthModal {
                            tab_id,
                            browser_id: req.id.browser,
                            request: req,
                            username_input: TextBuffer::default(),
                            password_input: TextBuffer::default(),
                        });
                        if was_empty {
                            r.model.active_input = ActiveInput::AuthUser;
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
        for req in cap_queues
            .cert_errors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            if config.smoke.is_some() {
                if let Some(rt) = rt_cell.borrow().as_ref() {
                    let _ = rt.resolve_certificate(req.id, CertificateDecision::Cancel);
                }
            } else {
                let owner_id = find_owner_tab(&tabs, req.id.browser);
                let _ = root.update(cx, |r, _, cx| {
                    let tab_id = owner_id.or(r.model.active_tab).unwrap_or(0);
                    r.model.enqueue_cert(PendingCertModal {
                        tab_id,
                        browser_id: req.id.browser,
                        kind: PendingCertKind::ServerError(req),
                    });
                    if r.model.pending_cert.len() == 1 {
                        r.selected_cert_index = None;
                    }
                    r.sync_native_tab_visibility(cx);
                    cx.notify();
                });
            }
        }
        for req in cap_queues
            .client_certs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            if config.smoke.is_some() {
                if let Some(rt) = rt_cell.borrow().as_ref() {
                    let _ = rt.resolve_certificate(req.id, CertificateDecision::Cancel);
                }
            } else {
                let owner_id = find_owner_tab(&tabs, req.id.browser);
                let first_idx = req.certificates.first().map(|c| c.index);
                let _ = root.update(cx, |r, _, cx| {
                    let tab_id = owner_id.or(r.model.active_tab).unwrap_or(0);
                    r.model.enqueue_cert(PendingCertModal {
                        tab_id,
                        browser_id: req.id.browser,
                        kind: PendingCertKind::ClientSelect(req),
                    });
                    if r.model.pending_cert.len() == 1 {
                        r.selected_cert_index = first_idx;
                    }
                    r.sync_native_tab_visibility(cx);
                    cx.notify();
                });
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
                    let _ = rt
                        .resolve_js_dialog(req.id, JsDialogDecision::Accept { prompt_input: None });
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
            let clean_name = dialogs::sanitize_download_filename(&req.suggested_name);
            let owner_id = find_owner_tab(&tabs, req.id.browser);
            let ask_save_as = root
                .update(cx, |r, _, _| r.model.ask_save_as)
                .unwrap_or(false)
                && config.smoke.is_none();
            if ask_save_as {
                let mut target_tab_id = owner_id.unwrap_or(0);
                let _ = root.update(cx, |r, _, cx| {
                    let tid = owner_id.or(r.model.active_tab);
                    if let Some(tid) = tid
                        && let Some(t) = r.model.tab_mut(tid)
                    {
                        target_tab_id = tid;
                        t.apply_download_request(req.id.browser, &req);
                        t.download.last_action = "PromptSaveAs".into();
                        t.download.suggested_file_name = clean_name.clone();
                        t.download.full_path.clear();
                        t.show_download_info = true;
                    }
                    r.model.active_save_as_download = Some(req.id);
                    r.sync_native_tab_visibility(cx);
                    cx.notify();
                });
                pending_save_as.insert(
                    req.id,
                    (target_tab_id, seq, clean_name.clone(), req.clone()),
                );
                spawn_download_save_as_dialog(cx, root, &req, &output_dir, &clean_name);
            } else {
                let dest = match resolve_download_destination(
                    &output_dir,
                    seq,
                    &req.suggested_name,
                    None,
                    &[],
                ) {
                    Ok(p) => p,
                    Err(err) => {
                        let _ = root.update(cx, |r, _, cx| {
                            if let Some(t) = owner_id
                                .or(r.model.active_tab)
                                .and_then(|id| r.model.tab_mut(id))
                            {
                                t.apply_download_request(req.id.browser, &req);
                                t.mark_download_cancelled(format!("Invalid destination: {err:?}"));
                                t.show_download_info = true;
                            }
                            cx.notify();
                        });
                        if let Some(rt) = rt_cell.borrow().as_ref() {
                            let _ = rt.cancel_download(req.id);
                        }
                        continue;
                    }
                };
                let _ = root.update(cx, |r, _, cx| {
                    let tid = owner_id.or(r.model.active_tab);
                    if let Some(t) = tid.and_then(|id| r.model.tab_mut(id)) {
                        t.apply_download_request(req.id.browser, &req);
                        t.download.suggested_file_name = clean_name.clone();
                        t.download.full_path = dest.to_string_lossy().into_owned();
                        t.show_download_info = true;
                    }
                    cx.notify();
                });
                if let Some(rt) = rt_cell.borrow().as_ref() {
                    let _ = rt.confirm_download(req.id, dest);
                }
            }
        }
        for prog in cap_queues
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            let owner_id = find_owner_tab(&tabs, prog.id.browser);
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
                &mut ux,
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
                        format!(
                            "Unable to save Pdf, check you have write permissions to {path_str}"
                        )
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
                    if config.smoke.is_some() || config.verify_tests || tabs.is_empty() {
                        force_quit = true;
                        break;
                    }
                    close.set(true);
                    if !window_close_pending {
                        window_close_pending = true;
                        let _ = root.update(cx, |r, _, cx| {
                            r.model.window_close_pending = true;
                            cx.notify();
                        });
                        let open_ids: Vec<u64> = tabs.keys().copied().collect();
                        for id in open_ids {
                            begin_close_example_tab(cx, root, id, &mut tabs);
                        }
                    }
                }
                ExampleAction::NewTab(opt_url) => {
                    let id = next_tab_id;
                    next_tab_id += 1;
                    let url = opt_url.unwrap_or_else(|| DEFAULT_HOME_URL.into());
                    if let Err(e) = spawn_example_tab(
                        cx,
                        &rt_cell,
                        menus.as_ref(),
                        &mut ux,
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
                        &mut ux,
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
                    begin_close_example_tab(cx, root, id, &mut tabs);
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
                        if let Some(v) = r.model.active_tab.and_then(|id| r.views.get(&id)).cloned()
                        {
                            v.go_back(cx);
                        }
                    });
                }
                ExampleAction::Forward => {
                    let _ = root.update(cx, |r, _, cx| {
                        if let Some(v) = r.model.active_tab.and_then(|id| r.views.get(&id)).cloned()
                        {
                            v.go_forward(cx);
                        }
                    });
                }
                ExampleAction::Reload => {
                    let _ = root.update(cx, |r, _, cx| {
                        if let Some(v) = r.model.active_tab.and_then(|id| r.views.get(&id)).cloned()
                        {
                            v.reload(cx);
                        }
                    });
                }
                ExampleAction::StopLoading => {
                    let _ = root.update(cx, |r, _, cx| {
                        if let Some(v) = r.model.active_tab.and_then(|id| r.views.get(&id)).cloned()
                        {
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
                        if let Some(v) = r.model.active_tab.and_then(|id| r.views.get(&id)).cloned()
                        {
                            v.toggle_audio_muted(cx);
                        }
                    });
                }
                ExampleAction::ToggleDevTools => {
                    if let Some(active_id) =
                        root.update(cx, |r, _, _| r.model.active_tab).ok().flatten()
                        && let Some(tab) = tabs.get(&active_id)
                    {
                        let open_now = root
                            .update(cx, |r, _, _| {
                                r.model.active_tab().is_some_and(|t| t.devtools_open)
                            })
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
                        let res =
                            rt.clear_profile_http_auth_credentials(rt.default_profile_handle());
                        let _ = root.update(cx, |r, _, cx| {
                            if let Some(t) = r.model.active_tab_mut() {
                                t.output_message =
                                    format!("ClearHttpAuthCredentials: {:?}", res.is_ok());
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
                        if let Some(v) = r.model.active_tab.and_then(|id| r.views.get(&id)).cloned()
                        {
                            v.execute_javascript(script, cx);
                        }
                    });
                }
                ExampleAction::EvaluateJs(script) => {
                    let _ = root.update(cx, |r, _, cx| {
                        if let Some(v) = r.model.active_tab.and_then(|id| r.views.get(&id)).cloned()
                        {
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
                                t.output_message = format_runtime_summary(
                                    "Submitting Load Custom Request POST...",
                                );
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
                        &mut ux,
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
                        &mut ux,
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
                ExampleAction::OpenFindBar => {
                    if let Ok(Some(id)) = root.update(cx, |r, _, _| r.model.active_tab)
                        && open_find_bar_for_tab(cx, root, id)
                    {
                        trigger_find_for_tab(cx, root, &tabs, &rt_cell, id, true, false);
                    }
                }
                ExampleAction::CloseFindBar => {
                    if let Ok(Some(id)) = root.update(cx, |r, _, _| r.model.active_tab) {
                        close_find_bar_for_tab(cx, root, &tabs, &rt_cell, id);
                    }
                }
                ExampleAction::FindToggleCase => {
                    if let Ok(Some(tab_id)) = root.update(cx, |r, _, cx| {
                        let id = r.model.active_tab?;
                        if let Some(t) = r.model.tab_mut(id) {
                            t.find.match_case = !t.find.match_case;
                        }
                        cx.notify();
                        Some(id)
                    }) {
                        trigger_find_for_tab(cx, root, &tabs, &rt_cell, tab_id, true, false);
                    }
                }
                ExampleAction::FindDispatch {
                    forward,
                    force_find_next,
                } => {
                    if let Ok(Some((tab_id, was_open))) = root.update(cx, |r, _, _| {
                        let id = r.model.active_tab?;
                        let open = r.model.tab(id).is_some_and(|t| t.find.open);
                        Some((id, open))
                    }) {
                        if !was_open {
                            let _ = open_find_bar_for_tab(cx, root, tab_id);
                        }
                        trigger_find_for_tab(
                            cx,
                            root,
                            &tabs,
                            &rt_cell,
                            tab_id,
                            forward,
                            force_find_next,
                        );
                    }
                }
                ExampleAction::ViewSource => {
                    if let Ok(Some(tab_id)) = root.update(cx, |r, _, _| r.model.active_tab)
                        && let Some(browser_id) = tabs.get(&tab_id).map(|t| t.browser_id)
                        && let Some(rt) = rt_cell.borrow().as_ref()
                        && let Err(err) = rt.view_source(browser_id)
                    {
                        let _ = root.update(cx, |r, _, cx| {
                            if let Some(t) = r.model.tab_mut(tab_id) {
                                t.output_message = format!("View Source failed: {err:?}");
                            }
                            cx.notify();
                        });
                    }
                }
                ExampleAction::ToggleAskSaveAs => {
                    let _ = root.update(cx, |r, _, cx| {
                        r.model.ask_save_as = !r.model.ask_save_as;
                        r.model.open_menu = None;
                        r.sync_native_tab_visibility(cx);
                        cx.notify();
                    });
                }
                ExampleAction::CancelDownload(tab_id) => {
                    if let Ok(Some(dl_id)) = root.update(cx, |r, _, cx| {
                        let dl_id = r.model.tab_mut(tab_id).and_then(|t| {
                            if t.download.is_complete || t.download.is_cancelled {
                                return None;
                            }
                            let id = t.download.download_id?;
                            t.mark_download_cancelled("Cancelled by user");
                            Some(id)
                        });
                        if let Some(id) = dl_id
                            && r.model.active_save_as_download == Some(id)
                        {
                            r.model.active_save_as_download = None;
                        }
                        r.sync_native_tab_visibility(cx);
                        cx.notify();
                        dl_id
                    }) {
                        pending_save_as.remove(&dl_id);
                        if let Some(rt) = rt_cell.borrow().as_ref() {
                            let _ = rt.cancel_download(dl_id);
                        }
                    }
                }
                ExampleAction::OpenContainingFolder(path) => {
                    let _ = cx.update(|app| {
                        app.reveal_path(&path);
                    });
                }
                ExampleAction::CompleteFileDialog { request_id, paths } => {
                    let _ = root.update(cx, |r, _, cx| {
                        if r.model.clear_active_file_dialog_if(request_id) {
                            r.sync_native_tab_visibility(cx);
                            cx.notify();
                        }
                    });
                    let mode = pending_file_dialogs.remove(&request_id);
                    if file_dialogs.take_for_resolution(request_id)
                        && let Some(kind) = mode
                        && let Some(rt) = rt_cell.borrow().as_ref()
                    {
                        let decision = match paths {
                            Some(list) => cap_selected_paths(list, kind),
                            None => FileDialogDecision::Cancel,
                        };
                        let _ = rt.resolve_file_dialog(request_id, decision);
                    }
                }
                ExampleAction::CompleteSaveAsDownload { request_id, path } => {
                    let _ = root.update(cx, |r, _, cx| {
                        if r.model.active_save_as_download == Some(request_id) {
                            r.model.active_save_as_download = None;
                            r.sync_native_tab_visibility(cx);
                            cx.notify();
                        }
                    });
                    if let Some((tab_id, seq, clean_name, _req)) =
                        pending_save_as.remove(&request_id)
                    {
                        match path {
                            Some(chosen) => {
                                match resolve_download_destination(
                                    &output_dir,
                                    seq,
                                    &clean_name,
                                    Some(chosen),
                                    &[],
                                ) {
                                    Ok(dest) => {
                                        let _ = root.update(cx, |r, _, cx| {
                                            if let Some(t) = r.model.tab_mut(tab_id) {
                                                t.download.last_action = "OnBeforeDownload".into();
                                                t.download.full_path =
                                                    dest.to_string_lossy().into_owned();
                                            }
                                            cx.notify();
                                        });
                                        if let Some(rt) = rt_cell.borrow().as_ref() {
                                            let _ = rt.confirm_download(request_id, dest);
                                        }
                                    }
                                    Err(err) => {
                                        let _ = root.update(cx, |r, _, cx| {
                                            if let Some(t) = r.model.tab_mut(tab_id) {
                                                t.mark_download_cancelled(format!(
                                                    "Invalid destination: {err:?}"
                                                ));
                                            }
                                            cx.notify();
                                        });
                                        if let Some(rt) = rt_cell.borrow().as_ref() {
                                            let _ = rt.cancel_download(request_id);
                                        }
                                    }
                                }
                            }
                            None => {
                                let _ = root.update(cx, |r, _, cx| {
                                    if let Some(t) = r.model.tab_mut(tab_id) {
                                        t.mark_download_cancelled("Cancelled by user");
                                    }
                                    cx.notify();
                                });
                                if let Some(rt) = rt_cell.borrow().as_ref() {
                                    let _ = rt.cancel_download(request_id);
                                }
                            }
                        }
                    }
                }
                ExampleAction::ResolvePermission { allow } => {
                    if let Ok(Some(modal)) = root.update(cx, |r, _, cx| {
                        let m = r.model.pop_front_permission();
                        r.sync_native_tab_visibility(cx);
                        cx.notify();
                        m
                    }) && let Some(rt) = rt_cell.borrow().as_ref()
                    {
                        let decision = if allow {
                            PermissionDecision::Allow {
                                media_mask: modal.request.requested_permissions,
                            }
                        } else {
                            PermissionDecision::Deny
                        };
                        let _ = rt.resolve_permission(modal.request.id, decision);
                    }
                }
                ExampleAction::ResolveAuth { submit } => {
                    if let Ok(Some(mut modal)) = root.update(cx, |r, _, cx| {
                        let m = r.model.pop_front_auth();
                        r.sync_native_tab_visibility(cx);
                        cx.notify();
                        m
                    }) {
                        let req_id = modal.request.id;
                        let decision = build_auth_decision_and_wipe(&mut modal, submit);
                        if let Some(rt) = rt_cell.borrow().as_ref() {
                            let _ = rt.resolve_auth(req_id, decision);
                        }
                        let _ = cx.update_window(root.into(), |view, window, cx| {
                            if let Ok(entity) = view.downcast::<CefExampleRoot>() {
                                entity.update(cx, |r, cx| {
                                    if !r.model.pending_auth.is_empty() {
                                        r.model.active_input = ActiveInput::AuthUser;
                                        text_input::ensure_parent_window_win32_focus(window);
                                        let handle = r.input_focus.clone();
                                        window.focus(&handle, cx);
                                        window.notify_text_input_changed(&handle);
                                        cx.notify();
                                    }
                                });
                            }
                        });
                    }
                }
                ExampleAction::ResolveCertificate { accept } => {
                    if let Ok(Some((modal, selected))) = root.update(cx, |r, _, cx| {
                        let sel = r.selected_cert_index;
                        let m = r.model.pop_front_cert();
                        r.selected_cert_index =
                            r.model.pending_cert.front().and_then(|c| match &c.kind {
                                PendingCertKind::ClientSelect(req) => {
                                    req.certificates.first().map(|x| x.index)
                                }
                                PendingCertKind::ServerError(_) => None,
                            });
                        r.sync_native_tab_visibility(cx);
                        cx.notify();
                        m.map(|modal| (modal, sel))
                    }) && let Some(rt) = rt_cell.borrow().as_ref()
                    {
                        let req_id = modal.request_id();
                        let decision = match &modal.kind {
                            PendingCertKind::ServerError(_) => {
                                if accept {
                                    CertificateDecision::Accept {
                                        certificate_index: None,
                                    }
                                } else {
                                    CertificateDecision::Cancel
                                }
                            }
                            PendingCertKind::ClientSelect(req) => {
                                if accept {
                                    let chosen = selected
                                        .filter(|idx| {
                                            req.certificates.iter().any(|c| c.index == *idx)
                                        })
                                        .or_else(|| req.certificates.first().map(|c| c.index));
                                    CertificateDecision::Accept {
                                        certificate_index: chosen,
                                    }
                                } else {
                                    CertificateDecision::Cancel
                                }
                            }
                        };
                        let _ = rt.resolve_certificate(req_id, decision);
                    }
                }
                ExampleAction::ResolveBeforeUnload(allow) => {
                    if let Ok(Some(modal)) = root.update(cx, |r, _, cx| {
                        let m = r.model.pop_front_before_unload();
                        if !allow {
                            if let Some(ref info) = m
                                && let Some(t) = r.model.tab_mut(info.tab_id)
                            {
                                t.closing_pending = false;
                            }
                            r.model.window_close_pending = false;
                        }
                        r.sync_native_tab_visibility(cx);
                        cx.notify();
                        m
                    }) {
                        if !allow {
                            if let Some(t) = tabs.get_mut(&modal.tab_id) {
                                t.closing_pending = false;
                            }
                            window_close_pending = false;
                            close.set(false);
                        }
                        let _ = root.update(cx, |r, _, cx| {
                            if let Some(v) = r.views.get(&modal.tab_id).cloned() {
                                v.resolve_before_unload(modal.request, allow, cx);
                            }
                        });
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
                let _ = finalize_close_example_tab(
                    cx,
                    &rt_cell,
                    menus.as_ref(),
                    &mut ux,
                    &mut file_dialogs,
                    &mut pending_save_as,
                    root,
                    tab_id,
                    &mut accounting,
                    &mut tabs,
                    false,
                );
                continue;
            }
            let view = root
                .update(cx, |r, _, _| r.views.get(&tab_id).cloned())
                .ok()
                .flatten();
            let diag = if let Some(view) = view {
                cx.update_window(root.into(), |_, window, cx| {
                    view.consume_and_sample(window, cx)
                })
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
                        let _ = menus.borrow_mut().dispatch(
                            tab.browser_id,
                            window,
                            cx,
                            |upd, w, cx| pres.borrow_mut().update(&upd, w, cx),
                        );
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

        drain_find_updates(cx, root, &tabs, &rt_cell, &mut ux);
        let mut source_failures = Vec::new();
        if let Some(sources) = ux.sources.as_mut() {
            cx.update(|app| sources.dispatch(app));
            let active_browsers: Vec<BrowserId> = tabs.values().map(|t| t.browser_id).collect();
            for browser_id in active_browsers {
                if let Ok(ops) = sources.operations(browser_id) {
                    for op in ops {
                        if op.phase == rust_cef_core::source_view::SourcePhase::Finished
                            && op.first_failure.is_some()
                        {
                            source_failures.push((browser_id, op));
                        }
                    }
                }
            }
        }
        for (browser_id, op) in source_failures {
            report_source_failure(cx, root, &tabs, &mut ux, browser_id, &op);
        }
        let direct_source_failures = cx.update(|app| ux.direct_sources.drain_and_apply(app));
        for (browser_id, status) in direct_source_failures {
            report_source_failure(cx, root, &tabs, &mut ux, browser_id, &status);
        }
        ux.sweep_retired(cx);
        if window_close_pending && tabs.is_empty() {
            break;
        }

        // Automated `--smoke-ms` self-check sequence.
        if config.smoke.is_some() {
            if smoke_stage == 0
                && tabs
                    .get(&1)
                    .is_some_and(|t| t.first_paint && !t.handle.snapshot().navigation.is_loading)
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
                    &mut ux,
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
                    &mut ux,
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
                &mut ux,
                &mut file_dialogs,
                &mut pending_save_as,
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
    ux.shutdown(cx);
    let remaining: Vec<u64> = tabs.keys().copied().collect();
    for id in remaining {
        let _ = finalize_close_example_tab(
            cx,
            &rt_cell,
            menus.as_ref(),
            &mut ux,
            &mut file_dialogs,
            &mut pending_save_as,
            root,
            id,
            &mut accounting,
            &mut tabs,
            true,
        );
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
        let _ = wait(
            cx,
            &receiver,
            Some(deadline.saturating_duration_since(Instant::now()).min(step)),
        )
        .await;
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
                    let focus_out_sub = cx.on_focus_out(
                        &input_focus,
                        window,
                        |this: &mut CefExampleRoot, _, _, cx| {
                            this.blur_active_input(cx);
                        },
                    );
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
                        selected_cert_index: None,
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

#[cfg(test)]
mod before_unload_tests {
    use super::assets::DESKTOP_UX_FIXTURES_URL;
    use super::*;
    use rust_cef_gpui::{
        contracts::RuntimeGeneration,
        host_capabilities::{
            AuthRequest, CertificateErrorRequest, DownloadId, DownloadRequest, HostRequestId,
            PermissionKind, PermissionRequest,
        },
    };

    fn test_browser(local: u64) -> BrowserId {
        BrowserId::new(RuntimeGeneration::new(1), local)
    }

    fn test_req_id(browser: BrowserId, seq: u64) -> HostRequestId {
        HostRequestId {
            browser,
            sequence: seq,
        }
    }

    #[test]
    fn before_unload_stay_clears_closing_pending_and_aborts_window_close() {
        let b1 = test_browser(1);
        let mut model = ExampleAppModel::default();
        assert!(model.add_tab(1, DEFAULT_HOME_URL.into(), false));
        model.tab_mut(1).unwrap().closing_pending = true;
        model.window_close_pending = true;
        model.enqueue_before_unload(PendingBeforeUnloadModal {
            tab_id: 1,
            browser_id: b1,
            request: BeforeUnloadRequestId {
                browser: b1,
                sequence: 10,
            },
            is_reload: false,
        });
        assert!(model.has_blocking_modal());

        // Simulate user choosing "Stay" (allow = false) on two-phase close.
        let popped = model.pop_front_before_unload().expect("modal present");
        assert_eq!(popped.tab_id, 1);
        model.tab_mut(popped.tab_id).unwrap().closing_pending = false;
        model.window_close_pending = false;

        assert_eq!(model.tabs.len(), 1);
        assert!(!model.tabs[0].closing_pending);
        assert!(!model.window_close_pending);
        assert!(!model.quit_requested);
        assert!(!model.has_blocking_modal());
    }

    #[test]
    fn before_unload_leave_finalizes_tab_cancels_downloads_and_prunes_modals() {
        let b1 = test_browser(1);
        let b2 = test_browser(2);
        let mut model = ExampleAppModel::default();
        assert!(model.add_tab(1, DEFAULT_HOME_URL.into(), false));
        assert!(model.add_tab(2, DESKTOP_UX_FIXTURES_URL.into(), false));
        let dl_id = DownloadId {
            browser: b1,
            cef_id: 1,
            sequence: 7,
        };
        model.tab_mut(1).unwrap().apply_download_request(
            b1,
            &DownloadRequest {
                id: dl_id,
                url: "https://example.test/a.bin".into(),
                original_url: "https://example.test/a.bin".into(),
                suggested_name: "a.bin".into(),
                mime_type: "application/octet-stream".into(),
                total_bytes: Some(128),
            },
        );
        model.active_native_file_dialog = Some(test_req_id(b1, 1));
        model.active_save_as_download = Some(dl_id);
        model.enqueue_permission(PendingPermissionModal {
            tab_id: 1,
            browser_id: b1,
            request: PermissionRequest {
                id: test_req_id(b1, 2),
                kind: PermissionKind::MediaAccess,
                requesting_origin: "https://a.test".into(),
                requested_permissions: 1,
            },
        });
        model.enqueue_auth(model::PendingAuthModal {
            tab_id: 1,
            browser_id: b1,
            request: AuthRequest {
                id: test_req_id(b1, 3),
                origin_url: "https://a.test".into(),
                is_proxy: false,
                host: "a.test".into(),
                port: 443,
                realm: "Test".into(),
                scheme: "basic".into(),
            },
            username_input: TextBuffer::new("alice"),
            password_input: TextBuffer::new("secret"),
        });
        model.enqueue_cert(PendingCertModal {
            tab_id: 2,
            browser_id: b2,
            kind: PendingCertKind::ServerError(CertificateErrorRequest {
                id: test_req_id(b2, 4),
                cert_error: -200,
                request_url: "https://b.test".into(),
            }),
        });
        model.active_input = ActiveInput::AuthPass;

        let cancelled =
            cancel_active_download_on_close(&mut model.tab_mut(1).unwrap().download, b1);
        assert_eq!(cancelled, Some(dl_id));
        assert!(model.tab_mut(1).unwrap().download.is_cancelled);

        model.clear_modals_for_tab(1, Some(b1));
        assert_eq!(model.active_native_file_dialog, None);
        assert_eq!(model.active_save_as_download, None);
        assert!(model.pending_permission.is_empty());
        assert!(model.pending_auth.is_empty());
        assert_eq!(model.active_input, ActiveInput::None);
        assert_eq!(model.pending_cert.len(), 1);

        assert!(model.close_tab(1));
        assert_eq!(model.tabs.len(), 1);
        assert!(!model.quit_requested);

        assert!(model.close_tab(2));
        assert!(model.quit_requested);
    }
}
