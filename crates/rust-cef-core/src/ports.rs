//! Explicit adapter boundaries. No CEF, GPUI, Win32, or service locator types.
use std::{
    sync::{
        Arc, Weak,
        atomic::{AtomicU8, Ordering},
    },
    time::Instant,
};

use crate::contracts::{
    BrowserConfig, BrowserCursor, BrowserId, BrowserSnapshot, CefFeatureState, ConsoleSeverity,
    DraggableRegionRect, FocusedDomNodeInfo, FrameLifecycleInfo, InitializeOutcome,
    JsUncaughtExceptionInfo, PixelRect, ResourceLoadCompleteInfo, ResourceResponseInfo,
    RuntimeConfig, RuntimeGeneration, ShortcutId, Viewport,
};
use crate::{
    custom_schemes::ResourceInterceptor,
    frame_budget::BudgetedPixels,
    host_capabilities::{
        DragEnterFilter, HostCapabilities, KeyEventFilter, NavigationFilter, PopupFilter,
        PopupPolicy, PreKeyFilter, ProtocolExecutionFilter, SetFocusFilter,
    },
    ime::ImeSnapshot,
    inbound_drag::InboundDragCommand,
    input::{BrowserTouchEvent, InputEvent, InputResetReason, InputSnapshot},
    js_bridge::{EvalOutcome, EvalRequestId, JsFrameScope},
    mailbox::FrameUpdate,
    outbound_drag::{DragOperationMask, OsrDragFinish, OutboundDragFinish, OutboundDragStart},
};

pub const EXT_EVENT_TOOLTIP: u32 = 1 << 0;
pub const EXT_EVENT_CONSOLE: u32 = 1 << 1;
pub const EXT_EVENT_AUTO_RESIZE: u32 = 1 << 2;
pub const EXT_EVENT_MEDIA_ACCESS: u32 = 1 << 3;
pub const EXT_EVENT_DOCUMENT_AVAILABLE: u32 = 1 << 4;
pub const EXT_EVENT_DRAGGABLE_REGIONS: u32 = 1 << 5;
pub const EXT_EVENT_FRAME_LIFECYCLE: u32 = 1 << 6;
pub const EXT_EVENT_V8_CONTEXT: u32 = 1 << 7;
pub const EXT_EVENT_FOCUSED_NODE: u32 = 1 << 8;
pub const EXT_EVENT_UNCAUGHT_EXCEPTION: u32 = 1 << 9;
pub const EXT_EVENT_RESOURCE_OBSERVE: u32 = 1 << 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseMode {
    Graceful,
    Force,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandId {
    pub browser: BrowserId,
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestId {
    pub browser: BrowserId,
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskId {
    pub runtime: RuntimeGeneration,
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseModifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub platform: bool,
}

/// One complete left-button click in DIP coordinates. `click_count` is the
/// platform click count (1 = single click, 2 = double click); it is never zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseClick {
    pub x: i32,
    pub y: i32,
    pub modifiers: MouseModifiers,
    pub click_count: std::num::NonZeroU8,
}

#[derive(Clone, PartialEq)]
pub enum BrowserCommand {
    Navigate(String),
    LoadUrlWithPostData {
        url: String,
        post_data: Vec<u8>,
        content_type: Option<String>,
    },
    Back,
    Forward,
    Reload,
    StopLoading,
    SetFocus(bool),
    SetVisible(bool),
    SetViewport(Viewport),
    SetZoomLevel(f64),
    SetAudioMuted(bool),
    /// Atomic left-button gesture: focus, move, down, up in one admitted
    /// command, so a bounded queue cannot strand half a click.
    MouseClick(MouseClick),
    Input(InputEvent),
    ResetInput(InputResetReason),
    ResolveBeforeUnload {
        request: RequestId,
        allow: bool,
    },
    ExecuteJavaScript {
        script: String,
        frame_scope: JsFrameScope,
    },
    EvaluateScript {
        id: EvalRequestId,
        script: String,
        frame_scope: JsFrameScope,
        timeout_ms: u64,
    },
    PostJsMessage {
        payload_json: String,
    },
    FinishOutboundDrag(OutboundDragFinish),
    FinishOsrDrag(OsrDragFinish),
    SetAccessibilityState(CefFeatureState),
    TouchEvent(BrowserTouchEvent),
    SetNativeBounds(PixelRect),
    NotifyParentMoved,
    SetScreenOrigin {
        x_px: i32,
        y_px: i32,
    },
    SetClientOffset {
        x_dip: f32,
        y_dip: f32,
    },
    DragTarget(InboundDragCommand),
}

impl std::fmt::Debug for BrowserCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Navigate(url) => f.debug_tuple("Navigate").field(url).finish(),
            Self::LoadUrlWithPostData {
                url,
                post_data,
                content_type,
            } => f
                .debug_struct("LoadUrlWithPostData")
                .field("url", url)
                .field("post_data_bytes", &post_data.len())
                .field("content_type", content_type)
                .finish(),
            Self::Back => f.write_str("Back"),
            Self::Forward => f.write_str("Forward"),
            Self::Reload => f.write_str("Reload"),
            Self::StopLoading => f.write_str("StopLoading"),
            Self::SetFocus(v) => f.debug_tuple("SetFocus").field(v).finish(),
            Self::SetVisible(v) => f.debug_tuple("SetVisible").field(v).finish(),
            Self::SetViewport(v) => f.debug_tuple("SetViewport").field(v).finish(),
            Self::SetZoomLevel(v) => f.debug_tuple("SetZoomLevel").field(v).finish(),
            Self::SetAudioMuted(v) => f.debug_tuple("SetAudioMuted").field(v).finish(),
            Self::MouseClick(v) => f.debug_tuple("MouseClick").field(v).finish(),
            Self::Input(v) => f.debug_tuple("Input").field(v).finish(),
            Self::ResetInput(v) => f.debug_tuple("ResetInput").field(v).finish(),
            Self::ResolveBeforeUnload { request, allow } => f
                .debug_struct("ResolveBeforeUnload")
                .field("request", request)
                .field("allow", allow)
                .finish(),
            Self::ExecuteJavaScript {
                script,
                frame_scope,
            } => f
                .debug_struct("ExecuteJavaScript")
                .field("script_bytes", &script.len())
                .field("frame_scope", frame_scope)
                .finish(),
            Self::EvaluateScript {
                id,
                script,
                frame_scope,
                timeout_ms,
            } => f
                .debug_struct("EvaluateScript")
                .field("id", id)
                .field("script_bytes", &script.len())
                .field("frame_scope", frame_scope)
                .field("timeout_ms", timeout_ms)
                .finish(),
            Self::PostJsMessage { payload_json } => f
                .debug_struct("PostJsMessage")
                .field("payload_bytes", &payload_json.len())
                .finish(),
            Self::FinishOutboundDrag(finish) => {
                f.debug_tuple("FinishOutboundDrag").field(finish).finish()
            }
            Self::FinishOsrDrag(finish) => f.debug_tuple("FinishOsrDrag").field(finish).finish(),
            Self::SetAccessibilityState(state) => {
                f.debug_tuple("SetAccessibilityState").field(state).finish()
            }
            Self::TouchEvent(touch) => f.debug_tuple("TouchEvent").field(touch).finish(),
            Self::SetNativeBounds(rect) => f.debug_tuple("SetNativeBounds").field(rect).finish(),
            Self::NotifyParentMoved => f.write_str("NotifyParentMoved"),
            Self::SetScreenOrigin { x_px, y_px } => f
                .debug_struct("SetScreenOrigin")
                .field("x_px", x_px)
                .field("y_px", y_px)
                .finish(),
            Self::SetClientOffset { x_dip, y_dip } => f
                .debug_struct("SetClientOffset")
                .field("x_dip", x_dip)
                .field("y_dip", y_dip)
                .finish(),
            Self::DragTarget(cmd) => f.debug_tuple("DragTarget").field(cmd).finish(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CommandEnvelope {
    pub id: CommandId,
    pub command: BrowserCommand,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OwnedEngineTask {
    Create {
        browser: BrowserId,
        config: BrowserConfig,
    },
    Command(CommandEnvelope),
    Close {
        browser: BrowserId,
        mode: CloseMode,
    },
}

/// Capture-stage failure category; raw codes remain adapter/platform errors,
/// never Browser, renderer-process or command identities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureFailureKind {
    InvalidDimensions,
    UnsupportedFormat,
    IncompleteFrame,
    Allocation,
    InvalidHandle,
    TextureMismatch,
    DeviceCreation,
    OpenSharedResource,
    StagingCreation,
    CopyResource,
    Map,
    DeviceRemoved,
    WrongBackend,
    WrongThread,
    Retired,
    Busy,
    SourceRejected,
    ResizeExhausted,
}

impl CaptureFailureKind {
    /// True for intermediate compositor/resize transitions that the runtime
    /// recovers from internally without corrupting or dropping the active session.
    #[must_use]
    pub const fn is_recoverable_resize_transition(self) -> bool {
        matches!(self, Self::IncompleteFrame | Self::ResizeExhausted | Self::Busy)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineError {
    WrongThread,
    WrongRuntime,
    NotReady,
    Closing,
    Closed,
    Backpressure,
    PostRejected,
    PolicyDenied,
    InvalidRequest,
    /// CEF 146 uses IME document replacement ranges only on OS X.
    /// Windows must refuse rather than silently insert at the current caret.
    UnsupportedInputReplacement,
    Backend {
        code: Option<i32>,
    },
    Capture {
        kind: CaptureFailureKind,
        code: Option<i32>,
    },
}

impl EngineError {
    /// True when the error represents an expected, kernel-handled intermediate
    /// resize/compositor frame rejection rather than a broken capture pipeline.
    #[must_use]
    pub const fn is_recoverable_capture(&self) -> bool {
        matches!(
            self,
            Self::Capture {
                kind: CaptureFailureKind::IncompleteFrame | CaptureFailureKind::ResizeExhausted,
                code: None,
            } | Self::Capture {
                kind: CaptureFailureKind::Busy,
                ..
            }
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishResult {
    Published,
    Coalesced,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PolicyAction {
    Navigation,
    Resource,
    Redirect,
    ExternalProtocol,
    Popup,
    Download,
    Permission,
    Certificate,
    ClientCertificate,
    Authentication,
    FileDialog,
    JsDialog,
}

#[derive(Clone, PartialEq, Eq)]
pub enum BrowserEvent {
    Created,
    CreateFailed(EngineError),
    AddressChanged(String),
    TitleChanged(String),
    CursorChanged(BrowserCursor),
    TooltipChanged(String),
    StatusMessageChanged(String),
    FaviconUrlsChanged(Vec<String>),
    FullscreenModeChanged(bool),
    LoadingProgressChanged(u16),
    ConsoleMessage {
        severity: ConsoleSeverity,
        message: String,
        line: i32,
    },
    ZoomLevelChanged(i32),
    AudioMutedChanged(bool),
    FocusTaken {
        next: bool,
    },
    GotFocus,
    HostShortcutTriggered {
        id: ShortcutId,
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
    LoadingChanged {
        loading: bool,
        can_go_back: bool,
        can_go_forward: bool,
    },
    LoadFailed {
        is_main_frame: bool,
        code: i32,
    },
    PolicyDenied {
        action: PolicyAction,
    },
    BeforeUnloadRequested {
        request: RequestId,
        is_reload: bool,
    },
    JsMessageReceived {
        origin: String,
        payload_json: String,
    },
    EvalCompleted {
        id: EvalRequestId,
        outcome: EvalOutcome,
    },
    /// IPC readiness only, not page load or frame completion.
    RenderViewReady,
    RendererTerminated {
        native_status: i32,
        code: Option<i32>,
    },
    Unresponsive,
    Responsive,
    CaptureFailed(EngineError),
    StartOutboundDrag(OutboundDragStart),
    OutboundDragCursorChanged(DragOperationMask),
    AccessibilityTreeUpdated {
        generation: u64,
        node_count: usize,
        truncated: bool,
    },
    PopupRequested(crate::host_capabilities::PopupRequest),
    DraggableRegionsChanged(Vec<DraggableRegionRect>),
    AutoResized {
        width: i32,
        height: i32,
    },
    MediaAccessChanged {
        has_video: bool,
        has_audio: bool,
    },
    DocumentAvailableInMainFrame,
    FrameCreated(FrameLifecycleInfo),
    FrameAttached {
        frame: FrameLifecycleInfo,
        reattached: bool,
    },
    FrameDetached {
        frame_id: String,
        is_main: bool,
    },
    MainFrameChanged {
        old_frame_id: Option<String>,
        new_frame: Option<FrameLifecycleInfo>,
    },
    ContextCreated {
        frame_id: String,
        is_main_frame: bool,
    },
    ContextReleased {
        frame_id: String,
        is_main_frame: bool,
    },
    FocusedNodeChanged(Option<FocusedDomNodeInfo>),
    UncaughtJsException(JsUncaughtExceptionInfo),
    ResourceResponseObserved(ResourceResponseInfo),
    ResourceLoadCompleted(ResourceLoadCompleteInfo),
    CloseCancelled,
    Closed,
}

impl std::fmt::Debug for BrowserEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created => f.write_str("Created"),
            Self::CreateFailed(err) => f.debug_tuple("CreateFailed").field(err).finish(),
            Self::AddressChanged(v) => f.debug_tuple("AddressChanged").field(v).finish(),
            Self::TitleChanged(v) => f.debug_tuple("TitleChanged").field(v).finish(),
            Self::CursorChanged(v) => f.debug_tuple("CursorChanged").field(v).finish(),
            Self::TooltipChanged(v) => f
                .debug_struct("TooltipChanged")
                .field("bytes", &v.len())
                .finish(),
            Self::StatusMessageChanged(v) => f
                .debug_struct("StatusMessageChanged")
                .field("bytes", &v.len())
                .finish(),
            Self::FaviconUrlsChanged(v) => f.debug_tuple("FaviconUrlsChanged").field(v).finish(),
            Self::FullscreenModeChanged(v) => {
                f.debug_tuple("FullscreenModeChanged").field(v).finish()
            }
            Self::LoadingProgressChanged(v) => {
                f.debug_tuple("LoadingProgressChanged").field(v).finish()
            }
            Self::ConsoleMessage {
                severity,
                message,
                line,
            } => f
                .debug_struct("ConsoleMessage")
                .field("severity", severity)
                .field("message_bytes", &message.len())
                .field("line", line)
                .finish(),
            Self::ZoomLevelChanged(v) => f.debug_tuple("ZoomLevelChanged").field(v).finish(),
            Self::AudioMutedChanged(v) => f.debug_tuple("AudioMutedChanged").field(v).finish(),
            Self::FocusTaken { next } => f.debug_struct("FocusTaken").field("next", next).finish(),
            Self::GotFocus => f.write_str("GotFocus"),
            Self::HostShortcutTriggered { id } => f
                .debug_struct("HostShortcutTriggered")
                .field("id", id)
                .finish(),
            Self::NavigationRequested {
                is_main_frame,
                is_redirect,
            } => f
                .debug_struct("NavigationRequested")
                .field("is_main_frame", is_main_frame)
                .field("is_redirect", is_redirect)
                .finish(),
            Self::LoadStarted { is_main_frame } => f
                .debug_struct("LoadStarted")
                .field("is_main_frame", is_main_frame)
                .finish(),
            Self::LoadFinished {
                is_main_frame,
                http_status_code,
            } => f
                .debug_struct("LoadFinished")
                .field("is_main_frame", is_main_frame)
                .field("http_status_code", http_status_code)
                .finish(),
            Self::LoadCancelled {
                is_main_frame,
                code,
            } => f
                .debug_struct("LoadCancelled")
                .field("is_main_frame", is_main_frame)
                .field("code", code)
                .finish(),
            Self::LoadingChanged {
                loading,
                can_go_back,
                can_go_forward,
            } => f
                .debug_struct("LoadingChanged")
                .field("loading", loading)
                .field("can_go_back", can_go_back)
                .field("can_go_forward", can_go_forward)
                .finish(),
            Self::LoadFailed {
                is_main_frame,
                code,
            } => f
                .debug_struct("LoadFailed")
                .field("is_main_frame", is_main_frame)
                .field("code", code)
                .finish(),
            Self::PolicyDenied { action } => f
                .debug_struct("PolicyDenied")
                .field("action", action)
                .finish(),
            Self::BeforeUnloadRequested { request, is_reload } => f
                .debug_struct("BeforeUnloadRequested")
                .field("request", request)
                .field("is_reload", is_reload)
                .finish(),
            Self::JsMessageReceived {
                origin,
                payload_json,
            } => f
                .debug_struct("JsMessageReceived")
                .field("origin", origin)
                .field("payload_bytes", &payload_json.len())
                .finish(),
            Self::EvalCompleted { id, outcome } => f
                .debug_struct("EvalCompleted")
                .field("id", id)
                .field("outcome", outcome)
                .finish(),
            Self::RenderViewReady => f.write_str("RenderViewReady"),
            Self::RendererTerminated {
                native_status,
                code,
            } => f
                .debug_struct("RendererTerminated")
                .field("native_status", native_status)
                .field("code", code)
                .finish(),
            Self::Unresponsive => f.write_str("Unresponsive"),
            Self::Responsive => f.write_str("Responsive"),
            Self::CaptureFailed(err) => f.debug_tuple("CaptureFailed").field(err).finish(),
            Self::StartOutboundDrag(start) => {
                f.debug_tuple("StartOutboundDrag").field(start).finish()
            }
            Self::OutboundDragCursorChanged(op) => f
                .debug_tuple("OutboundDragCursorChanged")
                .field(op)
                .finish(),
            Self::AccessibilityTreeUpdated {
                generation,
                node_count,
                truncated,
            } => f
                .debug_struct("AccessibilityTreeUpdated")
                .field("generation", generation)
                .field("node_count", node_count)
                .field("truncated", truncated)
                .finish(),
            Self::PopupRequested(req) => f.debug_tuple("PopupRequested").field(req).finish(),
            Self::DraggableRegionsChanged(regions) => f
                .debug_struct("DraggableRegionsChanged")
                .field("count", &regions.len())
                .finish(),
            Self::AutoResized { width, height } => f
                .debug_struct("AutoResized")
                .field("width", width)
                .field("height", height)
                .finish(),
            Self::MediaAccessChanged {
                has_video,
                has_audio,
            } => f
                .debug_struct("MediaAccessChanged")
                .field("has_video", has_video)
                .field("has_audio", has_audio)
                .finish(),
            Self::DocumentAvailableInMainFrame => f.write_str("DocumentAvailableInMainFrame"),
            Self::FrameCreated(info) => f.debug_tuple("FrameCreated").field(info).finish(),
            Self::FrameAttached { frame, reattached } => f
                .debug_struct("FrameAttached")
                .field("frame", frame)
                .field("reattached", reattached)
                .finish(),
            Self::FrameDetached { frame_id, is_main } => f
                .debug_struct("FrameDetached")
                .field("frame_id", frame_id)
                .field("is_main", is_main)
                .finish(),
            Self::MainFrameChanged {
                old_frame_id,
                new_frame,
            } => f
                .debug_struct("MainFrameChanged")
                .field("old_frame_id", old_frame_id)
                .field("new_frame", new_frame)
                .finish(),
            Self::ContextCreated {
                frame_id,
                is_main_frame,
            } => f
                .debug_struct("ContextCreated")
                .field("frame_id", frame_id)
                .field("is_main_frame", is_main_frame)
                .finish(),
            Self::ContextReleased {
                frame_id,
                is_main_frame,
            } => f
                .debug_struct("ContextReleased")
                .field("frame_id", frame_id)
                .field("is_main_frame", is_main_frame)
                .finish(),
            Self::FocusedNodeChanged(node) => {
                f.debug_tuple("FocusedNodeChanged").field(node).finish()
            }
            Self::UncaughtJsException(info) => {
                f.debug_tuple("UncaughtJsException").field(info).finish()
            }
            Self::ResourceResponseObserved(info) => f
                .debug_tuple("ResourceResponseObserved")
                .field(info)
                .finish(),
            Self::ResourceLoadCompleted(info) => {
                f.debug_tuple("ResourceLoadCompleted").field(info).finish()
            }
            Self::CloseCancelled => f.write_str("CloseCancelled"),
            Self::Closed => f.write_str("Closed"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserEventEnvelope {
    pub browser: BrowserId,
    pub sequence: u64,
    pub event: BrowserEvent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceKind {
    View,
    Popup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    PremultipliedBgra8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameInfo {
    pub browser: BrowserId,
    pub surface: SurfaceKind,
    pub surface_generation: u64,
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
}

pub const SHARED_TEXTURE_SLOT_COUNT: usize = 4;

const NO_SHARED_SLOT: u8 = u8::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextureFormat {
    Bgra8888,
}

impl TextureFormat {
    #[must_use]
    pub const fn dxgi_format_raw(self) -> u32 {
        match self {
            Self::Bgra8888 => 87, // DXGI_FORMAT_B8G8R8A8_UNORM
        }
    }
}

/// Lock-free quad-buffered slot state coordinator shared between the CEF render
/// handler pool and UI consumers (`FrameMailbox` -> `SurfacePipeline` -> `CefBrowser`).
#[derive(Debug)]
pub struct SharedTextureSlotTracker {
    latest_ready: std::sync::atomic::AtomicU8,
    presenting: std::sync::atomic::AtomicU8,
    next_write_cursor: std::sync::atomic::AtomicU8,
    consumer_refs: [std::sync::atomic::AtomicU32; SHARED_TEXTURE_SLOT_COUNT],
}

impl Default for SharedTextureSlotTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedTextureSlotTracker {
    #[must_use]
    pub const fn new() -> Self {
        use std::sync::atomic::{AtomicU8, AtomicU32};
        Self {
            latest_ready: AtomicU8::new(NO_SHARED_SLOT),
            presenting: AtomicU8::new(NO_SHARED_SLOT),
            next_write_cursor: AtomicU8::new(0),
            consumer_refs: [
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
            ],
        }
    }

    /// Selects a free slot index in `0..SHARED_TEXTURE_SLOT_COUNT` that is neither
    /// `latest_ready`, `presenting`, nor held by any active consumer lease.
    pub fn acquire_write_slot(&self) -> Option<usize> {
        use std::sync::atomic::Ordering;

        let latest = self.latest_ready.load(Ordering::Acquire);
        let presenting = self.presenting.load(Ordering::Acquire);
        let start =
            usize::from(self.next_write_cursor.load(Ordering::Relaxed)) % SHARED_TEXTURE_SLOT_COUNT;

        for offset in 0..SHARED_TEXTURE_SLOT_COUNT {
            let slot = (start + offset) % SHARED_TEXTURE_SLOT_COUNT;
            let slot_u8 = slot as u8;
            if slot_u8 == latest || slot_u8 == presenting {
                continue;
            }
            if self.consumer_refs[slot].load(Ordering::Acquire) != 0 {
                continue;
            }
            self.next_write_cursor
                .store(((slot + 1) % SHARED_TEXTURE_SLOT_COUNT) as u8, Ordering::Release);
            return Some(slot);
        }
        None
    }

    pub fn publish_ready(&self, slot_index: usize) {
        if slot_index < SHARED_TEXTURE_SLOT_COUNT {
            self.latest_ready
                .store(slot_index as u8, std::sync::atomic::Ordering::Release);
        }
    }

    pub fn clear_latest_ready_if(&self, slot_index: usize) {
        if slot_index < SHARED_TEXTURE_SLOT_COUNT {
            let _ = self.latest_ready.compare_exchange(
                slot_index as u8,
                NO_SHARED_SLOT,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            );
        }
    }

    pub fn clear_latest_ready(&self) {
        self.latest_ready
            .store(NO_SHARED_SLOT, std::sync::atomic::Ordering::Release);
    }

    pub fn mark_presenting(&self, slot_index: usize) {
        if slot_index < SHARED_TEXTURE_SLOT_COUNT {
            let slot_u8 = slot_index as u8;
            self.presenting
                .store(slot_u8, std::sync::atomic::Ordering::Release);
            let _ = self.latest_ready.compare_exchange(
                slot_u8,
                NO_SHARED_SLOT,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            );
        }
    }

    pub fn clear_presenting(&self) {
        self.presenting
            .store(NO_SHARED_SLOT, std::sync::atomic::Ordering::Release);
    }

    pub fn clear_presenting_if(&self, slot_index: usize) {
        if slot_index < SHARED_TEXTURE_SLOT_COUNT {
            let _ = self.presenting.compare_exchange(
                slot_index as u8,
                NO_SHARED_SLOT,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            );
        }
    }

    #[must_use]
    pub fn latest_ready_slot(&self) -> Option<usize> {
        let raw = self.latest_ready.load(std::sync::atomic::Ordering::Acquire);
        (raw != NO_SHARED_SLOT).then_some(usize::from(raw))
    }

    #[must_use]
    pub fn presenting_slot(&self) -> Option<usize> {
        let raw = self.presenting.load(std::sync::atomic::Ordering::Acquire);
        (raw != NO_SHARED_SLOT).then_some(usize::from(raw))
    }

    #[must_use]
    pub fn consumer_ref_count(&self, slot_index: usize) -> u32 {
        self.consumer_refs
            .get(slot_index)
            .map_or(0, |count| count.load(std::sync::atomic::Ordering::Acquire))
    }

    #[must_use]
    pub fn lease(self: &Arc<Self>, slot_index: usize) -> SharedTextureLease {
        assert!(
            slot_index < SHARED_TEXTURE_SLOT_COUNT,
            "shared texture slot index {slot_index} out of bounds"
        );
        self.consumer_refs[slot_index].fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        SharedTextureLease {
            inner: Arc::new(SharedTextureLeaseInner {
                tracker: Arc::clone(self),
                slot_index,
            }),
        }
    }
}

#[derive(Debug)]
struct SharedTextureLeaseInner {
    tracker: Arc<SharedTextureSlotTracker>,
    slot_index: usize,
}

impl Drop for SharedTextureLeaseInner {
    fn drop(&mut self) {
        // Clear stale ready/presenting flags BEFORE dropping consumer_refs so a producer
        // cannot re-acquire and re-publish this slot before old flags are cleared.
        self.tracker.clear_latest_ready_if(self.slot_index);
        self.tracker.clear_presenting_if(self.slot_index);
        self.tracker.consumer_refs[self.slot_index]
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

#[derive(Clone, Debug)]
pub struct SharedTextureLease {
    inner: Arc<SharedTextureLeaseInner>,
}

impl SharedTextureLease {
    #[must_use]
    pub fn tracker(&self) -> &Arc<SharedTextureSlotTracker> {
        &self.inner.tracker
    }

    #[must_use]
    pub fn slot_index(&self) -> usize {
        self.inner.slot_index
    }
}

#[derive(Clone, Debug)]
pub struct GpuSharedTextureFrame {
    pub pool_id: u64,
    pub shared_handle: isize,
    pub is_nt_handle: bool,
    pub keyed_mutex_key: Option<u64>,
    pub width: u32,
    pub height: u32,
    pub format: TextureFormat,
    pub frame_seq: u64,
    pub slot_index: usize,
    pub generation: u64,
    pub resource_keepalive: std::sync::Weak<()>,
    pub handle_owner: std::sync::Arc<dyn std::any::Any + Send + Sync>,
    pub lease: SharedTextureLease,
}

#[derive(Clone, Debug)]
pub enum FramePayload {
    CpuBgra {
        buffer: Arc<BudgetedPixels>,
        width: u32,
        height: u32,
        format: PixelFormat,
    },
    GpuSharedTexture(GpuSharedTextureFrame),
}

#[derive(Clone, Debug)]
pub struct OwnedFrame {
    pub info: FrameInfo,
    /// Frame image payload: either a budgeted CPU BGRA pixel buffer or a handle
    /// to a slot in the producer's triple-buffered D3D11 shared texture pool.
    pub payload: FramePayload,
}

impl OwnedFrame {
    #[must_use]
    pub fn from_cpu_pixels(info: FrameInfo, buffer: Arc<BudgetedPixels>) -> Self {
        Self {
            info,
            payload: FramePayload::CpuBgra {
                width: info.width,
                height: info.height,
                format: info.format,
                buffer,
            },
        }
    }

    #[must_use]
    pub fn cpu_pixels(&self) -> Option<&Arc<BudgetedPixels>> {
        match &self.payload {
            FramePayload::CpuBgra { buffer, .. } => Some(buffer),
            FramePayload::GpuSharedTexture(_) => None,
        }
    }

    #[must_use]
    pub fn gpu_texture(&self) -> Option<&GpuSharedTextureFrame> {
        match &self.payload {
            FramePayload::CpuBgra { .. } => None,
            FramePayload::GpuSharedTexture(frame) => Some(frame),
        }
    }

    #[must_use]
    pub fn is_gpu_shared(&self) -> bool {
        matches!(self.payload, FramePayload::GpuSharedTexture(_))
    }
}

pub trait RuntimeDriver {
    fn initialize(&mut self, config: &RuntimeConfig) -> Result<InitializeOutcome, EngineError>;
    fn pump(&mut self) -> Result<(), EngineError>;
    fn shutdown(&mut self) -> Result<(), EngineError>;
}

pub trait BrowserEngine {
    fn execute(&mut self, command: CommandEnvelope) -> Result<(), EngineError>;
}

pub trait CefUiExecutor: Send + Sync {
    fn post(&self, task: OwnedEngineTask) -> Result<TaskId, EngineError>;
}

pub trait BrowserEventSink: Send + Sync {
    fn publish(&self, event: BrowserEventEnvelope) -> PublishResult;
}

/// Borrowed only for capture's scope; implementations never return native pointers.
pub trait FrameInput {
    fn copy_bgra(&self, destination: &mut [u8]) -> Result<(), EngineError>;
}

/// Concrete capture implementations must be executed on their owning CEF UI sequence.
pub trait FrameCapture {
    fn capture(
        &mut self,
        info: FrameInfo,
        input: &dyn FrameInput,
    ) -> Result<OwnedFrame, EngineError>;
}

pub trait FrameSink: Send + Sync {
    fn publish(&self, frame: OwnedFrame) -> PublishResult;
}

pub trait FrameSource: Send + Sync {
    /// Pure query; it neither acknowledges an update nor disarms a wake.
    fn latest(&self, surface: SurfaceKind) -> Option<Arc<OwnedFrame>>;
    /// Clones both surface snapshots and atomically clears pending/armed state.
    fn take_update(&self) -> Option<FrameUpdate>;
    /// Current state without consuming pending state, for resynchronization.
    fn snapshot(&self) -> FrameUpdate;
    /// Pure query whether an unconsumed update is currently queued (`dirty`).
    fn has_pending_update(&self) -> bool {
        false
    }
}

pub trait UiDispatcher: Send + Sync {
    fn wake(&self) -> Result<(), EngineError>;
}

pub trait PumpScheduler {
    fn schedule(&mut self, deadline: Instant) -> Result<(), EngineError>;
    fn cancel(&mut self);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowBinding {
    pub browser: BrowserId,
    pub window: u64,
}

/// One caret rectangle in the client area of the window named by its
/// [`WindowBinding`], in DIP.
///
/// The producing side has already added the surface origin exactly once, so a
/// platform service only converts DIP to client physical pixels using the
/// window's current scale factor. These are never screen coordinates and never
/// already-physical pixels; nothing here may be fed to `ClientToScreen` for an
/// IMM caret.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImeGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// UI-thread-only operations; platform window ownership remains with the host.
pub trait NativeViewServices {
    fn is_valid(&self, binding: WindowBinding) -> bool;
    /// Places the IME composition window at `caret`, which is expressed in the
    /// bound window's client-area DIP and already includes the surface origin
    /// (see [`ImeGeometry`]). Implementations convert to client physical pixels
    /// with the window's current scale exactly once, validate the original
    /// PID/thread/HWND/token, and never activate a window or switch layout/IME
    /// state when the binding is unknown or retired.
    fn position_ime(
        &mut self,
        binding: WindowBinding,
        caret: ImeGeometry,
    ) -> Result<(), EngineError>;
}

/// UI-independent reply endpoint implemented by the CEF adapter. `Ok` means
/// one response was admitted, not that the native callback has already run.
/// Implementations validate the complete opening identity and enabled command,
/// reserve one terminal response and invoke CEF only on its UI sequence outside
/// locks. A refused post must roll back only its own reservation; no implicit
/// retry, toolkit dependency or native continuation is exposed to the presenter.
/// Contextual endpoints retain the original binding/revision, use bound reserve
/// and take operations, and still requalify actual window/frame liveness.
pub trait ContextMenuResponseSink: Send + Sync {
    fn submit(
        &self,
        request: crate::menu::ContextMenuRequestId,
        response: crate::menu::ContextMenuResponse,
    ) -> Result<(), EngineError>;
}

/// A host-owned registration's owned-data ingress. No method receives native
/// objects or toolkit state. `capture` observes projected UI eligibility, not
/// native frame liveness; selection still needs the original stamp's CEF-side
/// `try_enter` and Browser/Frame qualification. Delivery itself does not pump,
/// wake or consume a Runtime event stream: the producer/host arranges its one
/// existing dispatcher wake after publication, outside locks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuDeliveryError {
    Closed,
    NotReady,
    WrongScope,
    Stale,
    Backpressure,
    Poisoned,
    Lease(crate::menu_lease::MenuLeaseError),
    Model(crate::menu_model::MenuTreeError),
    Context(crate::menu_context::MenuContextError),
}

pub trait ContextMenuDeliverySink: Send + Sync {
    fn capture(&self) -> Result<crate::menu_lease::MenuLeaseStamp, MenuDeliveryError>;
    fn publish(
        &self,
        presentation: crate::menu_context::MenuPresentation,
        response: Arc<dyn ContextMenuResponseSink>,
        stamp: crate::menu_lease::MenuLeaseStamp,
    ) -> Result<(), MenuDeliveryError>;
    /// Exact, backend-identified terminal notice, never a raw no-ID dismissal.
    fn dismiss(&self, request: crate::menu::ContextMenuRequestId) -> Result<(), MenuDeliveryError>;
}

/// Data-only target handed explicitly from a host UI registration to Runtime.
/// Cloning it keeps no UI entity/owner alive and is not window authentication.
#[derive(Clone)]
pub struct ContextMenuTarget {
    pub lease: crate::menu_lease::MenuLease,
    pub delivery: Arc<dyn ContextMenuDeliverySink>,
}

/// UI-toolkit-neutral owned menu delivery. The host injects a presenter gateway
/// and explicitly dispatches its bounded pending state with its own toolkit's
/// window/context. This port deliberately accepts no GPUI Window/App. A GPUI
/// popup, WPF control or another toolkit is an adapter, never this protocol.
///
/// Holding `response` retains only an owned endpoint, not a CEF MenuModel or
/// continuation. Dismissing visual state alone does not answer CEF: cancellation
/// must also be submitted through the same one-terminal-response endpoint.
pub trait ContextMenuPresenter {
    fn present(
        &mut self,
        presentation: crate::menu_context::MenuPresentation,
        response: Arc<dyn ContextMenuResponseSink>,
    ) -> Result<(), EngineError>;
    fn dismiss(&mut self, request: crate::menu::ContextMenuRequestId);
}

pub trait NavigationPolicy: Send + Sync {
    fn allows(&self, url: &str, is_main_frame: bool) -> bool;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiagnosticEvent {
    pub browser: Option<BrowserId>,
    pub code: Option<i32>,
    pub observed_at: Instant,
}

pub trait DiagnosticsSink: Send + Sync {
    fn record(&self, event: DiagnosticEvent);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CloseResult {
    Pending = 0,
    Cancelled = 1,
    Closed = 2,
}

/// Retains this close attempt's result even after a later Force.
/// The cell contains no native reference and cannot keep an adapter alive.
#[derive(Clone, Debug)]
pub struct CloseTicket {
    pub browser: BrowserId,
    pub sequence: u64,
    result: Arc<AtomicU8>,
}

#[derive(Clone, Debug)]
pub struct CloseCompletion(Weak<AtomicU8>);

impl CloseTicket {
    pub fn pending(browser: BrowserId, sequence: u64) -> (Self, CloseCompletion) {
        let result = Arc::new(AtomicU8::new(CloseResult::Pending as u8));
        let completion = CloseCompletion(Arc::downgrade(&result));
        (
            Self {
                browser,
                sequence,
                result,
            },
            completion,
        )
    }

    pub fn result(&self) -> CloseResult {
        match self.result.load(Ordering::Acquire) {
            1 => CloseResult::Cancelled,
            2 => CloseResult::Closed,
            _ => CloseResult::Pending,
        }
    }
}

impl PartialEq for CloseTicket {
    fn eq(&self, other: &Self) -> bool {
        self.browser == other.browser && self.sequence == other.sequence
    }
}
impl Eq for CloseTicket {}

impl CloseCompletion {
    pub fn complete(&self, result: CloseResult) {
        if result != CloseResult::Pending
            && let Some(cell) = self.0.upgrade()
        {
            let _ = cell.compare_exchange(0, result as u8, Ordering::AcqRel, Ordering::Acquire);
        }
    }
}

/// The runtime implements this endpoint; a handle cannot expose its native registry.
pub trait BrowserControl: Send + Sync {
    fn submit(&self, command: BrowserCommand) -> Result<CommandId, EngineError>;
    fn snapshot(&self) -> BrowserSnapshot;
    /// Explicitly unsupported unless the adapter provides real input observations.
    fn input_snapshot(&self) -> Result<InputSnapshot, EngineError> {
        Err(EngineError::NotReady)
    }
    /// Latest owned IME composition geometry. Defaults to `NotReady` instead of
    /// synthesizing an empty/zero observation for adapters without a real
    /// render-handler callback.
    fn ime_snapshot(&self) -> Result<ImeSnapshot, EngineError> {
        Err(EngineError::NotReady)
    }
    fn take_ux_events(&self) -> Vec<BrowserEvent> {
        Vec::new()
    }
    fn take_js_events(&self) -> Vec<BrowserEvent> {
        Vec::new()
    }
    fn take_view_events(&self) -> Vec<BrowserEvent> {
        Vec::new()
    }
    fn evaluate_script(
        &self,
        _script: String,
        _frame_scope: JsFrameScope,
        _timeout_ms: u64,
    ) -> Result<EvalRequestId, EngineError> {
        Err(EngineError::NotReady)
    }
    fn request_close(&self, mode: CloseMode) -> CloseTicket;
    /// Latest CEF `RenderHandler::update_drag_cursor` mask for inbound OSR drags.
    fn drag_target_operation(&self) -> DragOperationMask {
        DragOperationMask::NONE
    }
    /// Fast readiness query for high-rate pointer streaming without locking or
    /// cloning snapshots.
    fn is_fast_input_ready(&self) -> bool {
        let s = self.snapshot();
        s.state == crate::contracts::SessionState::Alive
            && s.renderer.state != crate::contracts::RendererState::Terminated
    }
    /// High-rate lock-free/coalesced `MouseMove` submission path preserving
    /// causal order with discrete input commands.
    fn submit_mouse_move_fast(
        &self,
        x: i32,
        y: i32,
        leaving: bool,
        modifiers: MouseModifiers,
    ) -> Result<(), EngineError> {
        let snapshot = self.input_snapshot()?;
        if snapshot.cleanup_pending || snapshot.cleanup_failed {
            if snapshot.cleanup_pending && !snapshot.cleanup_failed {
                return Err(EngineError::Backpressure);
            }
            return Err(EngineError::NotReady);
        }
        self.submit(BrowserCommand::Input(crate::input::InputEvent {
            epoch: snapshot.epoch,
            kind: crate::input::InputKind::MouseMove {
                x,
                y,
                leaving,
                modifiers,
            },
        }))
        .map(|_| ())
    }
    /// High-rate lock-free/coalesced `MouseWheel` submission path preserving
    /// causal order with discrete input commands.
    fn submit_mouse_wheel_fast(
        &self,
        x: i32,
        y: i32,
        delta_x: i32,
        delta_y: i32,
        modifiers: MouseModifiers,
    ) -> Result<(), EngineError> {
        let snapshot = self.input_snapshot()?;
        if snapshot.cleanup_pending || snapshot.cleanup_failed {
            if snapshot.cleanup_pending && !snapshot.cleanup_failed {
                return Err(EngineError::Backpressure);
            }
            return Err(EngineError::NotReady);
        }
        self.submit(BrowserCommand::Input(crate::input::InputEvent {
            epoch: snapshot.epoch,
            kind: crate::input::InputKind::MouseWheel {
                x,
                y,
                delta_x,
                delta_y,
                modifiers,
            },
        }))
        .map(|_| ())
    }
    /// Latest-wins coalesced zoom submission path avoiding Blink reflow backlog.
    fn submit_zoom_level_fast(&self, level: f64) -> Result<(), EngineError> {
        self.submit(BrowserCommand::SetZoomLevel(level)).map(|_| ())
    }
    /// Latest-wins coalesced OSR HTML5 drag-over/leave update for the active outbound drag.
    fn submit_osr_drag_move_fast(
        &self,
        _drag_id: crate::outbound_drag::DragSessionId,
        _x: i32,
        _y: i32,
        _modifiers: MouseModifiers,
        _in_view: bool,
    ) -> Result<(), EngineError> {
        Err(EngineError::InvalidRequest)
    }
    fn set_popup_policy(&self, _policy: PopupPolicy) {}
    fn popup_policy(&self) -> PopupPolicy {
        PopupPolicy::Auto
    }
    fn set_popup_filter(&self, _filter: Option<PopupFilter>) {}
    fn set_popup_ui_listener_active(&self, _active: bool) {}
    fn set_navigation_filter(&self, _filter: Option<NavigationFilter>) {}
    fn set_extended_event_mask(&self, _mask: u32) {}
    fn set_drag_enter_filter(&self, _filter: Option<DragEnterFilter>) {}
    fn set_focus_filter(&self, _filter: Option<SetFocusFilter>) {}
    fn set_pre_key_filter(&self, _filter: Option<PreKeyFilter>) {}
    fn set_key_event_filter(&self, _filter: Option<KeyEventFilter>) {}
    fn set_protocol_execution_filter(&self, _filter: Option<ProtocolExecutionFilter>) {}
    fn set_browser_host_capabilities(&self, _capabilities: Option<HostCapabilities>) {}
    fn set_browser_resource_interceptor(&self, _interceptor: Option<Arc<dyn ResourceInterceptor>>) {}
}

#[derive(Clone)]
pub struct BrowserHandle {
    control: Arc<dyn BrowserControl>,
}

impl BrowserHandle {
    pub fn new(control: Arc<dyn BrowserControl>) -> Self {
        Self { control }
    }

    pub fn drag_target_operation(&self) -> DragOperationMask {
        self.control.drag_target_operation()
    }

    pub fn is_fast_input_ready(&self) -> bool {
        self.control.is_fast_input_ready()
    }

    pub fn submit_mouse_move_fast(
        &self,
        x: i32,
        y: i32,
        leaving: bool,
        modifiers: MouseModifiers,
    ) -> Result<(), EngineError> {
        self.control
            .submit_mouse_move_fast(x, y, leaving, modifiers)
    }

    pub fn submit_mouse_wheel_fast(
        &self,
        x: i32,
        y: i32,
        delta_x: i32,
        delta_y: i32,
        modifiers: MouseModifiers,
    ) -> Result<(), EngineError> {
        self.control
            .submit_mouse_wheel_fast(x, y, delta_x, delta_y, modifiers)
    }

    pub fn submit_zoom_level_fast(&self, level: f64) -> Result<(), EngineError> {
        self.control.submit_zoom_level_fast(level)
    }

    pub fn submit_osr_drag_move_fast(
        &self,
        drag_id: crate::outbound_drag::DragSessionId,
        x: i32,
        y: i32,
        modifiers: MouseModifiers,
        in_view: bool,
    ) -> Result<(), EngineError> {
        self.control
            .submit_osr_drag_move_fast(drag_id, x, y, modifiers, in_view)
    }

    pub fn submit(&self, command: BrowserCommand) -> Result<CommandId, EngineError> {
        self.control.submit(command)
    }

    pub fn snapshot(&self) -> BrowserSnapshot {
        self.control.snapshot()
    }

    pub fn input_snapshot(&self) -> Result<InputSnapshot, EngineError> {
        self.control.input_snapshot()
    }

    pub fn ime_snapshot(&self) -> Result<ImeSnapshot, EngineError> {
        self.control.ime_snapshot()
    }

    pub fn take_ux_events(&self) -> Vec<BrowserEvent> {
        self.control.take_ux_events()
    }

    pub fn take_js_events(&self) -> Vec<BrowserEvent> {
        self.control.take_js_events()
    }

    pub fn take_view_events(&self) -> Vec<BrowserEvent> {
        self.control.take_view_events()
    }

    pub fn evaluate_script(
        &self,
        script: String,
        frame_scope: JsFrameScope,
        timeout_ms: u64,
    ) -> Result<EvalRequestId, EngineError> {
        self.control
            .evaluate_script(script, frame_scope, timeout_ms)
    }

    pub fn request_close(&self, mode: CloseMode) -> CloseTicket {
        self.control.request_close(mode)
    }

    pub fn set_popup_policy(&self, policy: PopupPolicy) {
        self.control.set_popup_policy(policy);
    }

    pub fn popup_policy(&self) -> PopupPolicy {
        self.control.popup_policy()
    }

    pub fn set_popup_filter(&self, filter: Option<PopupFilter>) {
        self.control.set_popup_filter(filter);
    }

    pub fn set_popup_ui_listener_active(&self, active: bool) {
        self.control.set_popup_ui_listener_active(active);
    }

    pub fn set_navigation_filter(&self, filter: Option<NavigationFilter>) {
        self.control.set_navigation_filter(filter);
    }

    pub fn set_extended_event_mask(&self, mask: u32) {
        self.control.set_extended_event_mask(mask);
    }

    pub fn set_drag_enter_filter(&self, filter: Option<DragEnterFilter>) {
        self.control.set_drag_enter_filter(filter);
    }

    pub fn set_focus_filter(&self, filter: Option<SetFocusFilter>) {
        self.control.set_focus_filter(filter);
    }

    pub fn set_pre_key_filter(&self, filter: Option<PreKeyFilter>) {
        self.control.set_pre_key_filter(filter);
    }

    pub fn set_key_event_filter(&self, filter: Option<KeyEventFilter>) {
        self.control.set_key_event_filter(filter);
    }

    pub fn set_protocol_execution_filter(&self, filter: Option<ProtocolExecutionFilter>) {
        self.control.set_protocol_execution_filter(filter);
    }

    pub fn set_browser_host_capabilities(&self, capabilities: Option<HostCapabilities>) {
        self.control.set_browser_host_capabilities(capabilities);
    }

    pub fn set_browser_resource_interceptor(
        &self,
        interceptor: Option<Arc<dyn ResourceInterceptor>>,
    ) {
        self.control.set_browser_resource_interceptor(interceptor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        contracts::RuntimeGeneration,
        mailbox::{FrameMailbox, SurfaceBounds},
    };

    #[test]
    fn engine_error_recoverable_capture_matches_expected_resize_states() {
        assert!(CaptureFailureKind::IncompleteFrame.is_recoverable_resize_transition());
        assert!(CaptureFailureKind::ResizeExhausted.is_recoverable_resize_transition());
        assert!(CaptureFailureKind::Busy.is_recoverable_resize_transition());
        assert!(!CaptureFailureKind::DeviceRemoved.is_recoverable_resize_transition());

        assert!(EngineError::Capture {
            kind: CaptureFailureKind::IncompleteFrame,
            code: None,
        }
        .is_recoverable_capture());
        assert!(EngineError::Capture {
            kind: CaptureFailureKind::ResizeExhausted,
            code: None,
        }
        .is_recoverable_capture());
        assert!(EngineError::Capture {
            kind: CaptureFailureKind::Busy,
            code: Some(1),
        }
        .is_recoverable_capture());
        assert!(!EngineError::Capture {
            kind: CaptureFailureKind::IncompleteFrame,
            code: Some(-1),
        }
        .is_recoverable_capture());
        assert!(!EngineError::Closed.is_recoverable_capture());
    }

    #[test]
    fn frame_source_has_pending_update_tracks_mailbox_dirty_flag() {
        let mailbox = FrameMailbox::new(BrowserId::new(RuntimeGeneration::new(1), 1));
        assert!(!FrameSource::has_pending_update(&mailbox));
        let ticket = mailbox
            .configure_view(
                SurfaceBounds {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 4,
                },
                4,
                4,
            )
            .expect("configure_view");
        assert!(ticket.is_some());
        assert!(FrameSource::has_pending_update(&mailbox));
        let _ = mailbox.take_update().expect("take_update");
        assert!(!FrameSource::has_pending_update(&mailbox));
    }
}
