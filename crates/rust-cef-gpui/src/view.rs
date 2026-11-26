//! Minimal GPUI VIEW presentation for one attached CEF Browser.
//!
//! This module covers task 10.1 (VIEW presentation) and the 10.2 binding
//! lifecycle: one already-created Browser is drawn into one existing GPUI
//! window, and its binding can be retired explicitly, by window close, or by
//! entity release, 10.3 POPUP composition, 11.1 owned input and the bounded
//! 11.2 IME geometry/UI service path. Real Chinese candidate/caret and moving
//! DPI validation remain pending; menus and view migration are not added here.
//!
//! Ownership rules:
//!
//! * The host creates the Application and the window. [`CefBrowser::new`] only
//!   builds an entity for an existing window: no `Application::new`, no
//!   `cx.quit`, no `process::exit`, no CEF call and no environment lookup.
//! * The one-shot [`BrowserAttachment`] is held for the entity lifetime. Its
//!   token is not cloned or moved to another window; dropping it is the
//!   Runtime's asynchronous Graceful close request, nothing here pumps or
//!   shuts CEF down.
//! * The window identity is fixed at construction. A different window cannot
//!   consume this instance's frames or paint them, and the Source is not read
//!   before that identity check passes. There is no implicit migration.
//! * Frames are consumed with the Runtime's own frame mailbox, copied through
//!   the same per-Browser CPU budget and moved into a real `RenderImage`.
//!   Replaced or released images are removed from the window atlas explicitly.
//! * [`WindowToken`] is the single liveness/identity source. Every paint and
//!   atlas import validates an owned `PaintStamp`, so a render closure that was
//!   already built cannot import old pixels after a replacement, release or
//!   retirement. An image `Arc` staying alive proves nothing about the binding.
//!   Retirement only prevents *future* atlas imports and paints; GPU commands
//!   already submitted to the window are not cancelled by this module.
//!
//! The view may render an empty surface while the Browser is still `Creating`
//! (the native instance was accepted but has not produced a VIEW frame yet).

use std::{
    cell::{Cell, RefCell},
    fmt,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Corners, CursorStyle, DispatchPhase,
    DragMoveEvent, ElementInputHandler, EntityInputHandler, ExternalPaths, FileDropEvent,
    FocusHandle, Hitbox, HitboxBehavior, InteractiveElement as _, IntoElement, KeyDownEvent,
    KeyUpEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _,
    PinchEvent, Pixels, Point, Render, RenderImage, Role, ScrollDelta, ScrollEvent,
    StatefulInteractiveElement as _, Styled as _, Subscription, TouchPhase, UTF16Selection, Window,
    canvas, div, prelude::FluentBuilder as _, px, rgb, size,
};
use image::{Frame, ImageBuffer, Rgba};
use rust_cef_core::{
    accessibility::AxTreeSnapshot,
    contracts::{
        BrowserCursor, BrowserId, CefFeatureState, DraggableRegionRect, MAX_FAVICON_URL_BYTES,
        MAX_FAVICON_URLS, MAX_STATUS_MESSAGE_BYTES, MAX_TOOLTIP_BYTES, MAX_ZOOM_LEVEL,
        MIN_ZOOM_LEVEL, NavigationSnapshot, RendererState, SessionState, ShortcutId, Viewport,
        step_canonical_zoom_level, truncate_utf8_bytes, zoom_level_to_milli,
    },
    file_drop::{FileDragPhase, FileDropPaths},
    frame_budget::FrameError,
    host_capabilities::PopupRequest,
    input::{
        BrowserTouchEvent, InputKind, InputResetReason, MAX_INPUT_RANGE_UTF16,
        MAX_INPUT_TEXT_UTF16, MouseButton as InputButton, TouchEventType, Utf16Range,
    },
    js_bridge::{EvalOutcome, EvalRequestId, JsFrameScope},
    keymap,
    mailbox::{FrameUpdate, SurfaceSnapshot},
    outbound_drag::{DragOperationMask, OsrDragFinish, OutboundDragFinish, OutboundDragStart},
    ports::{
        BrowserCommand, BrowserEvent, BrowserHandle, CloseMode, CommandId, EngineError, FrameInfo,
        FramePayload, FrameSource, ImeGeometry, MouseModifiers, OwnedFrame, PixelFormat,
        SurfaceKind, WindowBinding,
    },
};
use smallvec::smallvec;

use crate::{
    BrowserAttachment,
    binding::{BindingError, PaintStamp, WindowToken},
    events::{BrowserViewCallbacks, dispatch_single_view_event},
    input::{
        ClickGesture, InputAdmission, InputCallback, InputCallbackGate, InputDiagnostics,
        InputLane, MirrorEffect,
    },
    native_ime::{NativeImeController, NativeImeDiagnostics},
    native_keyboard::{NativeDownAdmission, NativeKey, ServiceState, claim_up},
};

/// Rejected view operation. A mismatched window or Browser is never repaired by
/// silently adopting another window or Source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewError {
    /// The attachment's Browser, its control handle and/or an incoming frame
    /// disagree about which Browser instance this view presents.
    BrowserMismatch {
        expected: BrowserId,
        actual: BrowserId,
    },
    /// The caller passed a window other than the one fixed at construction.
    WrongWindow {
        expected: u64,
        actual: u64,
    },
    /// The view's binding token is retired, so no operation may read the Source
    /// or paint its image. This is terminal for this instance.
    RetiredBinding,
    /// A binding revision could not be issued; the token has been permanently
    /// retired (checked overflow). No old image is revived by this failure.
    BindingExhausted,
    /// Invalid non-finite geometry or nonpositive window scale.
    InvalidGeometry,
    /// A budgeted frame could not become a `RenderImage`. The previous good
    /// image remains available at its original 1:1 size, clipped to the surface.
    Image(FrameError),
    /// A GPUI window operation (atlas removal or paint) failed.
    Window {
        message: String,
    },
    Input(EngineError),
    SurfaceModeMismatch,
    NativeWindowMismatch {
        expected: std::num::NonZeroUsize,
        actual: Option<std::num::NonZeroUsize>,
    },
}

impl fmt::Display for ViewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BrowserMismatch { expected, actual } => write!(
                formatter,
                "browser identity mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::WrongWindow { expected, actual } => write!(
                formatter,
                "CefBrowser is bound to window {expected}, not window {actual}"
            ),
            Self::RetiredBinding => formatter.write_str("window binding is retired"),
            Self::BindingExhausted => formatter.write_str("window binding revision exhausted"),
            Self::InvalidGeometry => formatter.write_str("invalid window scale or surface extent"),
            Self::Image(error) => write!(formatter, "frame presentation failed: {error}"),
            Self::Window { message } => write!(formatter, "window operation failed: {message}"),
            Self::Input(error) => write!(formatter, "input admission failed: {error:?}"),
            Self::SurfaceModeMismatch => {
                formatter.write_str("browser attachment surface mode does not match view adapter")
            }
            Self::NativeWindowMismatch { expected, actual } => write!(
                formatter,
                "native child parent HWND mismatch: expected 0x{:x}, got {:?}",
                expected.get(),
                actual.map(|h| format!("0x{:x}", h.get()))
            ),
        }
    }
}

impl std::error::Error for ViewError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Image(error) => Some(error),
            _ => None,
        }
    }
}

/// Cloneable 10.1 diagnostics for one VIEW, extended with actual 10.2 counters.
///
/// `paint_calls` counts only successful `Window::paint_image` calls, so it is
/// never advanced by `render` or by merely building a canvas. `painted_*`
/// describe the image that call actually painted. `surface_bounds` is the
/// content bounds (DIP, window coordinates) captured during prepaint and
/// `scale_factor` is the window scale observed at that same point; a host that
/// takes its own window screenshot can use both to locate the surface.
///
/// Added 10.2 fields:
///
/// * `binding_retired`: the token is permanently retired; paints and Source
///   reads are refused.
/// * `binding_revision`: the last revision this view successfully issued. It
///   starts at 1 and advances only after an atlas import or clear commits.
/// * `resync_attempts`: explicit `consume` calls that re-read `snapshot()`
///   because the previous conversion failed and no new update was pending.
/// * `rejected_paints`: prepaint/paint/entry attempts refused by the token gate.
/// * `pending_resync`: a following explicit `consume` should snapshot-retry.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewDiagnostics {
    pub consumed_updates: u64,
    pub created_images: u64,
    pub released_images: u64,
    pub paint_calls: u64,
    pub painted_sequence: u64,
    pub painted_generation: u64,
    pub frame_width: u32,
    pub frame_height: u32,
    pub surface_bounds: (f32, f32, f32, f32),
    pub scale_factor: f32,
    pub last_error: Option<String>,
    pub binding_retired: bool,
    pub binding_revision: u64,
    pub resync_attempts: u64,
    pub rejected_paints: u64,
    pub pending_resync: bool,
    /// Presses the view actually received from GPUI, and how many of those got
    /// past its own gates to be posted to CEF. A host can then tell "the click
    /// never reached the view" from "the view refused it".
    pub presses_received: u64,
    pub presses_forwarded: u64,
    pub popup: PopupDiagnostics,
    /// In-bounds left pairs whose raw UP was accepted (not renderer effects).
    pub clicks_submitted: u64,
    pub clicks_rejected: u64,
    pub pending_click: bool,
    pub input: InputDiagnostics,
    /// Actual owner-thread IMM positioning attempts, not Chinese IME proof.
    pub ime: NativeImeDiagnostics,
    pub zoom_level_milli: i32,
    pub zoom_gate: ZoomGateStats,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZoomGateStats {
    pub requests: u64,
    pub dispatched: u64,
    pub coalesced: u64,
    pub frame_ack_flushes: u64,
    pub timeout_flushes: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PopupDiagnostics {
    pub generation: u64,
    pub visible: bool,
    pub bounds: (i32, i32, i32, i32),
    pub frame_width: u32,
    pub frame_height: u32,
    pub created_images: u64,
    pub released_images: u64,
    pub paint_calls: u64,
    pub painted_sequence: u64,
    pub painted_generation: u64,
    pub painted_bounds: (f32, f32, f32, f32),
}

/// Process-local witness of a successful GPUI VIEW paint submission.
/// This does not attest to swapchain present, scan-out, or desktop visibility.
#[cfg(feature = "validation")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewPaintSample {
    pub browser: BrowserId,
    pub window: u64,
    pub generation: u64,
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub paint_finished_at: std::time::Instant,
}

#[cfg(feature = "validation")]
struct ViewPaintTiming {
    generation: u64,
    last_sequence: u64,
    samples: Vec<ViewPaintSample>,
}

/// Diagnostics written from prepaint/paint closures.
///
/// GPUI closures must be `'static`, so the counters shared with them live in an
/// `Rc<RefCell<_>>` owned by the entity. Only short, non-reentrant borrows are
/// taken; no GPUI window/app is borrowed while it is held.
#[derive(Default)]
struct PaintDiagnostics {
    paint_calls: u64,
    painted_sequence: u64,
    painted_generation: u64,
    surface_bounds: (f32, f32, f32, f32),
    scale_factor: f32,
    last_error: Option<String>,
    rejected_paints: u64,
    popup_calls: u64,
    popup_sequence: u64,
    popup_generation: u64,
    popup_bounds: (f32, f32, f32, f32),
    #[cfg(feature = "validation")]
    view_timing: Option<ViewPaintTiming>,
}

impl PaintDiagnostics {
    fn reject_stamp(
        &mut self,
        token: &WindowToken,
        stamp: PaintStamp,
        actual: u64,
        expected: u64,
        operation: &str,
    ) {
        self.rejected_paints = self.rejected_paints.saturating_add(1);
        // Normal layout/image replacement cancels a previously built canvas.
        // Never paint it, never erase an earlier real error, and never treat a
        // wrong binding/window, future stamp or retired token as cancellation.
        if actual != expected || !token.is_obsolete(stamp, actual) {
            self.last_error.get_or_insert_with(|| {
                format!("{operation} skipped: paint stamp no longer permits window {actual}")
            });
        }
    }
}

#[derive(Clone)]
enum PresentedContent {
    CpuImage(Arc<RenderImage>),
    GpuTexture(Arc<OwnedFrame>),
}

impl PresentedContent {
    fn size_px(&self) -> (u32, u32) {
        match self {
            Self::CpuImage(image) => {
                let size = image.size(0);
                (u32::from(size.width), u32::from(size.height))
            }
            Self::GpuTexture(frame) => (frame.info.width, frame.info.height),
        }
    }
}

/// The displayed content plus the exact identity it carries.
///
/// Metadata is committed or retained together with the owner: a failed
/// atlas drop restores the complete old slot, never a half-cleared one.
struct ImageSlot {
    content: PresentedContent,
    sequence: u64,
    generation: u64,
    width: u32,
    height: u32,
    captured_scale: Option<f32>,
    backdrop_rgb: u32,
}

/// The exact content a render pass will paint, plus the identity and stamp it
/// needs to pass the prepaint/paint gate.
struct PaintFrame {
    content: PresentedContent,
    sequence: u64,
    generation: u64,
    captured_scale: Option<f32>,
    stamp: PaintStamp,
}

struct ActiveCefDrag {
    drag_id: rust_cef_core::outbound_drag::DragSessionId,
    allowed_ops: DragOperationMask,
    current_op: DragOperationMask,
    in_view: bool,
    cursor_win_dip: (f32, f32),
    last_view_point: (i32, i32),
    preview_image: Option<Arc<RenderImage>>,
    preview_size_dip: (f32, f32),
    hotspot_dip: (f32, f32),
    fallback_label: Option<String>,
}

struct DragPaintOverlay {
    cursor_win_dip: (f32, f32),
    preview_image: Arc<RenderImage>,
    preview_size_dip: (f32, f32),
    hotspot_dip: (f32, f32),
}

fn drag_cursor_style(op: DragOperationMask) -> CursorStyle {
    if op == DragOperationMask::NONE {
        return CursorStyle::OperationNotAllowed;
    }
    let preferred = op.preferred_single_op();
    if preferred == DragOperationMask::LINK {
        CursorStyle::DragLink
    } else if preferred == DragOperationMask::MOVE {
        CursorStyle::ClosedHand
    } else {
        CursorStyle::DragCopy
    }
}

fn drag_preview_from_start(
    start: &OutboundDragStart,
    window_scale: f32,
) -> (
    Option<Arc<RenderImage>>,
    (f32, f32),
    (f32, f32),
    Option<String>,
) {
    if let Some(img) = &start.payload.image {
        let mut premul = img.bgra.clone();
        // GPUI's DirectX atlas uses PremultipliedBgra8; apply slight ghost
        // translucency while premultiplying from CEF's POSTMULTIPLIED bitmap.
        for px in premul.chunks_exact_mut(4) {
            let eff_a = (u32::from(px[3]) * 224 + 127) / 255;
            px[0] = ((u32::from(px[0]) * eff_a + 127) / 255) as u8;
            px[1] = ((u32::from(px[1]) * eff_a + 127) / 255) as u8;
            px[2] = ((u32::from(px[2]) * eff_a + 127) / 255) as u8;
            px[3] = eff_a as u8;
        }
        if let Some(buffer) =
            ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(img.width, img.height, premul)
        {
            let scale = if img.scale_milli > 0 {
                img.scale_milli as f32 / 1000.0
            } else {
                window_scale.max(1.0)
            };
            let size_dip = (img.width as f32 / scale, img.height as f32 / scale);
            let hotspot_dip = (img.hotspot_x as f32 / scale, img.hotspot_y as f32 / scale);
            return (
                Some(Arc::new(RenderImage::new(smallvec![Frame::new(buffer)]))),
                size_dip,
                hotspot_dip,
                None,
            );
        }
    }
    let fallback = start
        .payload
        .url
        .as_deref()
        .or(start.payload.text.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut out = String::new();
            for ch in s.chars().take(48) {
                out.push(ch);
            }
            if s.chars().count() > 48 {
                out.push_str("...");
            }
            out
        });
    (None, (0.0, 0.0), (0.0, 0.0), fallback)
}

/// Explicit total mapping from UI-agnostic [`BrowserCursor`] to [`CursorStyle`].
/// Every standard GPUI variant maps 1-to-1; custom cursors fall back to `Arrow`.
pub fn cursor_style_for(cursor: BrowserCursor) -> CursorStyle {
    match cursor {
        BrowserCursor::Arrow => CursorStyle::Arrow,
        BrowserCursor::IBeam => CursorStyle::IBeam,
        BrowserCursor::Crosshair => CursorStyle::Crosshair,
        BrowserCursor::ClosedHand => CursorStyle::ClosedHand,
        BrowserCursor::OpenHand => CursorStyle::OpenHand,
        BrowserCursor::PointingHand => CursorStyle::PointingHand,
        BrowserCursor::ResizeLeft => CursorStyle::ResizeLeft,
        BrowserCursor::ResizeRight => CursorStyle::ResizeRight,
        BrowserCursor::ResizeLeftRight => CursorStyle::ResizeLeftRight,
        BrowserCursor::ResizeUp => CursorStyle::ResizeUp,
        BrowserCursor::ResizeDown => CursorStyle::ResizeDown,
        BrowserCursor::ResizeUpDown => CursorStyle::ResizeUpDown,
        BrowserCursor::ResizeUpLeftDownRight => CursorStyle::ResizeUpLeftDownRight,
        BrowserCursor::ResizeUpRightDownLeft => CursorStyle::ResizeUpRightDownLeft,
        BrowserCursor::ResizeColumn => CursorStyle::ResizeColumn,
        BrowserCursor::ResizeRow => CursorStyle::ResizeRow,
        BrowserCursor::IBeamVertical => CursorStyle::IBeamCursorForVerticalLayout,
        BrowserCursor::OperationNotAllowed => CursorStyle::OperationNotAllowed,
        BrowserCursor::DragLink => CursorStyle::DragLink,
        BrowserCursor::DragCopy => CursorStyle::DragCopy,
        BrowserCursor::ContextualMenu => CursorStyle::ContextualMenu,
        BrowserCursor::CustomUnsupported => CursorStyle::Arrow,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ViewUxState {
    pub(crate) address: String,
    pub(crate) title: String,
    pub(crate) is_loading: bool,
    pub(crate) can_go_back: bool,
    pub(crate) can_go_forward: bool,
    pub(crate) main_http_status: Option<i32>,
    pub(crate) last_main_frame_error: Option<i32>,
    pub(crate) cursor: BrowserCursor,
    pub(crate) tooltip: String,
    pub(crate) status_text: String,
    pub(crate) favicon_urls: Vec<String>,
    pub(crate) fullscreen: bool,
    pub(crate) loading_progress_per_mille: u16,
    pub(crate) zoom_level_milli: i32,
    pub(crate) audio_muted: bool,
    pub(crate) draggable_regions: Vec<DraggableRegionRect>,
}

impl ViewUxState {
    pub(crate) fn apply_snapshot(&mut self, nav: &NavigationSnapshot) {
        self.address.clone_from(&nav.address);
        self.title.clone_from(&nav.title);
        self.is_loading = nav.is_loading;
        self.can_go_back = nav.can_go_back;
        self.can_go_forward = nav.can_go_forward;
        self.main_http_status = nav.main_http_status;
        self.last_main_frame_error = nav.last_main_frame_error;
        self.cursor = nav.cursor;
        self.tooltip = truncate_utf8_bytes(&nav.tooltip, MAX_TOOLTIP_BYTES);
        self.status_text = truncate_utf8_bytes(&nav.status_message, MAX_STATUS_MESSAGE_BYTES);
        self.favicon_urls = nav
            .favicon_urls
            .iter()
            .take(MAX_FAVICON_URLS)
            .map(|url| truncate_utf8_bytes(url, MAX_FAVICON_URL_BYTES))
            .collect();
        self.fullscreen = nav.fullscreen;
        self.loading_progress_per_mille = nav.loading_progress_per_mille.min(1000);
        self.zoom_level_milli = nav.zoom_level_milli;
        self.audio_muted = nav.audio_muted;
    }

    pub(crate) fn apply_event(
        &mut self,
        event: BrowserEvent,
    ) -> (Option<bool>, Option<ShortcutId>) {
        match event {
            BrowserEvent::CursorChanged(cursor) => self.cursor = cursor,
            BrowserEvent::TooltipChanged(text) => {
                self.tooltip = truncate_utf8_bytes(&text, MAX_TOOLTIP_BYTES);
            }
            BrowserEvent::StatusMessageChanged(text) => {
                self.status_text = truncate_utf8_bytes(&text, MAX_STATUS_MESSAGE_BYTES);
            }
            BrowserEvent::FaviconUrlsChanged(urls) => {
                self.favicon_urls = urls
                    .into_iter()
                    .take(MAX_FAVICON_URLS)
                    .map(|url| truncate_utf8_bytes(&url, MAX_FAVICON_URL_BYTES))
                    .collect();
            }
            BrowserEvent::FullscreenModeChanged(fullscreen) => self.fullscreen = fullscreen,
            BrowserEvent::LoadingProgressChanged(progress) => {
                self.loading_progress_per_mille = progress.min(1000);
            }
            BrowserEvent::ZoomLevelChanged(zoom_milli) => self.zoom_level_milli = zoom_milli,
            BrowserEvent::AudioMutedChanged(muted) => self.audio_muted = muted,
            BrowserEvent::FocusTaken { next } => return (Some(next), None),
            BrowserEvent::HostShortcutTriggered { id } => return (None, Some(id)),
            _ => {}
        }
        (None, None)
    }
}

pub(crate) fn clear_routing_and_filters(handle: &BrowserHandle) {
    handle.set_popup_ui_listener_active(false);
    handle.set_extended_event_mask(0);
    handle.set_popup_filter(None);
    handle.set_navigation_filter(None);
    handle.set_drag_enter_filter(None);
    handle.set_focus_filter(None);
    handle.set_pre_key_filter(None);
    handle.set_key_event_filter(None);
    handle.set_protocol_execution_filter(None);
    handle.set_browser_host_capabilities(None);
    handle.set_browser_resource_interceptor(None);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingZoom {
    target_milli: i32,
    submitted_at: Instant,
}

const ZOOM_IN_FLIGHT_TIMEOUT: Duration = Duration::from_millis(120);
const ZOOM_EPSILON: f64 = 1e-6;

#[derive(Clone, Copy, Debug, PartialEq)]
enum ZoomFlushAction {
    Idle,
    UnlockedNoTail,
    Dispatch { level: f64, via_timeout: bool },
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct ZoomGate {
    desired_zoom_level: f64,
    last_sent_zoom_level: f64,
    zoom_in_flight: Option<(u64, Instant)>,
    stats: ZoomGateStats,
}

impl ZoomGate {
    fn desired_zoom_level(&self) -> f64 {
        self.desired_zoom_level
    }

    fn stats(&self) -> ZoomGateStats {
        self.stats
    }

    fn is_busy(&self) -> bool {
        self.zoom_in_flight.is_some()
            || (self.desired_zoom_level - self.last_sent_zoom_level).abs() > ZOOM_EPSILON
    }

    fn has_deferred_tail(&self) -> bool {
        self.zoom_in_flight.is_some()
            && (self.desired_zoom_level - self.last_sent_zoom_level).abs() > ZOOM_EPSILON
    }

    fn in_flight_since(&self) -> Option<Instant> {
        self.zoom_in_flight.map(|(_, sent_at)| sent_at)
    }

    fn expire_if_ready(&mut self, current_frame_seq: u64, now: Instant) -> Option<bool> {
        let (sent_seq, sent_at) = self.zoom_in_flight?;
        if current_frame_seq > sent_seq {
            self.zoom_in_flight = None;
            Some(false)
        } else if now.saturating_duration_since(sent_at) > ZOOM_IN_FLIGHT_TIMEOUT {
            self.zoom_in_flight = None;
            Some(true)
        } else {
            None
        }
    }

    fn plan_request(&mut self, target: f64, current_frame_seq: u64, now: Instant) -> Option<f64> {
        self.stats.requests = self.stats.requests.saturating_add(1);
        let _ = self.expire_if_ready(current_frame_seq, now);
        if self.zoom_in_flight.is_none() {
            if (target - self.last_sent_zoom_level).abs() > ZOOM_EPSILON {
                Some(target)
            } else {
                self.desired_zoom_level = target;
                None
            }
        } else {
            self.desired_zoom_level = target;
            self.stats.coalesced = self.stats.coalesced.saturating_add(1);
            None
        }
    }

    fn poll_flush(&mut self, current_frame_seq: u64, now: Instant) -> ZoomFlushAction {
        let unlock = self.expire_if_ready(current_frame_seq, now);
        if self.zoom_in_flight.is_some() {
            return ZoomFlushAction::Idle;
        }
        if (self.desired_zoom_level - self.last_sent_zoom_level).abs() > ZOOM_EPSILON {
            ZoomFlushAction::Dispatch {
                level: self.desired_zoom_level,
                via_timeout: unlock == Some(true),
            }
        } else if unlock.is_some() {
            ZoomFlushAction::UnlockedNoTail
        } else {
            ZoomFlushAction::Idle
        }
    }

    fn commit_sent(
        &mut self,
        sent_level: f64,
        current_frame_seq: u64,
        now: Instant,
        tail_flush_via_timeout: Option<bool>,
    ) {
        self.desired_zoom_level = sent_level;
        self.last_sent_zoom_level = sent_level;
        self.zoom_in_flight = Some((current_frame_seq, now));
        self.stats.dispatched = self.stats.dispatched.saturating_add(1);
        match tail_flush_via_timeout {
            Some(false) => {
                self.stats.frame_ack_flushes = self.stats.frame_ack_flushes.saturating_add(1);
            }
            Some(true) => {
                self.stats.timeout_flushes = self.stats.timeout_flushes.saturating_add(1);
            }
            None => {}
        }
    }

    fn abort_deferred_tail(&mut self) {
        self.desired_zoom_level = self.last_sent_zoom_level;
        self.zoom_in_flight = None;
    }

    fn clear_in_flight(&mut self) {
        self.zoom_in_flight = None;
    }

    fn sync_external(&mut self, level: f64) {
        if level.is_finite() && !self.is_busy() {
            self.desired_zoom_level = level;
            self.last_sent_zoom_level = level;
        }
    }
}

pub type TakeFocusCallback =
    Box<dyn FnMut(bool, &mut CefBrowser, &mut Window, &mut Context<CefBrowser>)>;
pub type ShortcutCallback =
    Box<dyn FnMut(ShortcutId, &mut CefBrowser, &mut Window, &mut Context<CefBrowser>)>;
pub type JsMessageCallback =
    Box<dyn FnMut(String, String, &mut CefBrowser, &mut Window, &mut Context<CefBrowser>)>;
pub type EvalCompletedCallback = Box<
    dyn FnMut(EvalRequestId, EvalOutcome, &mut CefBrowser, &mut Window, &mut Context<CefBrowser>),
>;
pub type OutboundDragCallback = Box<
    dyn FnMut(
        OutboundDragStart,
        &mut CefBrowser,
        &mut Window,
        &mut Context<CefBrowser>,
    ) -> Option<OutboundDragFinish>,
>;
pub type PopupCallback =
    Box<dyn FnMut(PopupRequest, &mut CefBrowser, &mut Window, &mut Context<CefBrowser>)>;
pub type NavigationCallback =
    Box<dyn FnMut(String, &mut CefBrowser, &mut Window, &mut Context<CefBrowser>)>;

/// GPUI view over one attached CEF Browser VIEW surface.
pub struct CefBrowser {
    attachment: BrowserAttachment,
    browser: BrowserId,
    /// Fixed at construction; every window operation compares the real handle
    /// (id + root type) against this. The `WindowToken` is derived from the
    /// same handle, so there is no second identity source.
    window_handle: AnyWindowHandle,
    token: WindowToken,
    image: Option<ImageSlot>,
    popup_image: Option<ImageSlot>,
    popup_generation: u64,
    popup_visible: bool,
    popup_invalid: bool,
    popup_bounds: (i32, i32, i32, i32),
    popup_created: u64,
    popup_released: u64,
    viewport_context: Option<Viewport>,
    surface_geometry: Option<(f32, f32, f32, f32)>,
    click: Rc<RefCell<ClickGesture>>,
    input: Rc<RefCell<InputLane>>,
    file_hitbox: Rc<RefCell<Option<(PaintStamp, Hitbox)>>>,
    file_drag: Option<crate::file_drag::FileDragSession>,
    file_serial: u64,
    file_fence: Option<u64>,
    focus: FocusHandle,
    native_keyboard: Option<std::rc::Weak<ServiceState>>,
    native_ime: Rc<NativeImeController>,
    ime_request: Option<(u64, u64, ImeGeometry, u64, f32)>,
    was_focused: bool,
    cef_focus_accepted: Rc<Cell<bool>>,
    composition: Option<String>,
    composition_selection: std::ops::Range<usize>,
    consumed_updates: u64,
    presses_received: u64,
    presses_forwarded: u64,
    created_images: u64,
    released_images: u64,
    resync_attempts: u64,
    binding_revision: u64,
    needs_resync: bool,
    paint: Rc<RefCell<PaintDiagnostics>>,
    /// Last `SetViewport` accepted by the Browser handle, shared with prepaint
    /// so an unchanged layout does not resubmit one every frame.
    sent_viewport: Rc<Cell<Option<Viewport>>>,
    in_flight_viewport: Rc<Cell<Option<(Viewport, std::time::Instant)>>>,
    sent_screen_origin: Rc<Cell<Option<(i32, i32)>>>,
    sent_client_offset: Rc<Cell<Option<(f32, f32)>>>,
    minimized_hidden: Rc<Cell<bool>>,
    drop_surface_geometry: Rc<Cell<(f32, f32, f32, f32, f32)>>,
    /// Finite release, close, activation and bounds observers, all owned here.
    /// They are never detached into the App for the process lifetime.
    subscriptions: [Option<Subscription>; 6],
    /// Weak UI-local projection only; the explicit host service owns the lease.
    menu: crate::menu::MenuProjection,
    ux: ViewUxState,
    outbound_drag: crate::outbound_drag::OutboundDragState,
    outbound_drag_pump: Option<crate::ole_drag::IntegratedPumpCallback>,
    outbound_drag_task: Option<gpui::Task<()>>,
    on_outbound_drag: Option<OutboundDragCallback>,
    active_drag: Option<ActiveCefDrag>,
    active_drag_flag: Rc<Cell<bool>>,
    left_mouse_down: bool,
    last_pointer_win_dip: Option<(f32, f32)>,
    ax_snapshot: Option<AxTreeSnapshot>,
    pinch_zoom_enabled: bool,
    ctrl_wheel_zoom_enabled: bool,
    surface_opacity: f32,
    rotation_angle_deg: f32,
    nearest_neighbor_scaling: bool,
    fit_rotated_to_bounds: bool,
    transparent_backdrop: bool,
    scroll_accumulator: keymap::ScrollWheelAccumulator,
    ctrl_wheel_zoom_notches: f32,
    ctrl_wheel_last_at: Option<Instant>,
    pending_zoom: Option<PendingZoom>,
    latest_view_frame_seq: u64,
    zoom_gate: ZoomGate,
    zoom_timeout_task: Option<gpui::Task<()>>,
    last_hover_dip: Option<(f32, f32)>,
    on_take_focus: Option<TakeFocusCallback>,
    on_shortcut: Option<ShortcutCallback>,
    on_js_message: Option<JsMessageCallback>,
    on_eval_completed: Option<EvalCompletedCallback>,
    on_popup: Option<PopupCallback>,
    on_popup_epoch: u64,
    on_address_changed: Option<NavigationCallback>,
    on_address_epoch: u64,
    callbacks: BrowserViewCallbacks<Self>,
    hwnd_raw: Option<isize>,
    native_cursor_revision: u64,
    native_cursor: rust_cef_core::cursor::NativeCursorData,
    pointer_over_surface: bool,
    right_mouse_down: bool,
    async_ops: Arc<crate::async_ops::AsyncOpLedger>,
}

const NATIVE_CONTEXT_MENU_REOPEN_WINDOW: Duration = Duration::from_millis(400);

impl CefBrowser {
    /// Builds the view in an existing GPUI window.
    ///
    /// `attachment` must come from `Runtime::attach_browser` for the Browser
    /// this view presents; it is consumed exactly once. Identity checks that can
    /// fail are completed before `cx.new`, whose closure only constructs the
    /// entity. Creating/closed state is observed later by [`Self::consume`],
    /// which never panics on it.
    pub fn new(
        attachment: BrowserAttachment,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<gpui::Entity<Self>, ViewError> {
        let browser = attachment.browser();
        let handle_browser = attachment.handle().snapshot().browser;
        if handle_browser != browser {
            return Err(ViewError::BrowserMismatch {
                expected: browser,
                actual: handle_browser,
            });
        }
        if attachment.surface_mode().is_native_child_hwnd() {
            return Err(ViewError::SurfaceModeMismatch);
        }
        let window_handle = window.window_handle();
        let hwnd_raw = crate::native_view::window_hwnd(window).map(|h| h.get() as isize);
        let window_id = window_handle.window_id().as_u64();
        let scale_factor = checked_scale(window.scale_factor())?;
        let sent_viewport = Rc::new(Cell::new(None));
        let in_flight_viewport = Rc::new(Cell::new(None));
        let sent_screen_origin = Rc::new(Cell::new(None));
        let sent_client_offset = Rc::new(Cell::new(None));
        let minimized_hidden = Rc::new(Cell::new(false));
        let drop_surface_geometry = Rc::new(Cell::new((0.0, 0.0, 0.0, 0.0, scale_factor)));
        let paint = Rc::new(RefCell::new(PaintDiagnostics {
            scale_factor,
            ..PaintDiagnostics::default()
        }));
        let token = WindowToken::new(WindowBinding {
            browser,
            window: window_id,
        });
        let native_ime =
            NativeImeController::new(token.clone(), window).map_err(ViewError::Input)?;
        let click = Rc::new(RefCell::new(ClickGesture::default()));
        let input = Rc::new(RefCell::new(InputLane::default()));
        let close_input = input.clone();
        let close_click = click.clone();
        let close_token = token.clone();
        let close_handle = attachment.handle().clone();
        let close_ax_tree = attachment.ax_tree().clone();
        let initial_ax_snapshot = close_ax_tree.snapshot();
        let menu = Rc::new(RefCell::new(std::rc::Weak::new()));
        let close_menu = menu.clone();
        let async_ops = crate::async_ops::AsyncOpLedger::new();
        let close_async_ops = Arc::clone(&async_ops);
        let view = cx.new(|cx| Self {
            attachment,
            browser,
            window_handle,
            token,
            image: None,
            popup_image: None,
            popup_generation: 0,
            popup_visible: false,
            popup_invalid: false,
            popup_bounds: (0, 0, 0, 0),
            popup_created: 0,
            popup_released: 0,
            viewport_context: None,
            surface_geometry: None,
            click,
            input,
            file_hitbox: Rc::new(RefCell::new(None)),
            file_drag: None,
            file_serial: 0,
            file_fence: None,
            focus: cx.focus_handle(),
            native_keyboard: None,
            native_ime,
            ime_request: None,
            was_focused: false,
            cef_focus_accepted: Rc::new(Cell::new(false)),
            composition: None,
            composition_selection: 0..0,
            consumed_updates: 0,
            presses_received: 0,
            presses_forwarded: 0,
            created_images: 0,
            released_images: 0,
            resync_attempts: 0,
            binding_revision: 1,
            needs_resync: false,
            paint,
            sent_viewport,
            in_flight_viewport,
            sent_screen_origin,
            sent_client_offset,
            minimized_hidden,
            drop_surface_geometry,
            subscriptions: [None, None, None, None, None, None],
            menu,
            ux: ViewUxState::default(),
            outbound_drag: crate::outbound_drag::OutboundDragState::new(),
            outbound_drag_pump: None,
            outbound_drag_task: None,
            on_outbound_drag: None,
            active_drag: None,
            active_drag_flag: Rc::new(Cell::new(false)),
            left_mouse_down: false,
            last_pointer_win_dip: None,
            ax_snapshot: initial_ax_snapshot,
            pinch_zoom_enabled: false,
            ctrl_wheel_zoom_enabled: false,
            surface_opacity: 1.0,
            rotation_angle_deg: 0.0,
            nearest_neighbor_scaling: false,
            fit_rotated_to_bounds: true,
            transparent_backdrop: false,
            scroll_accumulator: keymap::ScrollWheelAccumulator::default(),
            ctrl_wheel_zoom_notches: 0.0,
            ctrl_wheel_last_at: None,
            pending_zoom: None,
            latest_view_frame_seq: 0,
            zoom_gate: ZoomGate::default(),
            zoom_timeout_task: None,
            last_hover_dip: None,
            on_take_focus: None,
            on_shortcut: None,
            on_js_message: None,
            on_eval_completed: None,
            on_popup: None,
            on_popup_epoch: 0,
            on_address_changed: None,
            on_address_epoch: 0,
            callbacks: BrowserViewCallbacks::default(),
            hwnd_raw,
            native_cursor_revision: 0,
            native_cursor: rust_cef_core::cursor::NativeCursorData::default(),
            pointer_over_surface: false,
            right_mouse_down: false,
            async_ops,
        });

        // Release runs while the entity still exists but before it is dropped.
        // Retire first; image cleanup is independent of that retirement and
        // validates the original window id itself.
        let release_handle = window_handle;
        let release_subscription = cx.observe_release(&view, move |view, app| {
            view.release_on_entity_release(app, release_handle);
        });
        // A closed window is already inaccessible. Retire the route so it
        // cannot occupy composition capacity forever, and submit the ordinary
        // Graceful close on the original control. No Entity is captured.
        let close_view = view.downgrade();
        let close_subscription = cx.on_window_closed(move |app, closed_window| {
            if closed_window.as_u64() != window_id {
                return;
            }
            crate::menu::retire_projection(&close_menu);
            close_token.retire();
            close_ax_tree.clear();
            close_async_ops.retire_and_cancel_all();
            if close_view
                .update(app, |view, _| {
                    view.clear_callbacks_and_filters();
                    view.cancel_outbound_drags();
                    view.ax_snapshot = None;
                    view.reset_input(InputResetReason::Closing);
                })
                .is_err()
            {
                // The entity may already have been released. The finite lane
                // still owns the same handle and can revoke its pending event.
                close_click.borrow_mut().cancel();
                close_input
                    .borrow_mut()
                    .abandon(&close_handle, InputResetReason::Closing);
            }
            clear_routing_and_filters(&close_handle);
            close_handle.request_close(CloseMode::Graceful);
        });
        crate::native_view::install_smooth_resize_hook(window);
        view.update(cx, move |view, cx| {
            crate::native_view::register_sizemove_source(
                window,
                surface_cursor_key(view.browser),
                std::sync::Arc::downgrade(view.attachment.frame_source()),
            );
            let activation = cx.observe_window_activation(window, |view, window, cx| {
                if !crate::native_view::is_window_effectively_active(window) {
                    if view.active_drag.is_some() {
                        view.finish_active_osr_drag(
                            window.mouse_position(),
                            gpui::Modifiers::default(),
                            true,
                            window,
                            cx,
                        );
                    }
                    view.left_mouse_down = false;
                    if let Some(b) = crate::menu::project(&view.menu) {
                        b.readiness(false);
                    }
                    if view.attachment.native_context_menu().is_open() {
                        view.cef_focus_accepted.set(false);
                        view.right_mouse_down = false;
                        view.was_focused = false;
                    } else {
                        view.reset_input(InputResetReason::FocusLost);
                        view.was_focused = false;
                    }
                    cx.notify();
                } else {
                    view.ime_request = None;
                    view.refresh_ime(window, cx);
                    view.project_menu_focus(window, cx);
                }
            });
            let last_observed_extent = Cell::new(None::<(f32, f32, f32)>);
            let bounds = cx.observe_window_bounds(window, move |view, window, cx| {
                if let Some(b) = crate::menu::project(&view.menu) {
                    b.geometry_changed();
                }
                let in_sizemove = crate::native_view::is_window_in_sizemove(window);
                let minimized = crate::native_view::is_gpui_window_minimized(window);
                if minimized != view.minimized_hidden.get()
                    && view.attachment.handle().snapshot().state == SessionState::Alive
                {
                    if view
                        .attachment
                        .handle()
                        .submit(BrowserCommand::SetVisible(!minimized))
                        .is_ok()
                    {
                        view.minimized_hidden.set(minimized);
                    }
                }
                let wb = window.bounds();
                let extent = (
                    f32::from(wb.size.width),
                    f32::from(wb.size.height),
                    window.scale_factor(),
                );
                let size_changed = last_observed_extent.replace(Some(extent)) != Some(extent);
                if !in_sizemove {
                    if !minimized
                        && view.attachment.handle().snapshot().state == SessionState::Alive
                        && let Some((cx_px, cy_px)) =
                            crate::native_view::window_client_screen_origin(window)
                    {
                        let (sx, sy, _, _) = view.paint.borrow().surface_bounds;
                        let scale = window.scale_factor();
                        let origin = (
                            cx_px.saturating_add((sx * scale).round() as i32),
                            cy_px.saturating_add((sy * scale).round() as i32),
                        );
                        if view.sent_screen_origin.get() != Some(origin)
                            && view
                                .attachment
                                .handle()
                                .submit(BrowserCommand::SetScreenOrigin {
                                    x_px: origin.0,
                                    y_px: origin.1,
                                })
                                .is_ok()
                        {
                            view.sent_screen_origin.set(Some(origin));
                        }
                    }
                    view.ime_request = None;
                    view.refresh_ime(window, cx);
                    cx.notify();
                } else if !minimized && size_changed {
                    if view.composition.is_some() {
                        view.ime_request = None;
                        view.refresh_ime(window, cx);
                    }
                    let _ = view.consume(window, cx);
                }
            });
            let focus = view.focus.clone();
            let menu_focus_in = cx.on_focus_in(&focus, window, |view, window, cx| {
                view.project_menu_focus(window, cx)
            });
            let menu_focus_out = cx.on_focus_out(&focus, window, |view, _, window, cx| {
                view.project_menu_focus(window, cx)
            });
            view.subscriptions = [
                Some(release_subscription),
                Some(close_subscription),
                Some(activation),
                Some(bounds),
                Some(menu_focus_in),
                Some(menu_focus_out),
            ];
        });
        Ok(view)
    }

    pub(crate) fn menu_attachment(
        &self,
    ) -> (
        WindowToken,
        AnyWindowHandle,
        crate::menu::MenuProjection,
        FocusHandle,
    ) {
        (
            self.token.clone(),
            self.window_handle,
            self.menu.clone(),
            self.focus.clone(),
        )
    }
    pub(crate) fn project_menu_focus(&self, window: &Window, cx: &App) {
        if let Some(binding) = crate::menu::project(&self.menu) {
            let snapshot = self.attachment.handle().snapshot();
            if matches!(
                snapshot.state,
                SessionState::Closing | SessionState::Closed | SessionState::CreateFailed
            ) || !self.token.is_live()
            {
                binding.retire();
                return;
            }
            let ready = window.window_handle() == self.window_handle
                && crate::native_view::is_window_effectively_active(window)
                && snapshot.state == SessionState::Alive
                && snapshot.renderer.state != RendererState::Terminated
                && (self.focus.is_focused(window) || binding.owns_focus(window, cx));
            binding.readiness(ready);
        }
    }

    /// Host-owned native service attaches explicitly; this does not retain the
    /// service or extend its hook/lease lifetime beyond the host's scope.
    pub(crate) fn native_keyboard_attached(&mut self, service: &Rc<ServiceState>) {
        self.native_keyboard = Some(Rc::downgrade(service));
    }

    pub(crate) fn native_keyboard_attachment(
        &self,
    ) -> (
        WindowToken,
        FocusHandle,
        rust_cef_core::ports::BrowserHandle,
        AnyWindowHandle,
        Rc<RefCell<InputLane>>,
        Rc<Cell<bool>>,
    ) {
        (
            self.token.clone(),
            self.focus.clone(),
            self.attachment.handle().clone(),
            self.window_handle,
            self.input.clone(),
            self.cef_focus_accepted.clone(),
        )
    }

    pub(crate) fn native_touch_attachment(&self) -> (WindowToken, AnyWindowHandle, BrowserHandle) {
        (
            self.token.clone(),
            self.window_handle,
            self.attachment.handle().clone(),
        )
    }

    pub(crate) fn native_drop_target_attachment(
        &self,
    ) -> (
        WindowToken,
        AnyWindowHandle,
        BrowserHandle,
        Rc<Cell<(f32, f32, f32, f32, f32)>>,
        Option<crate::ole_drag::IntegratedPumpCallback>,
    ) {
        (
            self.token.clone(),
            self.window_handle,
            self.attachment.handle().clone(),
            self.drop_surface_geometry.clone(),
            self.outbound_drag_pump.clone(),
        )
    }

    pub(crate) fn native_touch_event(
        &mut self,
        mut event: BrowserTouchEvent,
        window: &Window,
    ) -> Result<CommandId, ViewError> {
        self.check_window(window)?;
        self.check_live()?;
        if event.event_type == TouchEventType::Pressed
            && !crate::native_view::is_window_effectively_active(window)
        {
            return Err(ViewError::Input(EngineError::NotReady));
        }
        let (bx, by, width, height) = self.paint.borrow().surface_bounds;
        if ![
            bx,
            by,
            width,
            height,
            event.x,
            event.y,
            bx + width,
            by + height,
        ]
        .into_iter()
        .all(f32::is_finite)
        {
            return Err(ViewError::Input(EngineError::InvalidRequest));
        }
        if width <= 0.0 || height <= 0.0 {
            if matches!(
                event.event_type,
                TouchEventType::Pressed | TouchEventType::Moved
            ) {
                return Err(ViewError::Input(EngineError::NotReady));
            }
            event.x = 0.0;
            event.y = 0.0;
        } else {
            if event.event_type == TouchEventType::Pressed
                && (event.x < bx || event.x >= bx + width || event.y < by || event.y >= by + height)
            {
                return Err(ViewError::Input(EngineError::InvalidRequest));
            }
            event.x = (event.x - bx).clamp(0.0, width);
            event.y = (event.y - by).clamp(0.0, height);
        }
        self.attachment
            .handle()
            .submit(BrowserCommand::TouchEvent(event))
            .map_err(ViewError::Input)
    }

    /// Consumes at most one pending frame update from this Browser's
    /// authoritative `FrameSource`.
    ///
    /// Returns `Ok(true)` when a surface image/state changed. The call
    /// rejects a foreign or retired binding before reading the Source; a closed
    /// or hidden surface, a stale generation, or an absent frame returns
    /// `Ok(false)`. New images are charged to the Browser's own CPU budget,
    /// moved into a real `RenderImage`, and replace the old atlas entry in a
    /// transaction: if the old atlas removal fails, the old image and metadata
    /// stay current and the new candidate is dropped.
    ///
    /// If a conversion failed and no new update is pending, one following
    /// explicit call re-reads `snapshot()` once. It never starts a timer, wake
    /// or automatic retry loop.
    pub fn consume(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, ViewError> {
        self.check_window(window)?;
        self.check_live()?;
        self.sync_focus(window, cx);
        self.sync_ux(window, cx);
        self.sync_js(window, cx);
        self.sync_view_events(window, cx);
        // IME state has its own owned snapshot/wake, including Threaded mode
        // with no new frame and no manual pump.
        self.refresh_ime(window, cx);
        // A lifecycle wake may precede a frame. In particular, an attached
        // Creating browser must retry its pure viewport post after Alive.
        cx.notify();

        let result = match poll_source(self.attachment.frame_source().as_ref(), self.needs_resync) {
            SourcePoll::Update(update) => {
                self.consumed_updates = self.consumed_updates.saturating_add(1);
                self.apply_update(window, update)
            }
            SourcePoll::Resync(update) => {
                // A snapshot attempt is not a new CEF frame; it only reconciles
                // missed state and is counted separately.
                self.resync_attempts = self.resync_attempts.saturating_add(1);
                self.apply_update(window, update)
            }
            SourcePoll::Empty => Ok(false),
        };
        // A new CEF frame committed in this consume unlocks any in-flight zoom
        // request and flushes the single latest deferred target immediately.
        self.flush_zoom_gate(window, cx);
        // A single show update must reopen projected readiness in this turn,
        // not depend on another frame/wake arriving after hidden was cleared.
        self.project_menu_focus(window, cx);
        result
    }

    /// Explicitly releases the current atlas entry and image owner.
    ///
    /// Hosts should call this before unmounting the view so the atlas removal
    /// happens while its window is still usable. The frame Source is not read,
    /// and the attachment token keeps the Runtime's close request independent
    /// of this release. This public path requires a live binding; internal
    /// close/release cleanup uses the fixed window id instead.
    pub fn release_image(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), ViewError> {
        self.check_window(window)?;
        self.check_live()?;
        if let Some(b) = crate::menu::project(&self.menu) {
            b.geometry_changed();
        }
        self.native_ime.invalidate();
        self.ime_request = None;
        self.click.borrow_mut().cancel();
        self.needs_resync = false;
        let invalidate = self.popup_visible && self.popup_image.is_some();
        self.popup_visible = false;
        self.popup_invalid = true;
        let revision = if invalidate {
            self.advance_binding()
        } else {
            Ok(())
        };
        let view_result = self.clear_image(window);
        let popup_result = self.clear_popup(window);
        let result = revision.and(view_result).and(popup_result);
        if result.is_err() && self.token.is_live() {
            // A failed atlas removal kept the old slot; one later explicit
            // consume may reconcile again. Nothing wakes automatically.
            self.needs_resync = true;
        }
        cx.notify();
        result
    }

    /// Validation-only, one-shot sampling after an already painted VIEW.
    #[cfg(feature = "validation")]
    pub fn probe_arm_view_paint_timing(&mut self, generation: u64, sequence: u64) -> bool {
        let mut paint = self.paint.borrow_mut();
        if !self.token.is_live()
            || paint.view_timing.is_some()
            || generation == 0
            || sequence == 0
            || paint.painted_generation != generation
            || paint.painted_sequence != sequence
        {
            return false;
        }
        paint.view_timing = Some(ViewPaintTiming {
            generation,
            last_sequence: sequence,
            samples: Vec::with_capacity(crate::test_support::VIEW_COPY_SAMPLE_CAPACITY),
        });
        true
    }

    /// Validation-only query before waiting for paint. A newer FrameSource
    /// publication does not by itself mean the actual View has consumed it.
    #[cfg(feature = "validation")]
    pub fn probe_has_view_image(&self, generation: u64, sequence: u64) -> bool {
        self.token.is_live()
            && self
                .image
                .as_ref()
                .is_some_and(|image| image.generation == generation && image.sequence == sequence)
    }

    /// Validation-only owned stamps from the original live paint binding.
    #[cfg(feature = "validation")]
    pub fn probe_view_paint_samples(&self) -> Option<Vec<ViewPaintSample>> {
        self.paint
            .borrow()
            .view_timing
            .as_ref()
            .map(|timing| timing.samples.clone())
    }

    /// Snapshot of frame/binding counters and the real Runtime input ledger.
    pub fn diagnostics(&self) -> ViewDiagnostics {
        let paint = self.paint.borrow();
        let (frame_width, frame_height) = self
            .image
            .as_ref()
            .map_or((0, 0), |slot| (slot.width, slot.height));
        ViewDiagnostics {
            consumed_updates: self.consumed_updates,
            presses_received: self.presses_received,
            presses_forwarded: self.presses_forwarded,
            created_images: self.created_images,
            released_images: self.released_images,
            paint_calls: paint.paint_calls,
            painted_sequence: paint.painted_sequence,
            painted_generation: paint.painted_generation,
            frame_width,
            frame_height,
            surface_bounds: paint.surface_bounds,
            scale_factor: paint.scale_factor,
            last_error: paint.last_error.clone(),
            binding_retired: !self.token.is_live(),
            binding_revision: self.binding_revision,
            resync_attempts: self.resync_attempts,
            rejected_paints: paint.rejected_paints,
            pending_resync: self.needs_resync,
            clicks_submitted: self.input.borrow().accepted_clicks(),
            clicks_rejected: self.input.borrow().rejected_clicks(),
            pending_click: self.click.borrow().pending(),
            input: self.input.borrow().diagnostics(self.attachment.handle()),
            ime: self.native_ime.diagnostics(),
            zoom_level_milli: self.ux.zoom_level_milli,
            zoom_gate: self.zoom_gate.stats(),
            popup: PopupDiagnostics {
                generation: self.popup_generation,
                visible: self.popup_visible,
                bounds: self.popup_bounds,
                frame_width: self.popup_image.as_ref().map_or(0, |slot| slot.width),
                frame_height: self.popup_image.as_ref().map_or(0, |slot| slot.height),
                created_images: self.popup_created,
                released_images: self.popup_released,
                paint_calls: paint.popup_calls,
                painted_sequence: paint.popup_sequence,
                painted_generation: paint.popup_generation,
                painted_bounds: paint.popup_bounds,
            },
        }
    }

    /// Clone of the UI-thread binding token for composition routing.
    pub(crate) fn window_token(&self) -> WindowToken {
        self.token.clone()
    }

    /// Whether an explicit follow-up `consume` should snapshot-retry once.
    pub(crate) fn needs_resync(&self) -> bool {
        self.needs_resync
    }

    pub fn cursor(&self) -> BrowserCursor {
        self.ux.cursor
    }

    pub fn native_cursor(&self) -> &rust_cef_core::cursor::NativeCursorData {
        &self.native_cursor
    }

    fn surface_client_rect_px(&self) -> (i32, i32, i32, i32) {
        let paint = self.paint.borrow();
        let scale = paint.scale_factor.max(1.0);
        surface_bounds_to_px(paint.surface_bounds, scale)
    }

    pub fn tooltip(&self) -> &str {
        &self.ux.tooltip
    }

    pub fn status_text(&self) -> &str {
        &self.ux.status_text
    }

    pub fn favicon_urls(&self) -> &[String] {
        &self.ux.favicon_urls
    }

    pub fn fullscreen(&self) -> bool {
        self.ux.fullscreen
    }

    pub fn loading_progress(&self) -> f64 {
        f64::from(self.ux.loading_progress_per_mille) / 1000.0
    }

    pub fn zoom_level(&self) -> f64 {
        f64::from(self.ux.zoom_level_milli) / 1000.0
    }

    pub fn zoom_gate_stats(&self) -> ZoomGateStats {
        self.zoom_gate.stats()
    }

    pub fn audio_muted(&self) -> bool {
        self.ux.audio_muted
    }

    pub fn has_active_drag(&self) -> bool {
        self.active_drag.is_some()
    }

    pub fn active_drag_cursor_op(&self) -> Option<DragOperationMask> {
        self.active_drag.as_ref().map(|d| d.current_op)
    }

    pub fn has_active_drag_preview(&self) -> bool {
        self.active_drag
            .as_ref()
            .is_some_and(|d| d.preview_image.is_some() || d.fallback_label.is_some())
    }

    pub fn is_left_mouse_down(&self) -> bool {
        self.left_mouse_down
    }

    pub fn handle(&self) -> &BrowserHandle {
        self.attachment.handle()
    }

    fn clear_callbacks_and_filters(&mut self) {
        self.on_take_focus = None;
        self.on_shortcut = None;
        self.on_js_message = None;
        self.on_eval_completed = None;
        self.set_on_popup(None);
        self.set_on_address_changed(None);
        self.callbacks.clear_all();
        clear_routing_and_filters(self.attachment.handle());
    }

    crate::events::impl_view_surface_api!();

    pub fn set_on_popup(&mut self, handler: Option<PopupCallback>) {
        self.on_popup_epoch = self.on_popup_epoch.wrapping_add(1);
        self.attachment
            .handle()
            .set_popup_ui_listener_active(handler.is_some());
        self.on_popup = handler;
    }

    pub fn set_on_address_changed(&mut self, handler: Option<NavigationCallback>) {
        self.on_address_epoch = self.on_address_epoch.wrapping_add(1);
        self.on_address_changed = handler;
    }

    pub fn set_on_take_focus(&mut self, handler: Option<TakeFocusCallback>) {
        self.on_take_focus = handler;
    }

    pub fn set_on_shortcut(&mut self, handler: Option<ShortcutCallback>) {
        self.on_shortcut = handler;
    }

    pub fn set_on_js_message(&mut self, handler: Option<JsMessageCallback>) {
        self.on_js_message = handler;
    }

    pub fn set_on_eval_completed(&mut self, handler: Option<EvalCompletedCallback>) {
        self.on_eval_completed = handler;
    }

    pub fn execute_javascript(
        &self,
        script: impl Into<String>,
        frame_scope: JsFrameScope,
    ) -> Result<CommandId, ViewError> {
        if !self.token.is_live() {
            return Err(ViewError::RetiredBinding);
        }
        self.attachment
            .handle()
            .submit(BrowserCommand::ExecuteJavaScript {
                script: script.into(),
                frame_scope,
            })
            .map_err(ViewError::Input)
    }

    pub fn evaluate_script(
        &self,
        script: impl Into<String>,
        frame_scope: JsFrameScope,
        timeout_ms: u64,
    ) -> Result<EvalRequestId, ViewError> {
        if !self.token.is_live() {
            return Err(ViewError::RetiredBinding);
        }
        self.attachment
            .handle()
            .evaluate_script(script.into(), frame_scope, timeout_ms)
            .map_err(ViewError::Input)
    }

    pub fn evaluate_script_async(
        &self,
        script: impl Into<String>,
        frame_scope: JsFrameScope,
        timeout: Option<Duration>,
    ) -> crate::async_ops::AsyncOpFuture<Option<String>> {
        if !self.token.is_live() {
            return crate::async_ops::AsyncOpFuture::ready(Err(
                crate::async_ops::AsyncOpError::Closed,
            ));
        }
        let dur = timeout.unwrap_or(crate::async_ops::DEFAULT_ASYNC_OP_TIMEOUT);
        let timeout_ms = dur.as_millis().clamp(1, i64::MAX as u128) as u64;
        let id =
            match self
                .attachment
                .handle()
                .evaluate_script(script.into(), frame_scope, timeout_ms)
            {
                Ok(id) => id,
                Err(err) => {
                    return crate::async_ops::AsyncOpFuture::ready(Err(
                        crate::async_ops::AsyncOpError::Runtime(err),
                    ));
                }
            };
        match self.async_ops.begin_eval(id, Some(dur)) {
            Ok(fut) => fut,
            Err(err) => crate::async_ops::AsyncOpFuture::ready(Err(err)),
        }
    }

    pub fn get_source_async(
        &self,
        frame_scope: JsFrameScope,
        timeout: Option<Duration>,
    ) -> crate::async_ops::AsyncOpFuture<String> {
        if !self.token.is_live() {
            return crate::async_ops::AsyncOpFuture::ready(Err(
                crate::async_ops::AsyncOpError::Closed,
            ));
        }
        crate::async_ops::request_frame_text_async(
            &self.async_ops,
            self.attachment.runtime_bridge(),
            rust_cef_runtime::FrameTextKind::Source,
            frame_scope,
            timeout,
        )
    }

    pub fn get_text_async(
        &self,
        frame_scope: JsFrameScope,
        timeout: Option<Duration>,
    ) -> crate::async_ops::AsyncOpFuture<String> {
        if !self.token.is_live() {
            return crate::async_ops::AsyncOpFuture::ready(Err(
                crate::async_ops::AsyncOpError::Closed,
            ));
        }
        crate::async_ops::request_frame_text_async(
            &self.async_ops,
            self.attachment.runtime_bridge(),
            rust_cef_runtime::FrameTextKind::Text,
            frame_scope,
            timeout,
        )
    }

    pub fn wait_for_initial_load_async(
        &self,
        timeout: Option<Duration>,
    ) -> crate::async_ops::AsyncOpFuture<crate::async_ops::InitialLoadResult> {
        if !self.token.is_live() {
            return crate::async_ops::AsyncOpFuture::ready(Err(
                crate::async_ops::AsyncOpError::Closed,
            ));
        }
        let nav = self.attachment.handle().snapshot().navigation;
        let hint = (!nav.is_loading && !nav.address.is_empty()).then_some(
            crate::async_ops::InitialLoadResult {
                http_status_code: 200,
            },
        );
        self.async_ops.wait_for_initial_load(
            hint,
            Some(timeout.unwrap_or(crate::async_ops::DEFAULT_ASYNC_OP_TIMEOUT)),
        )
    }

    fn latest_osr_frame(&self) -> Option<Arc<OwnedFrame>> {
        if let Some(slot) = self.image.as_ref()
            && let PresentedContent::GpuTexture(ref frame) = slot.content
        {
            return Some(Arc::clone(frame));
        }
        self.attachment.frame_source().snapshot().view.frame
    }

    pub fn capture_screenshot_bgra(
        &self,
        timeout: Option<Duration>,
    ) -> crate::async_ops::AsyncOpFuture<crate::async_ops::ScreenshotBgra> {
        if !self.token.is_live() {
            return crate::async_ops::AsyncOpFuture::ready(Err(
                crate::async_ops::AsyncOpError::Closed,
            ));
        }
        crate::async_ops::capture_osr_bgra(
            &self.async_ops,
            self.attachment.runtime_bridge(),
            self.latest_osr_frame(),
            timeout,
        )
    }

    pub fn capture_screenshot_png(
        &self,
        timeout: Option<Duration>,
    ) -> crate::async_ops::AsyncOpFuture<Vec<u8>> {
        if !self.token.is_live() {
            return crate::async_ops::AsyncOpFuture::ready(Err(
                crate::async_ops::AsyncOpError::Closed,
            ));
        }
        crate::async_ops::capture_osr_png(
            &self.async_ops,
            self.attachment.runtime_bridge(),
            self.latest_osr_frame(),
            timeout,
        )
    }

    pub fn post_js_message(&self, payload_json: impl Into<String>) -> Result<CommandId, ViewError> {
        if !self.token.is_live() {
            return Err(ViewError::RetiredBinding);
        }
        self.attachment
            .handle()
            .submit(BrowserCommand::PostJsMessage {
                payload_json: payload_json.into(),
            })
            .map_err(ViewError::Input)
    }

    pub fn set_zoom_level(&mut self, level: f64) -> Result<CommandId, ViewError> {
        self.check_live()?;
        let cmd_id = self
            .attachment
            .handle()
            .submit(BrowserCommand::SetZoomLevel(level))
            .map_err(ViewError::Input)?;
        if let Ok(milli) = zoom_level_to_milli(level) {
            let now = Instant::now();
            let clamped = level.clamp(MIN_ZOOM_LEVEL, MAX_ZOOM_LEVEL);
            self.zoom_timeout_task = None;
            self.zoom_gate
                .commit_sent(clamped, self.latest_view_frame_seq, now, None);
            self.ux.zoom_level_milli = milli;
            self.pending_zoom = Some(PendingZoom {
                target_milli: milli,
                submitted_at: now,
            });
        }
        Ok(cmd_id)
    }

    pub fn step_zoom(&mut self, steps: i32, window: &mut Window, cx: &mut Context<Self>) {
        if self.check_window(window).is_err() || self.check_live().is_err() {
            return;
        }
        let base = if self.zoom_gate.is_busy() {
            self.zoom_gate.desired_zoom_level()
        } else {
            self.zoom_level()
        };
        let next = step_canonical_zoom_level(base, steps);
        let _ = self.apply_target_zoom(next, window, cx);
    }

    pub fn reset_zoom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.check_window(window).is_err() || self.check_live().is_err() {
            return;
        }
        let _ = self.apply_target_zoom(0.0, window, cx);
    }

    fn apply_target_zoom(
        &mut self,
        next_level: f64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let clamped = if next_level.is_finite() {
            next_level.clamp(MIN_ZOOM_LEVEL, MAX_ZOOM_LEVEL)
        } else {
            return false;
        };
        let Ok(milli) = zoom_level_to_milli(clamped) else {
            return false;
        };
        let now = Instant::now();
        if let Some(level_to_send) =
            self.zoom_gate
                .plan_request(clamped, self.latest_view_frame_seq, now)
        {
            if self
                .attachment
                .handle()
                .submit_zoom_level_fast(level_to_send)
                .is_err()
            {
                return false;
            }
            self.zoom_timeout_task = None;
            self.zoom_gate
                .commit_sent(level_to_send, self.latest_view_frame_seq, now, None);
        } else if self.zoom_gate.has_deferred_tail() {
            self.arm_zoom_timeout_timer(now, window, cx);
        } else {
            self.zoom_timeout_task = None;
        }
        self.ux.zoom_level_milli = milli;
        self.pending_zoom = Some(PendingZoom {
            target_milli: milli,
            submitted_at: now,
        });
        cx.notify();
        window.refresh();
        true
    }

    fn arm_zoom_timeout_timer(
        &mut self,
        now: Instant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.zoom_timeout_task.is_some() {
            return;
        }
        let Some(sent_at) = self.zoom_gate.in_flight_since() else {
            return;
        };
        let remaining = ZOOM_IN_FLIGHT_TIMEOUT
            .saturating_sub(now.saturating_duration_since(sent_at))
            .saturating_add(Duration::from_millis(2));
        self.zoom_timeout_task = Some(cx.spawn_in(window, async move |weak, cx| {
            cx.background_executor().timer(remaining).await;
            let _ = cx.update(|window, app| {
                let _ = weak.update(app, |view, cx| {
                    view.zoom_timeout_task = None;
                    view.flush_zoom_gate(window, cx);
                });
            });
        }));
    }

    fn flush_zoom_gate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.check_window(window).is_err() || self.check_live().is_err() {
            self.zoom_timeout_task = None;
            self.zoom_gate.clear_in_flight();
            return;
        }
        let now = Instant::now();
        match self.zoom_gate.poll_flush(self.latest_view_frame_seq, now) {
            ZoomFlushAction::Idle => {}
            ZoomFlushAction::UnlockedNoTail => {
                self.zoom_timeout_task = None;
            }
            ZoomFlushAction::Dispatch { level, via_timeout } => {
                self.zoom_timeout_task = None;
                if self
                    .attachment
                    .handle()
                    .submit_zoom_level_fast(level)
                    .is_ok()
                {
                    self.zoom_gate.commit_sent(
                        level,
                        self.latest_view_frame_seq,
                        now,
                        Some(via_timeout),
                    );
                    if let Ok(milli) = zoom_level_to_milli(level) {
                        self.ux.zoom_level_milli = milli;
                        self.pending_zoom = Some(PendingZoom {
                            target_milli: milli,
                            submitted_at: now,
                        });
                    }
                    cx.notify();
                    window.refresh();
                } else {
                    self.zoom_gate.abort_deferred_tail();
                    self.pending_zoom = None;
                }
            }
        }
    }

    pub fn set_audio_muted(&mut self, muted: bool) -> Result<CommandId, ViewError> {
        self.check_live()?;
        self.attachment
            .handle()
            .submit(BrowserCommand::SetAudioMuted(muted))
            .map_err(ViewError::Input)
    }

    pub fn set_outbound_drag_pump(
        &mut self,
        pump: Option<crate::ole_drag::IntegratedPumpCallback>,
    ) {
        self.outbound_drag_pump = pump;
    }

    pub fn set_on_outbound_drag(&mut self, handler: Option<OutboundDragCallback>) {
        self.on_outbound_drag = handler;
    }

    pub fn finish_outbound_drag(
        &mut self,
        finish: OutboundDragFinish,
    ) -> Result<CommandId, ViewError> {
        self.check_live()?;
        let _ = self.outbound_drag.finish_in_flight(
            finish.drag_id,
            finish.x,
            finish.y,
            finish.operation,
        );
        self.input.borrow_mut().clear_held_buttons();
        self.click.borrow_mut().cancel();
        self.attachment
            .handle()
            .submit(BrowserCommand::FinishOutboundDrag(finish))
            .map_err(ViewError::Input)
    }

    pub fn set_accessibility_state(
        &mut self,
        state: CefFeatureState,
    ) -> Result<CommandId, ViewError> {
        self.check_live()?;
        if state != CefFeatureState::Enabled {
            self.ax_snapshot = None;
            self.attachment.ax_tree().clear();
        }
        self.attachment
            .handle()
            .submit(BrowserCommand::SetAccessibilityState(state))
            .map_err(ViewError::Input)
    }

    pub fn accessibility_snapshot(&self) -> Option<&AxTreeSnapshot> {
        self.ax_snapshot.as_ref()
    }

    pub fn set_pinch_zoom_enabled(&mut self, enabled: bool) {
        self.pinch_zoom_enabled = enabled;
    }

    pub fn pinch_zoom_enabled(&self) -> bool {
        self.pinch_zoom_enabled
    }

    pub fn set_ctrl_wheel_zoom_enabled(&mut self, enabled: bool) {
        self.ctrl_wheel_zoom_enabled = enabled;
    }

    pub fn ctrl_wheel_zoom_enabled(&self) -> bool {
        self.ctrl_wheel_zoom_enabled
    }

    pub fn set_surface_opacity(&mut self, opacity: f32) {
        self.surface_opacity = if opacity.is_finite() {
            opacity.clamp(0.0, 1.0)
        } else {
            1.0
        };
    }

    pub fn surface_opacity(&self) -> f32 {
        self.surface_opacity
    }

    pub fn set_rotation_angle_deg(&mut self, degrees: f32) {
        self.rotation_angle_deg = if degrees.is_finite() {
            degrees.clamp(-180.0, 180.0)
        } else {
            0.0
        };
    }

    pub fn rotation_angle_deg(&self) -> f32 {
        self.rotation_angle_deg
    }

    pub fn set_nearest_neighbor_scaling(&mut self, nearest: bool) {
        self.nearest_neighbor_scaling = nearest;
    }

    pub fn nearest_neighbor_scaling(&self) -> bool {
        self.nearest_neighbor_scaling
    }

    pub fn set_fit_rotated_to_bounds(&mut self, fit: bool) {
        self.fit_rotated_to_bounds = fit;
    }

    pub fn fit_rotated_to_bounds(&self) -> bool {
        self.fit_rotated_to_bounds
    }

    pub fn set_transparent_backdrop(&mut self, transparent: bool) {
        self.transparent_backdrop = transparent;
    }

    pub fn transparent_backdrop(&self) -> bool {
        self.transparent_backdrop
    }

    pub fn hit_test_surface_point(&self, position_dip: (f32, f32)) -> Option<(i32, i32)> {
        translate_point_transformed(
            self.paint.borrow().surface_bounds,
            position_dip,
            self.rotation_angle_deg,
            self.fit_rotated_to_bounds,
            false,
        )
    }

    pub fn send_touch_event(&mut self, event: BrowserTouchEvent) -> Result<CommandId, ViewError> {
        self.check_live()?;
        let event = event.validate_and_normalize().map_err(ViewError::Input)?;
        self.attachment
            .handle()
            .submit(BrowserCommand::TouchEvent(event))
            .map_err(ViewError::Input)
    }

    fn cancel_active_osr_drag_without_window(&mut self) -> Option<Arc<RenderImage>> {
        self.left_mouse_down = false;
        self.active_drag_flag.set(false);
        let Some(mut drag) = self.active_drag.take() else {
            return None;
        };
        let _ = self.outbound_drag.finish_in_flight(
            drag.drag_id,
            drag.last_view_point.0,
            drag.last_view_point.1,
            DragOperationMask::NONE,
        );
        let _ = self
            .attachment
            .handle()
            .submit(BrowserCommand::FinishOsrDrag(OsrDragFinish {
                drag_id: drag.drag_id,
                x: drag.last_view_point.0,
                y: drag.last_view_point.1,
                modifiers: MouseModifiers::default(),
                allow_drop: false,
            }));
        drag.preview_image.take()
    }

    fn finish_active_osr_drag(
        &mut self,
        position: Point<Pixels>,
        modifiers: gpui::Modifiers,
        cancelled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.left_mouse_down = false;
        self.active_drag_flag.set(false);
        let Some(mut drag) = self.active_drag.take() else {
            return;
        };
        let inside_point = self.point(position, false);
        let (vx, vy) = inside_point
            .or_else(|| self.point(position, true))
            .unwrap_or(drag.last_view_point);
        let allow_drop =
            !cancelled && inside_point.is_some() && drag.current_op != DragOperationMask::NONE;
        let final_op = if allow_drop {
            drag.current_op.preferred_single_op()
        } else {
            DragOperationMask::NONE
        };
        let _ = self
            .outbound_drag
            .finish_in_flight(drag.drag_id, vx, vy, final_op);
        let _ = self
            .attachment
            .handle()
            .submit(BrowserCommand::FinishOsrDrag(OsrDragFinish {
                drag_id: drag.drag_id,
                x: vx,
                y: vy,
                modifiers: mouse_modifiers(modifiers),
                allow_drop,
            }));
        if let Some(img) = drag.preview_image.take() {
            let _ = drop_atlas_image(window, img);
        }
        self.input.borrow_mut().clear_held_buttons();
        self.click.borrow_mut().cancel();
        crate::native_view::set_window_drag_cursor(window, None);
        crate::native_view::update_surface_cursor(
            window,
            surface_cursor_key(self.browser),
            &self.native_cursor,
            self.surface_client_rect_px(),
            self.pointer_over_surface,
        );
        window.refresh();
        cx.notify();
    }

    fn cancel_active_osr_drag_in_window(&mut self, window: &mut Window) {
        let img = self.cancel_active_osr_drag_without_window();
        if let Some(img) = img {
            let _ = drop_atlas_image(window, img);
        }
        self.input.borrow_mut().clear_held_buttons();
        self.click.borrow_mut().cancel();
        crate::native_view::set_window_drag_cursor(window, None);
        crate::native_view::update_surface_cursor(
            window,
            surface_cursor_key(self.browser),
            &self.native_cursor,
            self.surface_client_rect_px(),
            self.pointer_over_surface,
        );
        window.refresh();
    }

    fn cancel_outbound_drags(&mut self) {
        self.outbound_drag_task.take();
        let _ = self.cancel_active_osr_drag_without_window();
        for finish in self.outbound_drag.cancel_all() {
            let _ = self
                .attachment
                .handle()
                .submit(BrowserCommand::FinishOutboundDrag(finish));
        }
    }

    fn pointer_pinch(&mut self, event: &PinchEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.pinch_zoom_enabled
            || self.check_window(window).is_err()
            || self.check_live().is_err()
        {
            return;
        }
        match event.phase {
            TouchPhase::Started | TouchPhase::Ended | TouchPhase::Cancelled => {}
            TouchPhase::Moved => {
                let base = if self.zoom_gate.is_busy() {
                    self.zoom_gate.desired_zoom_level()
                } else {
                    self.zoom_level()
                };
                if let Some(next) = compute_pinch_zoom(base, event.delta) {
                    let _ = self.apply_target_zoom(next, window, cx);
                }
            }
        }
    }

    fn sync_ux(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = Instant::now();
        let nav = self.attachment.handle().snapshot().navigation;
        let prev_zoom_milli = self.ux.zoom_level_milli;
        self.ux.apply_snapshot(&nav);
        let _ = self
            .zoom_gate
            .expire_if_ready(self.latest_view_frame_seq, now);
        if self.zoom_gate.is_busy() {
            if let Ok(desired_milli) = zoom_level_to_milli(self.zoom_gate.desired_zoom_level()) {
                self.ux.zoom_level_milli = desired_milli;
            }
        } else {
            reconcile_zoom_from_snapshot(
                nav.zoom_level_milli,
                now,
                &mut self.ux,
                &mut self.pending_zoom,
            );
            if self.pending_zoom.is_none() {
                self.zoom_gate
                    .sync_external(f64::from(self.ux.zoom_level_milli) / 1000.0);
            }
        }
        let events = self.attachment.handle().take_ux_events();
        let mut focus_taken = Vec::new();
        let mut shortcuts = Vec::new();
        let mut addresses = Vec::new();
        let mut popups = Vec::new();
        let mut a11y_updated = false;
        for event in events {
            match event {
                BrowserEvent::StartOutboundDrag(start) => {
                    if let Some(cancel) = self.outbound_drag.enqueue(start) {
                        let _ = self
                            .attachment
                            .handle()
                            .submit(BrowserCommand::FinishOutboundDrag(cancel));
                    }
                }
                BrowserEvent::OutboundDragCursorChanged(op) => {
                    self.outbound_drag.update_cursor(op);
                    if let Some(drag) = self.active_drag.as_mut() {
                        let next_op = if drag.in_view {
                            DragOperationMask(op.bits() & drag.allowed_ops.bits())
                        } else {
                            DragOperationMask::NONE
                        };
                        if drag.current_op != next_op {
                            drag.current_op = next_op;
                            crate::native_view::set_window_drag_cursor(window, Some(next_op));
                            window.refresh();
                            cx.notify();
                        }
                    }
                }
                BrowserEvent::AccessibilityTreeUpdated { .. } => {
                    a11y_updated = true;
                }
                BrowserEvent::PopupRequested(req) => {
                    popups.push(req);
                }
                BrowserEvent::ZoomLevelChanged(milli) => {
                    if self.zoom_gate.is_busy() {
                        if let Ok(desired_milli) =
                            zoom_level_to_milli(self.zoom_gate.desired_zoom_level())
                        {
                            self.ux.zoom_level_milli = desired_milli;
                        }
                    } else if let Some(pending) = self.pending_zoom {
                        if (milli - pending.target_milli).abs() <= 2 {
                            self.ux.zoom_level_milli = milli;
                            self.pending_zoom = None;
                            self.zoom_gate.sync_external(f64::from(milli) / 1000.0);
                        }
                    } else {
                        self.ux.zoom_level_milli = milli;
                        self.zoom_gate.sync_external(f64::from(milli) / 1000.0);
                    }
                }
                other => {
                    if matches!(
                        other,
                        BrowserEvent::LoadStarted {
                            is_main_frame: true
                        }
                    ) {
                        self.last_hover_dip = None;
                        self.ux.tooltip.clear();
                        self.scroll_accumulator.reset();
                        self.ctrl_wheel_zoom_notches = 0.0;
                        self.zoom_timeout_task = None;
                        self.zoom_gate.clear_in_flight();
                    }
                    if let BrowserEvent::AddressChanged(ref addr) = other {
                        addresses.push(addr.clone());
                    }
                    let (focus, shortcut) = self.ux.apply_event(other);
                    if let Some(next) = focus {
                        focus_taken.push(next);
                    }
                    if let Some(id) = shortcut {
                        shortcuts.push(id);
                    }
                }
            }
        }
        if self.ux.zoom_level_milli != prev_zoom_milli {
            window.refresh();
        }
        let latest_ax = self.attachment.ax_tree().snapshot();
        if a11y_updated
            || latest_ax.as_ref().map(|s| s.generation)
                != self.ax_snapshot.as_ref().map(|s| s.generation)
        {
            self.ax_snapshot = latest_ax;
        }
        if !focus_taken.is_empty()
            && let Some(mut cb) = self.on_take_focus.take()
        {
            for next in focus_taken {
                cb(next, self, window, cx);
            }
            if self.on_take_focus.is_none() {
                self.on_take_focus = Some(cb);
            }
        }
        if !shortcuts.is_empty()
            && let Some(mut cb) = self.on_shortcut.take()
        {
            for id in shortcuts {
                cb(id, self, window, cx);
            }
            if self.on_shortcut.is_none() {
                self.on_shortcut = Some(cb);
            }
        }
        if !addresses.is_empty()
            && let Some(mut cb) = self.on_address_changed.take()
        {
            let epoch = self.on_address_epoch;
            for addr in addresses {
                cb(addr, self, window, cx);
            }
            if self.on_address_epoch == epoch && self.on_address_changed.is_none() {
                self.on_address_changed = Some(cb);
            }
        }
        if !popups.is_empty()
            && let Some(mut cb) = self.on_popup.take()
        {
            let epoch = self.on_popup_epoch;
            for req in popups {
                cb(req, self, window, cx);
            }
            if self.on_popup_epoch == epoch && self.on_popup.is_none() {
                self.on_popup = Some(cb);
            }
        }
        if let Some(start) = self.outbound_drag.take_pending() {
            if let Some(mut cb) = self.on_outbound_drag.take() {
                let result = cb(start, self, window, cx);
                if self.on_outbound_drag.is_none() {
                    self.on_outbound_drag = Some(cb);
                }
                if let Some(finish) = result {
                    let _ = self.outbound_drag.finish_in_flight(
                        finish.drag_id,
                        finish.x,
                        finish.y,
                        finish.operation,
                    );
                    self.left_mouse_down = false;
                    self.input.borrow_mut().clear_held_buttons();
                    self.click.borrow_mut().cancel();
                    let _ = self
                        .attachment
                        .handle()
                        .submit(BrowserCommand::FinishOutboundDrag(finish));
                }
            } else {
                self.outbound_drag_task.take();
                self.input.borrow_mut().clear_held_buttons();
                self.click.borrow_mut().cancel();
                let cursor_win_dip = self.last_pointer_win_dip.unwrap_or_else(|| {
                    let pos = window.mouse_position();
                    (f32::from(pos.x), f32::from(pos.y))
                });
                let cursor_pos = gpui::point(px(cursor_win_dip.0), px(cursor_win_dip.1));
                let inside_pt = self.point(cursor_pos, false);
                let last_view_point = inside_pt
                    .or_else(|| self.point(cursor_pos, true))
                    .unwrap_or((start.x, start.y));
                let was_left_down = std::mem::replace(&mut self.left_mouse_down, false);
                let os_button_down = crate::ole_drag::is_os_drag_button_down();
                if os_button_down {
                    // Interactive OS drag: hand off to Windows COM OLE `DoDragDrop` + `SHDRAGIMAGE`
                    // so the drag preview floats freely beyond window edges and external apps
                    // receive real OLE drops, while `ComDropTarget` (`ole_drop.rs`) handles
                    // intra-window CEF `DragTarget*` feedback & drop.
                    if let Some(old) = self.active_drag.take()
                        && let Some(old_img) = old.preview_image
                    {
                        let _ = drop_atlas_image(window, old_img);
                    }
                    crate::native_view::set_window_drag_cursor(window, None);
                    self.active_drag_flag.set(true);
                    let hwnd = crate::native_view::window_hwnd(window)
                        .map(|h| h.get() as *mut std::ffi::c_void);
                    let surface_geom = Some(self.drop_surface_geometry.get());
                    let pump = self.outbound_drag_pump.clone();
                    let handle = self.attachment.handle().clone();
                    let drag_flag = self.active_drag_flag.clone();
                    self.outbound_drag_task = Some(cx.spawn_in(window, async move |weak, cx| {
                        // Yield once so `consume_ux_events` releases all GPUI App/Entity
                        // borrows before `DoDragDrop` enters the Win32 modal loop.
                        cx.background_executor().timer(Duration::ZERO).await;
                        let finish = if crate::ole_drag::is_os_drag_button_down() {
                            crate::ole_drag::run_ole_drag_in_surface(
                                &start,
                                hwnd,
                                surface_geom,
                                pump,
                            )
                        } else {
                            OutboundDragFinish {
                                drag_id: start.drag_id,
                                x: start.x,
                                y: start.y,
                                operation: DragOperationMask::NONE,
                            }
                        };
                        drag_flag.set(false);
                        let _ = cx.update(|window, app| {
                            let _ = weak.update(app, |view, cx| {
                                view.left_mouse_down = false;
                                view.input.borrow_mut().clear_held_buttons();
                                view.click.borrow_mut().cancel();
                                let _ = view.outbound_drag.finish_in_flight(
                                    finish.drag_id,
                                    finish.x,
                                    finish.y,
                                    finish.operation,
                                );
                                view.outbound_drag_task = None;
                                crate::native_view::set_window_drag_cursor(window, None);
                                crate::native_view::update_surface_cursor(
                                    window,
                                    surface_cursor_key(view.browser),
                                    &view.native_cursor,
                                    view.surface_client_rect_px(),
                                    view.pointer_over_surface,
                                );
                                window.refresh();
                                cx.notify();
                            });
                        });
                        let _ = handle.submit(BrowserCommand::FinishOutboundDrag(finish));
                    }));
                } else if !was_left_down {
                    let _ = self.outbound_drag.finish_in_flight(
                        start.drag_id,
                        last_view_point.0,
                        last_view_point.1,
                        DragOperationMask::NONE,
                    );
                    let _ = self
                        .attachment
                        .handle()
                        .submit(BrowserCommand::FinishOutboundDrag(OutboundDragFinish {
                            drag_id: start.drag_id,
                            x: last_view_point.0,
                            y: last_view_point.1,
                            operation: DragOperationMask::NONE,
                        }));
                } else {
                    let in_view = inside_pt.is_some();
                    let current_op = if in_view {
                        DragOperationMask(
                            self.outbound_drag.current_cursor().bits() & start.allowed_ops.bits(),
                        )
                    } else {
                        DragOperationMask::NONE
                    };
                    let (preview_image, preview_size_dip, hotspot_dip, fallback_label) =
                        drag_preview_from_start(&start, window.scale_factor());
                    if let Some(old) = self.active_drag.take()
                        && let Some(old_img) = old.preview_image
                    {
                        let _ = drop_atlas_image(window, old_img);
                    }
                    self.active_drag = Some(ActiveCefDrag {
                        drag_id: start.drag_id,
                        allowed_ops: start.allowed_ops,
                        current_op,
                        in_view,
                        cursor_win_dip,
                        last_view_point,
                        preview_image,
                        preview_size_dip,
                        hotspot_dip,
                        fallback_label,
                    });
                    self.active_drag_flag.set(true);
                    let _ = self.attachment.handle().submit_osr_drag_move_fast(
                        start.drag_id,
                        last_view_point.0,
                        last_view_point.1,
                        MouseModifiers::default(),
                        in_view,
                    );
                    crate::native_view::set_window_drag_cursor(window, Some(current_op));
                    window.refresh();
                    cx.notify();
                }
            }
        }
        let (cursor_rev, native_cursor) = self.attachment.cursor_slot().snapshot();
        if cursor_rev != self.native_cursor_revision || self.native_cursor != native_cursor {
            self.native_cursor_revision = cursor_rev;
            self.native_cursor = native_cursor;
            if self.active_drag.is_none() {
                let key = surface_cursor_key(self.browser);
                let rect_px = self.surface_client_rect_px();
                crate::native_view::update_surface_cursor(
                    window,
                    key,
                    &self.native_cursor,
                    rect_px,
                    self.pointer_over_surface,
                );
            }
        }
    }

    fn sync_js(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let events = self.attachment.handle().take_js_events();
        for event in events {
            match event {
                BrowserEvent::JsMessageReceived {
                    origin,
                    payload_json,
                } => {
                    if let Some(mut cb) = self.on_js_message.take() {
                        cb(origin, payload_json, self, window, cx);
                        if self.on_js_message.is_none() {
                            self.on_js_message = Some(cb);
                        }
                    }
                }
                BrowserEvent::EvalCompleted { id, outcome } => {
                    let _ = self.async_ops.on_eval_completed(id, &outcome);
                    if let Some(mut cb) = self.on_eval_completed.take() {
                        cb(id, outcome, self, window, cx);
                        if self.on_eval_completed.is_none() {
                            self.on_eval_completed = Some(cb);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn callbacks_mut(&mut self) -> &mut BrowserViewCallbacks<Self> {
        &mut self.callbacks
    }

    fn sync_view_events(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let events = self.attachment.handle().take_view_events();
        for event in events {
            self.async_ops.on_browser_event(&event);
            if let BrowserEvent::DraggableRegionsChanged(ref regions) = event {
                self.ux.draggable_regions.clone_from(regions);
            }
            dispatch_single_view_event(self, event, window, cx, Self::callbacks_mut);
        }
    }

    /// One host-selected retry; never pumps CEF or changes frame retries.
    pub fn retry_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, ViewError> {
        self.check_window(window)?;
        self.check_live()?;
        self.sync_focus(window, cx);
        if !self.focus.is_focused(window)
            || !crate::native_view::is_window_effectively_active(window)
        {
            self.reset_input(InputResetReason::FocusLost);
            return Err(ViewError::Input(EngineError::NotReady));
        }
        let result = self
            .input
            .borrow_mut()
            .retry(self.attachment.handle(), window_identity(window));
        let accepted = match result {
            Ok(InputAdmission::Admitted(mirror)) => {
                self.apply_mirror(mirror);
                Ok(true)
            }
            Ok(InputAdmission::AdmittedAndReset) => {
                self.clear_local_input();
                Ok(true)
            }
            Ok(InputAdmission::Idle) => Ok(false),
            Ok(InputAdmission::Deferred) => unreachable!("retry never defers twice"),
            Err(error) => {
                self.clear_local_input();
                self.record_message(format!("input retry refused: {error:?}"));
                Err(ViewError::Input(error))
            }
        };
        cx.notify();
        accepted
    }

    pub(crate) fn cancel_pending_input(&mut self) {
        self.reset_input(InputResetReason::Rejected);
    }

    fn clear_local_input(&mut self) {
        self.file_drag = None;
        // A queued post-dispatch fence retains its one slot until it runs.
        self.native_ime.invalidate();
        self.ime_request = None;
        self.composition = None;
        self.composition_selection = 0..0;
        self.click.borrow_mut().cancel();
        self.left_mouse_down = false;
        self.right_mouse_down = false;
        self.cef_focus_accepted.set(false);
    }

    fn reset_input(&mut self, reason: InputResetReason) {
        if reason == InputResetReason::Closing {
            crate::menu::retire_projection(&self.menu);
        }
        self.clear_local_input();
        self.input
            .borrow_mut()
            .abandon(self.attachment.handle(), reason);
    }

    fn reject_invalid_input(&mut self) {
        let pending = self.input.borrow().pending();
        if pending {
            self.input.borrow_mut().note_overflow();
            self.clear_local_input();
        } else {
            self.reset_input(InputResetReason::Rejected);
        }
    }

    fn apply_mirror(&mut self, mirror: Option<MirrorEffect>) {
        if mirror.is_some() {
            self.native_ime.invalidate();
            self.ime_request = None;
        }
        apply_mirror_state(
            &mut self.composition,
            &mut self.composition_selection,
            mirror,
        );
    }

    /// Restore CEF focus only from an owned, currently focused input callback.
    /// A frame/render never activates the OS window or calls this method.
    fn ensure_cef_focus(&mut self, window: &Window) -> Result<(), InputCallbackGate> {
        if !self.token.is_live() {
            return Err(InputCallbackGate::BindingRetired);
        }
        if !crate::native_view::is_window_effectively_active(window) {
            return Err(InputCallbackGate::WindowInactive);
        }
        if !self.focus.is_focused(window) {
            return Err(InputCallbackGate::FocusNotOwned);
        }
        // Pending owns the only slot: its successor must reach the lane's
        // observable overflow path without posting another focus command.
        if self.input.borrow().pending() || self.cef_focus_accepted.get() {
            return Ok(());
        }
        let handle = self.attachment.handle().clone();
        match handle.input_snapshot() {
            Ok(snapshot) if snapshot.cleanup_pending || snapshot.cleanup_failed => {
                self.input.borrow_mut().reject_focus_unavailable(
                    &handle,
                    EngineError::NotReady,
                    true,
                );
                self.clear_local_input();
                self.record_message(
                    "input focus refused: Runtime cleanup pending or failed".into(),
                );
                return Err(InputCallbackGate::CleanupUnavailable);
            }
            Err(error) => {
                self.input
                    .borrow_mut()
                    .reject_focus_unavailable(&handle, error.clone(), false);
                self.clear_local_input();
                self.record_message(format!(
                    "input focus refused: snapshot unavailable: {error:?}"
                ));
                return Err(InputCallbackGate::SnapshotUnavailable);
            }
            Ok(_) => {}
        }
        match handle.submit(BrowserCommand::SetFocus(true)) {
            Ok(_) => {
                self.input.borrow_mut().mark_focus();
                self.cef_focus_accepted.set(true);
                Ok(())
            }
            Err(error) => {
                self.input
                    .borrow_mut()
                    .reject_focus_unavailable(&handle, error.clone(), false);
                self.clear_local_input();
                self.record_message(format!("input focus refused: {error:?}"));
                Err(InputCallbackGate::FocusCommandRejected)
            }
        }
    }

    fn ime_geometry(
        &self,
        window: &Window,
        bounds: Bounds<Pixels>,
    ) -> Option<(u64, u64, ImeGeometry, u64)> {
        let composition = self.composition.as_ref()?;
        self.text_context(window).ok()?;
        let owner = self.attachment.handle().snapshot();
        if owner.browser != self.browser
            || owner.state != SessionState::Alive
            || owner.renderer.state == RendererState::Terminated
        {
            return None;
        }
        if self.window_handle != window.window_handle()
            || !self.token.is_live()
            || self.image.is_none()
        {
            return None;
        }
        let viewport = viewport_for(bounds, window.scale_factor()).ok()?;
        // Applied metadata is read from the authoritative producer, not merely
        // a locally accepted SetViewport or a matching rounded pixel size.
        let surface = self.attachment.frame_source().snapshot();
        if surface.closed
            || !surface.view.visible
            || surface.viewport != Some(viewport)
            || self.image.as_ref()?.generation != surface.view.generation
        {
            return None;
        }
        let input = self.attachment.handle().input_snapshot().ok()?;
        let snapshot = self.attachment.handle().ime_snapshot().ok()?;
        let geometry = crate::ime_geometry::composition_anchor(
            &snapshot,
            self.browser,
            input.epoch,
            self.composition_selection.clone(),
            composition.encode_utf16().count(),
            bounds_dip(bounds),
        )?;
        Some((
            snapshot.revision,
            input.epoch,
            geometry,
            surface.view.generation,
        ))
    }

    fn refresh_ime(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (x, y, width, height) = self.paint.borrow().surface_bounds;
        let bounds = Bounds {
            origin: Point { x: px(x), y: px(y) },
            size: size(px(width), px(height)),
        };
        let Some((revision, epoch, geometry, generation)) = self.ime_geometry(window, bounds)
        else {
            self.native_ime.invalidate();
            self.ime_request = None;
            return;
        };
        let projected_scale = window.scale_factor();
        let request = (revision, epoch, geometry, generation, projected_scale);
        if self.ime_request == Some(request) {
            return;
        }
        self.ime_request = Some(request);
        // This only coalesces one owned projection and arms one App.defer.
        // Actual IMM runs after the borrowed Entity/window update is over.
        let entity = cx.entity().downgrade();
        let validate = Rc::new(move |window: &Window, app: &App| {
            let Some(entity) = entity.upgrade() else {
                return false;
            };
            let view = entity.read(app);
            let (x, y, width, height) = view.paint.borrow().surface_bounds;
            let bounds = Bounds {
                origin: Point { x: px(x), y: px(y) },
                size: size(px(width), px(height)),
            };
            window.scale_factor() == projected_scale
                && view.ime_geometry(window, bounds)
                    == Some((revision, epoch, geometry, generation))
        });
        self.native_ime.queue(
            geometry,
            projected_scale,
            revision,
            epoch,
            self.attachment.handle().clone(),
            self.focus.clone(),
            self.window_handle,
            validate,
            cx,
        );
    }

    fn text_context(&self, window: &Window) -> Result<(), InputCallbackGate> {
        if !self.token.is_live() {
            return Err(InputCallbackGate::BindingRetired);
        }
        if !crate::native_view::is_window_effectively_active(window) {
            return Err(InputCallbackGate::WindowInactive);
        }
        if !self.focus.is_focused(window) {
            return Err(InputCallbackGate::FocusNotOwned);
        }
        let snapshot = self
            .attachment
            .handle()
            .input_snapshot()
            .map_err(|_| InputCallbackGate::SnapshotUnavailable)?;
        if snapshot.cleanup_pending || snapshot.cleanup_failed {
            return Err(InputCallbackGate::CleanupUnavailable);
        }
        Ok(())
    }

    fn sync_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.project_menu_focus(window, cx);
        let focused = crate::native_view::is_window_effectively_active(window)
            && self.focus.is_focused(window);
        if self.was_focused && !focused {
            if self.active_drag.is_some() {
                self.finish_active_osr_drag(
                    window.mouse_position(),
                    gpui::Modifiers::default(),
                    true,
                    window,
                    cx,
                );
            }
            if self.attachment.native_context_menu().is_open() {
                self.cef_focus_accepted.set(false);
                self.right_mouse_down = false;
            } else {
                self.reset_input(InputResetReason::FocusLost);
            }
        }
        self.was_focused = focused;
    }

    fn check_window(&self, window: &Window) -> Result<(), ViewError> {
        if window.window_handle() == self.window_handle {
            return Ok(());
        }
        let error = ViewError::WrongWindow {
            expected: self.window_handle.window_id().as_u64(),
            actual: window_identity(window),
        };
        self.record_error(&error);
        Err(error)
    }

    fn check_live(&mut self) -> Result<(), ViewError> {
        if self.token.is_live() {
            return Ok(());
        }
        // Retirement is terminal: no snapshot retry can repair it.
        self.needs_resync = false;
        self.reset_input(InputResetReason::Closing);
        let error = ViewError::RetiredBinding;
        self.record_error(&error);
        Err(error)
    }

    /// Advances the paint revision after a committed atlas change. Checked
    /// overflow permanently retires the token; the caller never revives old
    /// pixels in that case.
    fn advance_binding(&mut self) -> Result<(), ViewError> {
        match self.token.advance() {
            Ok(()) => {
                self.binding_revision = self.binding_revision.saturating_add(1);
                Ok(())
            }
            Err(BindingError::Exhausted) => {
                self.reset_input(InputResetReason::Closing);
                let error = ViewError::BindingExhausted;
                self.record_error(&error);
                Err(error)
            }
        }
    }

    /// Takes the displayed slot, attempts the atlas removal through the shared
    /// transactional helper, and resets metadata only after success.
    ///
    /// This intentionally does not consult token liveness: close/release
    /// cleanup must still remove an atlas entry after retirement. Callers
    /// validate the original window id independently.
    fn clear_image(&mut self, window: &mut Window) -> Result<(), ViewError> {
        match commit_image_change(&mut self.image, None, |old| drop_atlas_image(window, old)) {
            Ok(removed) => {
                if removed {
                    self.released_images = self.released_images.saturating_add(1);
                    self.advance_binding()?;
                    window.refresh();
                }
                Ok(())
            }
            Err(message) => Err(self.window_error(message)),
        }
    }

    fn clear_popup(&mut self, window: &mut Window) -> Result<(), ViewError> {
        match commit_image_change(&mut self.popup_image, None, |old| {
            drop_atlas_image(window, old)
        }) {
            Ok(removed) => {
                if removed {
                    self.popup_released = self.popup_released.saturating_add(1);
                    self.advance_binding()?;
                    window.refresh();
                }
                Ok(())
            }
            Err(message) => Err(self.window_error(message)),
        }
    }

    /// Applies one authoritative mailbox update. The caller has already checked
    /// the fixed window and binding liveness and counted the update source.
    fn apply_update(
        &mut self,
        window: &mut Window,
        update: FrameUpdate,
    ) -> Result<bool, ViewError> {
        if update.browser != self.browser {
            let error = ViewError::BrowserMismatch {
                expected: self.browser,
                actual: update.browser,
            };
            self.record_error(&error);
            self.needs_resync = false;
            return Err(error);
        }
        if update.closed {
            if self.active_drag.is_some() {
                self.cancel_active_osr_drag_in_window(window);
            }
            self.cancel_outbound_drags();
            crate::native_view::set_window_drag_cursor(window, None);
            self.zoom_timeout_task = None;
            self.zoom_gate.clear_in_flight();
            self.pointer_over_surface = false;
            self.attachment.cursor_slot().clear();
            crate::native_view::clear_surface_cursor(
                self.hwnd_raw,
                surface_cursor_key(self.browser),
            );
            self.reset_input(InputResetReason::Closing);
            let invalidate = self.popup_visible && self.popup_image.is_some();
            self.popup_visible = false;
            self.popup_invalid = true;
            let revision = if invalidate {
                self.advance_binding()
            } else {
                Ok(())
            };
            let view = self.clear_image(window);
            let popup = self.clear_popup(window);
            let result = revision.and(view).and(popup);
            self.needs_resync = result.is_err() && self.token.is_live();
            if result.is_ok() {
                self.token.retire();
            }
            return result.map(|_| false);
        }
        let view = self.apply_view_update(window, &update);
        let view_retry = self.needs_resync;
        let target_matches = self
            .viewport_context
            .is_some_and(|target| source_matches_viewport(&update, target));
        let popup = if update.view.visible && target_matches {
            self.apply_popup_update(window, &update.popup)
        } else {
            let had_image = self.popup_image.is_some();
            let invalidate = self.popup_visible && had_image;
            self.popup_visible = false;
            self.popup_invalid = true;
            let revision = if invalidate {
                self.advance_binding()
            } else {
                Ok(())
            };
            let clear = self.clear_popup(window);
            revision.and(clear).map(|_| invalidate || had_image)
        };
        self.needs_resync =
            (view_retry || popup.is_err() || !target_matches) && self.token.is_live();
        match (view, popup) {
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(view_changed), Ok(popup_changed)) => Ok(view_changed || popup_changed),
        }
    }

    /// Synchronous local eligibility boundary, before this frame can paint.
    /// Async SetViewport admission alone cannot revoke an old popup. Snapshot
    /// here is read-only (no mailbox acknowledgement) after window/token checks.
    fn reconcile_popup_context(
        &mut self,
        next: Viewport,
        geometry: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<PaintStamp, ViewError> {
        self.check_window(window)?;
        self.check_live()?;
        let geometry = bounds_dip(geometry);
        let geometry_changed = input_geometry_changed(self.surface_geometry, geometry);
        if self.viewport_context != Some(next) || geometry_changed {
            let changed = geometry_changed
                || self.viewport_context.is_some()
                || self.paint.borrow().scale_factor != next.scale;
            self.viewport_context = Some(next);
            self.surface_geometry = Some(geometry);
            if changed {
                if self.active_drag.is_some() {
                    self.cancel_active_osr_drag_in_window(window);
                }
                if let Some(b) = crate::menu::project(&self.menu) {
                    b.geometry_changed();
                }
                self.reset_input(InputResetReason::FocusLost);
            }
            if changed {
                let had_popup = self.popup_visible || self.popup_image.is_some();
                self.popup_visible = false;
                self.popup_invalid = true;
                if had_popup {
                    // Revoke popup closures before attempting a fallible atlas removal.
                    self.advance_binding()?;
                    let clear = self.clear_popup(window);
                    cx.notify();
                    self.needs_resync = true;
                    clear?;
                } else {
                    self.needs_resync = true;
                }
            }
            let snapshot = self.attachment.frame_source().snapshot();
            // Use the exact producer-applied context, never command admission
            // or a scale inferred from equal ceil pixel dimensions. Returning
            // to an unchanged applied target can restore without a new epoch.
            if snapshot.browser != self.browser {
                let error = ViewError::BrowserMismatch {
                    expected: self.browser,
                    actual: snapshot.browser,
                };
                self.record_error(&error);
                return Err(error);
            }
            if source_matches_viewport(&snapshot, next) {
                self.resync_attempts = self.resync_attempts.saturating_add(1);
                let restored = self.apply_update(window, snapshot)?;
                if changed || restored {
                    cx.notify();
                }
            } else {
                self.needs_resync = true;
            }
        }
        self.token.stamp().ok_or(ViewError::RetiredBinding)
    }

    fn apply_popup_update(
        &mut self,
        window: &mut Window,
        popup: &SurfaceSnapshot,
    ) -> Result<bool, ViewError> {
        let bounds = (
            popup.bounds.x,
            popup.bounds.y,
            popup.bounds.width,
            popup.bounds.height,
        );
        let state_changed = self.popup_generation != popup.generation
            || self.popup_bounds != bounds
            || self.popup_visible != popup.visible;
        let had_image = self.popup_image.is_some();
        let changed = popup_requires_clear(
            self.popup_generation,
            self.popup_bounds,
            popup.generation,
            bounds,
            popup.visible,
        );
        self.popup_invalid |= changed;
        self.popup_generation = popup.generation;
        self.popup_bounds = bounds;
        self.popup_visible = popup.visible;
        // Revoke already-built paint closures even if the atlas clear fails.
        if changed && self.popup_image.is_some() {
            self.advance_binding()?;
        }
        if self.popup_invalid
            || self
                .popup_image
                .as_ref()
                .is_some_and(|slot| slot.generation != popup.generation)
        {
            self.popup_invalid = true;
            self.clear_popup(window)?;
            self.popup_invalid = false;
        }
        let state_changed = state_changed || (had_image && self.popup_image.is_none());
        if !popup.visible {
            return Ok(state_changed);
        }
        let Some(frame) = popup.frame.as_ref() else {
            return Ok(state_changed);
        };
        if !surface_frame_is_current(self.browser, popup, &frame.info, SurfaceKind::Popup) {
            return Ok(state_changed);
        }
        if self.popup_image.as_ref().is_some_and(|slot| {
            slot.generation == popup.generation && slot.sequence == frame.info.sequence
        }) {
            return Ok(state_changed);
        }
        let candidate = image_slot_from_surface(frame, SurfaceKind::Popup).map_err(|error| {
            let error = ViewError::Image(error);
            self.record_error(&error);
            error
        })?;
        match commit_image_change(&mut self.popup_image, Some(candidate), |old| {
            drop_atlas_image(window, old)
        }) {
            Ok(removed) => {
                if removed {
                    self.popup_released = self.popup_released.saturating_add(1);
                }
                self.popup_created = self.popup_created.saturating_add(1);
                self.advance_binding()?;
                window.refresh();
                if crate::native_view::is_window_in_sizemove(window) {
                    crate::native_view::request_sizemove_present(window);
                }
                Ok(true)
            }
            Err(message) => Err(self.window_error(message)),
        }
    }

    fn apply_view_update(
        &mut self,
        window: &mut Window,
        update: &FrameUpdate,
    ) -> Result<bool, ViewError> {
        if update.browser != self.browser {
            self.needs_resync = false;
            let error = ViewError::BrowserMismatch {
                expected: self.browser,
                actual: update.browser,
            };
            self.record_error(&error);
            return Err(error);
        }

        // The coordinator handled Closed before invoking either surface path.
        if let Some(b) = crate::menu::project(&self.menu) {
            b.visible(update.view.visible);
        }
        if !update.view.visible {
            if self.active_drag.is_some() {
                self.cancel_active_osr_drag_in_window(window);
            }
            self.reset_input(InputResetReason::FocusLost);
            // Hiding is recoverable: clear the atlas entry but keep the token
            // live so a later show can present again.
            self.needs_resync = false;
            match self.clear_image(window) {
                Ok(()) => Ok(false),
                Err(error) => {
                    self.needs_resync = self.token.is_live();
                    Err(error)
                }
            }
        } else {
            // Pending/new generations retain the old good image. Paint anchors
            // it at its original 1:1 size and clips it; only a successful new
            // conversion may replace it. Old pixels never acknowledge the new
            // target.
            let Some(frame) = update.view.frame.as_ref() else {
                self.needs_resync = false;
                return Ok(false);
            };
            if !frame_is_current(self.browser, &update.view, &frame.info) {
                // A callback for the previous target, or a token that lost the
                // race with a resize/close, is normal; the good current image
                // (if any) stays untouched.
                self.needs_resync = false;
                return Ok(false);
            }
            if self.image.as_ref().is_some_and(|slot| {
                slot.generation == update.view.generation && slot.sequence == frame.info.sequence
            }) {
                self.needs_resync = false;
                return Ok(false);
            }

            let mut candidate = match image_slot_from_frame(frame) {
                Ok(candidate) => candidate,
                Err(error) => {
                    // The old good image is untouched. One explicit follow-up
                    // consume may snapshot again; no automatic wake is started.
                    self.needs_resync = true;
                    let error = ViewError::Image(error);
                    self.record_error(&error);
                    return Err(error);
                }
            };
            candidate.captured_scale = update.viewport.map(|v| v.scale);

            match commit_image_change(&mut self.image, Some(candidate), |old| {
                drop_atlas_image(window, old)
            }) {
                Ok(removed) => {
                    if removed {
                        self.released_images = self.released_images.saturating_add(1);
                    }
                    self.created_images = self.created_images.saturating_add(1);
                    self.latest_view_frame_seq =
                        self.latest_view_frame_seq.max(frame.info.sequence);
                    self.needs_resync = false;
                    self.in_flight_viewport.set(None);
                    if crate::native_view::is_window_in_sizemove(window)
                        && !self.minimized_hidden.get()
                        && let Some(latest_vp) = self.viewport_context
                        && self.sent_viewport.get() != Some(latest_vp)
                        && self.attachment.handle().snapshot().state == SessionState::Alive
                        && self
                            .attachment
                            .handle()
                            .submit(BrowserCommand::SetViewport(latest_vp))
                            .is_ok()
                    {
                        self.sent_viewport.set(Some(latest_vp));
                        self.in_flight_viewport
                            .set(Some((latest_vp, std::time::Instant::now())));
                    }
                    self.advance_binding()?;
                    window.refresh();
                    if crate::native_view::is_window_in_sizemove(window) {
                        crate::native_view::request_sizemove_present(window);
                    }
                    Ok(true)
                }
                Err(message) => {
                    // The new candidate was dropped and the old image/meta were
                    // restored. The next explicit consume may snapshot-retry.
                    self.needs_resync = true;
                    Err(self.window_error(message))
                }
            }
        }
    }

    /// Detaches the displayed owner into data-only cleanup state, retires the
    /// token, and defers exactly one atlas removal attempt.
    ///
    /// Window routing is by the original `AnyWindowHandle`, never by a strong
    /// Entity. The deferred callback runs at the end of the current effect
    /// cycle, after any entity/window currently on the stack is returned. A
    /// window that is already closed has had its atlas destroyed by GPUI, so
    /// the callback then only drops the CPU owner; no atlas removal is reported
    /// as successful in that case, and no loop or timer is created.
    fn release_on_entity_release(&mut self, app: &mut App, window_handle: AnyWindowHandle) {
        self.clear_callbacks_and_filters();
        let drag_image = self.cancel_active_osr_drag_without_window();
        self.cancel_outbound_drags();
        self.zoom_timeout_task = None;
        self.zoom_gate.clear_in_flight();
        self.pointer_over_surface = false;
        self.pending_zoom = None;
        self.ax_snapshot = None;
        self.attachment.ax_tree().clear();
        self.attachment.cursor_slot().clear();
        self.attachment.native_context_menu().clear();
        crate::native_view::clear_surface_cursor(self.hwnd_raw, surface_cursor_key(self.browser));
        self.reset_input(InputResetReason::Closing);
        self.async_ops.retire_and_cancel_all();
        self.token.retire();
        self.needs_resync = false;
        let view = self.image.take();
        let popup = self.popup_image.take();
        let has_cpu = [&view, &popup]
            .into_iter()
            .flatten()
            .any(|slot| matches!(slot.content, PresentedContent::CpuImage(_)))
            || drag_image.is_some();
        if !has_cpu {
            // GpuTexture leases drop immediately here with no GPUI atlas entry to remove.
            return;
        }
        let diagnostics = self.paint.clone();
        app.defer(move |app| {
            let result = window_handle.update(app, |_, window, _| {
                if window.window_handle() != window_handle {
                    return Err("deferred image cleanup reached a foreign window".to_owned());
                }
                let mut error = None;
                for slot in [view, popup].into_iter().flatten() {
                    if let PresentedContent::CpuImage(image) = slot.content
                        && let Err(message) = drop_atlas_image(window, image)
                    {
                        error.get_or_insert(message);
                    }
                }
                if let Some(drag_image) = drag_image
                    && let Err(message) = drop_atlas_image(window, drag_image)
                {
                    error.get_or_insert(message);
                }
                error.map_or(Ok(()), Err)
            });
            let unexpected = match result {
                Ok(Ok(())) => None,
                Ok(Err(message)) => Some(message),
                // Fixed GPUI flushes deferred effects after returning windows to
                // the App. A missing closed window is a distinct, normal atlas
                // destruction boundary, not a successful drop_image call.
                Err(_) if !app.windows().contains(&window_handle) => None,
                Err(error) => Some(format!(
                    "deferred image cleanup could not access an open window: {error:#}"
                )),
            };
            if let Some(message) = unexpected {
                diagnostics.borrow_mut().last_error = Some(message);
            }
            // No retry task, retained Entity, or synthetic released_images
            // increment. Implicit teardown diagnostics are best effort; this
            // does not promise an observer API after the Entity has gone away.
        });
    }

    fn send_input(&mut self, kind: InputKind, accepted_click: bool, window: &Window) -> bool {
        self.send_input_with_mirror(kind, accepted_click, None, window)
    }

    fn send_input_with_mirror(
        &mut self,
        kind: InputKind,
        accepted_click: bool,
        mirror: Option<MirrorEffect>,
        window: &Window,
    ) -> bool {
        self.send_input_epoch(kind, accepted_click, mirror, window, None)
    }

    fn send_input_epoch(
        &mut self,
        kind: InputKind,
        accepted_click: bool,
        mirror: Option<MirrorEffect>,
        window: &Window,
        expected_epoch: Option<u64>,
    ) -> bool {
        let is_transient = crate::input::is_transient_pointer_input(&kind);
        let result = {
            let mut lane = self.input.borrow_mut();
            match expected_epoch {
                Some(epoch) => lane
                    .submit_with_epoch(
                        self.attachment.handle(),
                        window_identity(window),
                        kind,
                        accepted_click,
                        mirror,
                        Some(epoch),
                    )
                    .map(|(admission, _)| admission),
                None => lane.submit(
                    self.attachment.handle(),
                    window_identity(window),
                    kind,
                    accepted_click,
                    mirror,
                ),
            }
        };
        match result {
            Ok(InputAdmission::Admitted(effect)) => self.apply_mirror(effect),
            Ok(InputAdmission::Deferred) => return true,
            Ok(InputAdmission::AdmittedAndReset | InputAdmission::Idle) => {
                unreachable!("submit is not a retry")
            }
            Err(error) => {
                // Overflow retains the original pending slot, but no later
                // local composition is presented as though it was accepted.
                // Dropped transient pointer samples under backpressure must not
                // disturb active composition/IME state.
                if !(is_transient && error == EngineError::Backpressure)
                    && !expected_epoch.is_some_and(|epoch| {
                        self.attachment
                            .handle()
                            .input_snapshot()
                            .is_ok_and(|s| s.epoch != epoch)
                    })
                {
                    self.clear_local_input();
                }
                self.record_message(format!("input admission refused: {error:?}"));
                return false;
            }
        }
        true
    }

    fn file_target(
        &mut self,
        position: Point<Pixels>,
        window: &Window,
    ) -> Result<(i32, i32), EngineError> {
        // External hover may target an inactive window; OS keyboard focus is not drag eligibility.
        if self
            .check_window(window)
            .and_then(|_| self.check_live())
            .is_err()
            || self.attachment.handle().snapshot().state != SessionState::Alive
            || self.attachment.handle().snapshot().renderer.state == RendererState::Terminated
        {
            return Err(EngineError::NotReady);
        }
        let hitbox = self.file_hitbox.borrow().clone();
        let Some((stamp, hitbox)) = hitbox else {
            return Err(EngineError::NotReady);
        };
        if !self.token.permits(stamp, window_identity(window))
            || !hitbox.is_hovered_at(position, window)
        {
            return Err(EngineError::NotReady);
        }
        self.point(position, false)
            .ok_or(EngineError::InvalidRequest)
    }

    fn send_file_input(
        &mut self,
        kind: InputKind,
        epoch: u64,
        window: &Window,
    ) -> Result<(), EngineError> {
        let result = self.input.borrow_mut().submit_with_epoch(
            self.attachment.handle(),
            window_identity(window),
            kind,
            false,
            None,
            Some(epoch),
        );
        match result {
            Ok((InputAdmission::Admitted(None), accepted_epoch)) if accepted_epoch == epoch => {
                Ok(())
            }
            Ok(_) => Err(EngineError::InvalidRequest), // File events must NEVER enter the retry slot.
            Err(error) => {
                self.record_message(format!("file input refused: {error:?}"));
                Err(error)
            }
        }
    }

    fn cancel_file_drag(&mut self) {
        let Some(gesture) = self.file_drag.as_mut() else {
            return;
        };
        gesture.cancelled = true;
        let epoch = gesture.epoch;
        let Ok(snapshot) = self.attachment.handle().input_snapshot() else {
            self.record_message("file cancellation awaiting original input snapshot".into());
            return; // Keep the cancelled binding; never attach unknown old work to a new drag.
        };
        if snapshot.epoch != epoch || snapshot.cleanup_pending {
            self.file_drag = None;
            return; // The newer epoch/owned cleanup barrier must not be reset again.
        }
        self.input
            .borrow_mut()
            .abandon(self.attachment.handle(), InputResetReason::Rejected);
        if self.attachment.handle().input_snapshot().is_ok_and(|s| {
            s.epoch != epoch
                || s.cleanup_pending
                || (!s.file_drag_active && s.inflight == 0 && !s.native_execution_unconfirmed)
        }) {
            self.file_drag = None;
        }
    }

    fn file_move(&mut self, event: &DragMoveEvent<ExternalPaths>, window: &Window) {
        let Ok((x, y)) = self.file_target(event.event.position, window) else {
            self.cancel_file_drag();
            return;
        };
        let Some(paths) = event.dragged_item().downcast_ref::<ExternalPaths>() else {
            self.cancel_file_drag();
            return;
        };
        let Ok(snapshot) = self.attachment.handle().input_snapshot() else {
            self.cancel_file_drag();
            return;
        };
        if self.input.borrow().pending() {
            self.reject_invalid_input();
            return;
        }
        if let Some(gesture) = self.file_drag.as_ref() {
            if gesture.cancelled
                || !gesture.matches_paths(paths.paths())
                || gesture.epoch != snapshot.epoch
            {
                self.cancel_file_drag();
                return;
            }
            // Only a subsequent real platform move may advance the gesture.
            // Waiting is observable; no task/timer retries this event.
            if let Err(error) = gesture.ready(&snapshot, false) {
                self.record_message(format!("file Over not ready: {error:?}"));
                if snapshot.cleanup_pending
                    || snapshot.cleanup_failed
                    || error == EngineError::InvalidRequest
                {
                    self.cancel_file_drag();
                }
                return;
            }
            if self
                .send_file_input(
                    InputKind::FileDragOver {
                        x,
                        y,
                        modifiers: MouseModifiers::default(),
                    },
                    snapshot.epoch,
                    window,
                )
                .is_ok()
            {
                if let Some(gesture) = self.file_drag.as_mut() {
                    gesture.over_admitted();
                }
            } else {
                self.cancel_file_drag();
            }
        } else {
            if self.file_fence.is_some()
                || snapshot.cleanup_pending
                || snapshot.cleanup_failed
                || snapshot.native_execution_unconfirmed
                || snapshot.inflight != 0
                || snapshot.file_drag_phase != FileDragPhase::Idle
                || snapshot.file_drag_active
                || snapshot.pressed_buttons != 0
                || snapshot.composition_active
            {
                self.record_message("file Enter not ready".into());
                return;
            }
            let paths = match FileDropPaths::copy_from_slice(paths.paths()) {
                Ok(paths) => paths,
                Err(error) => {
                    self.record_message(format!("file payload refused: {error:?}"));
                    return;
                }
            };
            let Some(serial) = self.file_serial.checked_add(1) else {
                self.record_message("file gesture serial exhausted".into());
                return;
            };
            self.file_serial = serial;
            if self
                .send_file_input(
                    InputKind::FileDragEnter {
                        paths: paths.clone(),
                        x,
                        y,
                        modifiers: MouseModifiers::default(),
                    },
                    snapshot.epoch,
                    window,
                )
                .is_ok()
            {
                self.file_drag = Some(crate::file_drag::FileDragSession::new(
                    serial,
                    snapshot.epoch,
                    paths,
                ));
            }
        }
    }

    fn file_drop(&mut self, paths: &ExternalPaths, window: &Window) {
        let Ok((x, y)) = self.file_target(window.mouse_position(), window) else {
            self.cancel_file_drag();
            return;
        };
        let Ok(snapshot) = self.attachment.handle().input_snapshot() else {
            self.cancel_file_drag();
            return;
        };
        let ready = self
            .file_drag
            .as_ref()
            .is_some_and(|s| s.matches_paths(paths.paths()) && s.ready(&snapshot, true).is_ok());
        if !ready || self.input.borrow().pending() {
            self.record_message("file Drop refused before confirmed original Over".into());
            self.cancel_file_drag();
            return;
        }
        if self
            .send_file_input(
                InputKind::FileDrop {
                    x,
                    y,
                    modifiers: MouseModifiers::default(),
                },
                snapshot.epoch,
                window,
            )
            .is_ok()
        {
            self.file_drag = None; // Admission only; actual effect remains Runtime/page-owned.
        } else {
            self.cancel_file_drag();
        }
    }

    fn file_release_fence(&mut self, window: &Window) -> Option<u64> {
        self.check_window(window).ok()?;
        let serial = self.file_drag.as_ref()?.serial;
        if self.file_fence.is_some() {
            return None;
        }
        self.file_fence = Some(serial);
        Some(serial)
    }

    fn finish_file_release(&mut self, serial: u64) {
        if self.file_fence != Some(serial) {
            return;
        }
        self.file_fence = None;
        if self.file_drag.as_ref().is_some_and(|s| s.serial == serial) {
            // Bubble drop did not consume this exact gesture (overlay/stopped propagation).
            self.cancel_file_drag();
        }
    }

    fn point(&self, position: Point<Pixels>, outside: bool) -> Option<(i32, i32)> {
        translate_point_transformed(
            self.paint.borrow().surface_bounds,
            (f32::from(position.x), f32::from(position.y)),
            self.rotation_angle_deg,
            self.fit_rotated_to_bounds,
            outside,
        )
    }

    fn pointer_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(button) = input_button(event.button) else {
            return;
        };
        self.last_pointer_win_dip =
            Some((f32::from(event.position.x), f32::from(event.position.y)));
        if self.active_drag.is_some() {
            self.finish_active_osr_drag(event.position, event.modifiers, true, window, cx);
        }
        self.presses_received = self.presses_received.saturating_add(1);
        if self
            .check_window(window)
            .and_then(|_| self.check_live())
            .is_err()
            || !crate::native_view::is_window_effectively_active(window)
            || self.attachment.handle().snapshot().state != SessionState::Alive
        {
            return;
        }
        self.last_hover_dip = None;
        let Some((x, y)) = self.point(event.position, false) else {
            return;
        };
        let Ok(count) = u8::try_from(event.click_count) else {
            self.record_message("invalid platform click count".into());
            return;
        };
        if count == 0 {
            return;
        }
        // Match CefSharp OnMouseDown: fold clickCount > 3 to 1, and normalize
        // MBT_RIGHT to 1 so rapid right-clicks never interfere with context menu trigger.
        let count = normalize_click_count(button, count);
        let input_pending = self.input.borrow().pending();
        if input_pending {
            // The capture listener already marked this down as overflow.
            // Keep the original slot until the host explicitly retries it.
            return;
        }
        self.presses_forwarded = self.presses_forwarded.saturating_add(1);
        // Revoke a previous menu before a new browser gesture is posted.
        if let Some(b) = crate::menu::project(&self.menu) {
            b.geometry_changed();
        }
        self.focus.focus(window, cx);
        self.project_menu_focus(window, cx);
        self.was_focused = true;
        if button == InputButton::Right
            && self
                .attachment
                .native_context_menu()
                .was_active_within(NATIVE_CONTEXT_MENU_REOPEN_WINDOW)
        {
            self.cef_focus_accepted.set(false);
        }
        if self.ensure_cef_focus(window).is_err() {
            return;
        }
        let modifiers = mouse_modifiers(event.modifiers);
        if button == InputButton::Right
            && (self.last_hover_dip.is_none()
                || self
                    .attachment
                    .native_context_menu()
                    .was_active_within(NATIVE_CONTEXT_MENU_REOPEN_WINDOW))
        {
            let _ = self.send_input(
                InputKind::MouseMove {
                    x,
                    y,
                    leaving: false,
                    modifiers,
                },
                false,
                window,
            );
        }
        if button == InputButton::Left {
            let bounds = self.paint.borrow().surface_bounds;
            if let Err(error) = self.click.borrow_mut().begin(
                window_identity(window),
                bounds,
                (f32::from(event.position.x), f32::from(event.position.y)),
                modifiers,
                count,
            ) {
                self.record_message(error.to_string());
            }
        } else {
            self.click.borrow_mut().cancel();
        }
        let admitted = self.send_input(
            InputKind::MouseButton {
                x,
                y,
                button,
                up: false,
                click_count: count,
                modifiers,
            },
            false,
            window,
        );
        if button == InputButton::Left {
            self.left_mouse_down = admitted;
        } else if button == InputButton::Right {
            self.right_mouse_down = admitted;
        }
        cx.notify();
    }

    fn pointer_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(button) = input_button(event.button) else {
            return;
        };
        self.last_pointer_win_dip =
            Some((f32::from(event.position.x), f32::from(event.position.y)));
        if button == InputButton::Left {
            self.left_mouse_down = false;
            if self.active_drag.is_some() {
                self.finish_active_osr_drag(event.position, event.modifiers, false, window, cx);
                return;
            }
            if self.active_drag_flag.get() {
                self.click.borrow_mut().cancel();
                return;
            }
        }
        let owned = self.input.borrow().owns_button(button);
        if button == InputButton::Right
            && !owned
            && !self.right_mouse_down
            && !self.input.borrow().pending()
            && self
                .attachment
                .native_context_menu()
                .was_active_within(NATIVE_CONTEXT_MENU_REOPEN_WINDOW)
        {
            // When CEF's native popup menu captured and swallowed WM_RBUTTONDOWN
            // at a new location to dismiss the old menu, GPUI receives only the
            // trailing orphan MouseUp(Right). Restore focus, sync cursor position,
            // and synthesize a clean right-click pair so the new menu opens directly.
            if self
                .check_window(window)
                .and_then(|_| self.check_live())
                .is_err()
                || self.attachment.handle().snapshot().state != SessionState::Alive
            {
                return;
            }
            let Some((x, y)) = self.point(event.position, false) else {
                return;
            };
            self.focus.focus(window, cx);
            self.project_menu_focus(window, cx);
            self.was_focused = true;
            self.cef_focus_accepted.set(false);
            if let Err(error) = self
                .attachment
                .handle()
                .submit(BrowserCommand::SetFocus(true))
            {
                self.record_message(format!("orphan right-up focus refused: {error:?}"));
                return;
            }
            self.input.borrow_mut().mark_focus();
            self.cef_focus_accepted.set(true);
            let modifiers = mouse_modifiers(event.modifiers);
            let _ = self.send_input(
                InputKind::MouseMove {
                    x,
                    y,
                    leaving: false,
                    modifiers,
                },
                false,
                window,
            );
            if self.send_input(
                InputKind::MouseButton {
                    x,
                    y,
                    button: InputButton::Right,
                    up: false,
                    click_count: 1,
                    modifiers,
                },
                false,
                window,
            ) {
                let _ = self.send_input(
                    InputKind::MouseButton {
                        x,
                        y,
                        button: InputButton::Right,
                        up: true,
                        click_count: 1,
                        modifiers,
                    },
                    false,
                    window,
                );
            }
            self.right_mouse_down = false;
            cx.notify();
            return;
        }
        if !owned && !self.input.borrow().pending() {
            if button == InputButton::Right {
                self.right_mouse_down = false;
            }
            return;
        }
        if button == InputButton::Right {
            self.right_mouse_down = false;
        }
        if self
            .check_window(window)
            .and_then(|_| self.check_live())
            .is_err()
            || (!owned && !crate::native_view::is_window_effectively_active(window))
        {
            self.reset_input(InputResetReason::FocusLost);
            return;
        }
        let coords = if button == InputButton::Right {
            self.point(event.position, false)
                .or_else(|| self.point(event.position, true))
        } else {
            self.point(event.position, true)
        };
        let Some((x, y)) = coords else {
            self.reset_input(InputResetReason::Rejected);
            return;
        };
        let Ok(count) = u8::try_from(event.click_count) else {
            self.reset_input(InputResetReason::Rejected);
            return;
        };
        if count == 0 {
            self.reset_input(InputResetReason::Rejected);
            return;
        }
        let count = normalize_click_count(button, count);
        let complete = if button == InputButton::Left {
            let bounds = self.paint.borrow().surface_bounds;
            self.click
                .borrow_mut()
                .finish(
                    window_identity(window),
                    bounds,
                    (f32::from(event.position.x), f32::from(event.position.y)),
                    mouse_modifiers(event.modifiers),
                )
                .ok()
                .flatten()
                .is_some()
        } else {
            false
        };
        let _ = self.send_input(
            InputKind::MouseButton {
                x,
                y,
                button,
                up: true,
                click_count: count,
                modifiers: mouse_modifiers(event.modifiers),
            },
            complete,
            window,
        );
        cx.notify();
    }

    fn pointer_move(&mut self, event: &MouseMoveEvent, window: &Window, owned_only: bool) {
        self.last_pointer_win_dip =
            Some((f32::from(event.position.x), f32::from(event.position.y)));
        if self.active_drag.is_some() || self.active_drag_flag.get() {
            return;
        }
        let held = self.input.borrow().held_button();
        if held != owned_only {
            return;
        }
        if self
            .check_window(window)
            .and_then(|_| self.check_live())
            .is_err()
        {
            return;
        }
        if !self.attachment.handle().is_fast_input_ready() {
            return;
        }
        if held
            && ((self.input.borrow().owns_button(InputButton::Left) && !self.left_mouse_down)
                || (event.pressed_button.is_none() && !os_any_mouse_button_down()))
        {
            if self.right_mouse_down || self.input.borrow().owns_button(InputButton::Right) {
                // Avoid dropping an in-flight right-click on micro-movement right before MouseUp(Right).
                return;
            }
            // A mouse move no longer reporting a pressed button (neither in
            // GPUI's event nor physically pressed — guarding against Win32
            // WM_NCMOUSEMOVE which clears pressed_button mid-drag) cannot
            // extend an owned drag; revoke it instead of leaving CEF pressed.
            self.reset_input(InputResetReason::FocusLost);
            return;
        }
        let Some((x, y)) = self.point(event.position, held) else {
            return;
        };
        if !held {
            if !self.pointer_over_surface {
                self.pointer_over_surface = true;
                crate::native_view::set_surface_cursor_hover(
                    window,
                    surface_cursor_key(self.browser),
                    true,
                );
            }
            let (sx, sy, _, _) = self.paint.borrow().surface_bounds;
            self.last_hover_dip = Some((
                f32::from(event.position.x) - sx,
                f32::from(event.position.y) - sy,
            ));
        } else {
            self.last_hover_dip = None;
        }
        let _ = self.input.borrow_mut().submit_mouse_move_fast(
            self.attachment.handle(),
            x,
            y,
            false,
            mouse_modifiers(event.modifiers),
        );
    }

    fn drag_pointer_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.last_pointer_win_dip =
            Some((f32::from(event.position.x), f32::from(event.position.y)));
        if self.active_drag.is_none() {
            return;
        }
        if self
            .check_window(window)
            .and_then(|_| self.check_live())
            .is_err()
        {
            self.finish_active_osr_drag(event.position, event.modifiers, true, window, cx);
            return;
        }
        if event.pressed_button.is_none() && !os_any_mouse_button_down() {
            self.finish_active_osr_drag(event.position, event.modifiers, true, window, cx);
            return;
        }
        let inside_pt = self.point(event.position, false);
        let view_pt = inside_pt.or_else(|| self.point(event.position, true));
        let cursor_win_dip = (f32::from(event.position.x), f32::from(event.position.y));
        let Some(drag) = self.active_drag.as_mut() else {
            return;
        };
        drag.cursor_win_dip = cursor_win_dip;
        let next_pt = view_pt.unwrap_or(drag.last_view_point);
        drag.last_view_point = next_pt;
        let now_in_view = inside_pt.is_some();
        let left_view = drag.in_view && !now_in_view;
        drag.in_view = now_in_view;
        if left_view {
            drag.current_op = DragOperationMask::NONE;
            crate::native_view::set_window_drag_cursor(window, Some(DragOperationMask::NONE));
        }
        let drag_id = drag.drag_id;
        let _ = self.attachment.handle().submit_osr_drag_move_fast(
            drag_id,
            next_pt.0,
            next_pt.1,
            mouse_modifiers(event.modifiers),
            now_in_view,
        );
        window.refresh();
        cx.notify();
    }

    fn drag_pointer_leave(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.active_drag.as_mut() else {
            return;
        };
        if !drag.in_view {
            return;
        }
        drag.in_view = false;
        drag.current_op = DragOperationMask::NONE;
        let drag_id = drag.drag_id;
        let (x, y) = drag.last_view_point;
        crate::native_view::set_window_drag_cursor(window, Some(DragOperationMask::NONE));
        let _ = self.attachment.handle().submit_osr_drag_move_fast(
            drag_id,
            x,
            y,
            MouseModifiers::default(),
            false,
        );
        window.refresh();
        cx.notify();
    }

    fn pointer_leave(&mut self, window: &Window) {
        if self.active_drag.is_some()
            || self.input.borrow().held_button()
            || self
                .check_window(window)
                .and_then(|_| self.check_live())
                .is_err()
        {
            return;
        }
        let snapshot = self.attachment.handle().snapshot();
        if snapshot.state != SessionState::Alive
            || snapshot.renderer.state == RendererState::Terminated
        {
            return;
        }
        self.last_hover_dip = None;
        self.ux.tooltip.clear();
        let (x, y, w, h) = self.paint.borrow().surface_bounds;
        if w > 0. && h > 0. && [x, y, w, h].into_iter().all(f32::is_finite) {
            let _ = self.send_input(
                InputKind::MouseMove {
                    x: (w / 2.) as i32,
                    y: (h / 2.) as i32,
                    leaving: true,
                    modifiers: MouseModifiers::default(),
                },
                false,
                window,
            );
        }
    }

    fn pointer_scroll(&mut self, event: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .check_window(window)
            .and_then(|_| self.check_live())
            .is_err()
        {
            return;
        }
        if !self.attachment.handle().is_fast_input_ready() {
            return;
        }
        let Some((x, y)) = self.point(event.position, false) else {
            return;
        };
        if matches!(event.touch_phase, TouchPhase::Ended | TouchPhase::Cancelled) {
            self.scroll_accumulator.reset();
        }
        if self.ctrl_wheel_zoom_enabled && event.modifiers.control {
            let notch_delta = match event.delta {
                ScrollDelta::Lines(delta) => delta.y / keymap::DEFAULT_WHEEL_SCROLL_LINES,
                ScrollDelta::Pixels(delta) => f32::from(delta.y) / 100.0,
            };
            if notch_delta.is_finite() && notch_delta != 0.0 {
                let now = Instant::now();
                let expired = self
                    .ctrl_wheel_last_at
                    .is_some_and(|t| now.saturating_duration_since(t) > Duration::from_millis(400));
                let reversed = (self.ctrl_wheel_zoom_notches > 0.0 && notch_delta < 0.0)
                    || (self.ctrl_wheel_zoom_notches < 0.0 && notch_delta > 0.0);
                if expired || reversed {
                    self.ctrl_wheel_zoom_notches = 0.0;
                }
                self.ctrl_wheel_last_at = Some(now);
                self.ctrl_wheel_zoom_notches += notch_delta;
                let whole_steps = self.ctrl_wheel_zoom_notches.trunc() as i32;
                if whole_steps != 0 {
                    self.ctrl_wheel_zoom_notches -= whole_steps as f32;
                    self.step_zoom(whole_steps, window, cx);
                }
            }
            return;
        }
        let (mut delta_x, mut delta_y) = match event.delta {
            ScrollDelta::Lines(delta) => {
                self.scroll_accumulator.accumulate_lines((delta.x, delta.y))
            }
            ScrollDelta::Pixels(delta) => self
                .scroll_accumulator
                .accumulate_pixels((f32::from(delta.x), f32::from(delta.y))),
        };
        if event.modifiers.shift && delta_x == 0 && delta_y != 0 {
            delta_x = delta_y;
            delta_y = 0;
        }
        if delta_x == 0 && delta_y == 0 {
            return;
        }
        let _ = self.input.borrow_mut().submit_mouse_wheel_fast(
            self.attachment.handle(),
            x,
            y,
            delta_x,
            delta_y,
            mouse_modifiers(event.modifiers),
        );
    }

    pub(crate) fn native_key_down(
        &mut self,
        key: NativeKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> NativeDownAdmission {
        if key.vk == 0x1B && self.active_drag.is_some() {
            self.finish_active_osr_drag(
                window.mouse_position(),
                gpui::Modifiers::default(),
                true,
                window,
                cx,
            );
            return NativeDownAdmission::ConsumedByHostGesture;
        }
        self.input
            .borrow_mut()
            .note_callback_entered(InputCallback::KeyDown);
        if let Err(gate) = self.ensure_cef_focus(window) {
            self.input
                .borrow_mut()
                .note_callback_rejected(InputCallback::KeyDown, gate);
            return NativeDownAdmission::Refused;
        }
        // The bridge needs an explicit *actual lane outcome*: a deferred raw
        // key holds the only pending slot but must not generate a later CHAR.
        let result = self.input.borrow_mut().submit_with_epoch(
            self.attachment.handle(),
            window_identity(window),
            key.kind(),
            false,
            None,
            None,
        );
        match result {
            Ok((InputAdmission::Admitted(effect), epoch)) => {
                self.apply_mirror(effect);
                NativeDownAdmission::Admitted(epoch)
            }
            Ok((InputAdmission::Deferred, epoch)) => NativeDownAdmission::Deferred(epoch),
            Err(error) => {
                self.clear_local_input();
                self.record_message(format!("native key admission refused: {error:?}"));
                NativeDownAdmission::Refused
            }
            Ok((InputAdmission::Idle | InputAdmission::AdmittedAndReset, _)) => {
                unreachable!("initial native submit is not a retry")
            }
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // A live native service intercepts real down *before* GPUI shortcut
        // pending/replay and forwards it above. Exempt host shortcuts deliberately
        // stay in GPUI and must not leak into Chromium through this listener.
        if self
            .native_keyboard
            .as_ref()
            .and_then(|service| service.upgrade())
            .is_some_and(|service| service.is_active())
        {
            return;
        }
        if self.active_drag.is_some() && event.keystroke.key.eq_ignore_ascii_case("escape") {
            self.finish_active_osr_drag(
                window.mouse_position(),
                event.keystroke.modifiers,
                true,
                window,
                cx,
            );
            return;
        }
        self.input
            .borrow_mut()
            .note_callback_entered(InputCallback::KeyDown);
        if let Err(gate) = self.ensure_cef_focus(window) {
            self.input
                .borrow_mut()
                .note_callback_rejected(InputCallback::KeyDown, gate);
            return;
        }
        let Some(code) = keymap::windows_key_code(&event.keystroke.key) else {
            return;
        };
        let _ = self.send_input(
            InputKind::Key {
                windows_key_code: code,
                native_key_code: code,
                up: false,
                repeat: event.is_held,
                system: event.keystroke.modifiers.alt,
                modifiers: mouse_modifiers(event.keystroke.modifiers),
            },
            false,
            window,
        );
        // WM_CHAR / WM_IME_COMPOSITION is routed only through EntityInputHandler.
    }

    fn key_up(&mut self, event: &KeyUpEvent, window: &Window) {
        let native = self
            .native_keyboard
            .as_ref()
            .and_then(|service| service.upgrade())
            .filter(|service| service.is_active());
        let claimed = native
            .as_ref()
            .and_then(|service| claim_up(service, self.browser, window, event));
        if native.is_some() && claimed.is_none() {
            // Original host exceptions are not browser input. No value-based
            // pairing of an ordinary GPUI event to a raw Win32 key is allowed.
            return;
        }
        self.input
            .borrow_mut()
            .note_callback_entered(InputCallback::KeyUp);
        if let Err(gate) = self.ensure_cef_focus(window) {
            self.input
                .borrow_mut()
                .note_callback_rejected(InputCallback::KeyUp, gate);
            return;
        }
        let kind = if let Some(native) = claimed {
            native.kind()
        } else {
            let Some(code) = keymap::windows_key_code(&event.keystroke.key) else {
                return;
            };
            InputKind::Key {
                windows_key_code: code,
                native_key_code: code,
                up: true,
                repeat: false,
                system: event.keystroke.modifiers.alt,
                modifiers: mouse_modifiers(event.keystroke.modifiers),
            }
        };
        let _ = self.send_input_epoch(
            kind,
            false,
            None,
            window,
            claimed.and_then(|key| key.expected_epoch),
        );
    }

    fn record_error(&self, error: &ViewError) {
        self.record_message(error.to_string());
    }

    fn record_message(&self, message: String) {
        self.paint.borrow_mut().last_error = Some(message);
    }

    fn note_rejected_paint(&self, message: String) {
        let mut paint = self.paint.borrow_mut();
        paint.rejected_paints = paint.rejected_paints.saturating_add(1);
        paint.last_error = Some(message);
    }

    fn window_error(&self, message: String) -> ViewError {
        let error = ViewError::Window { message };
        self.record_error(&error);
        error
    }
}

impl Drop for CefBrowser {
    fn drop(&mut self) {
        // Fallback if the release observer did not run. It may submit an
        // owned, thread-safe ResetInput request, but never invokes GPUI,
        // direct native CEF or an event-loop pump. Dropping the ordinary
        // subscriber fields unregisters them without App-lifetime detachment.
        self.clear_callbacks_and_filters();
        self.cancel_outbound_drags();
        self.zoom_timeout_task = None;
        self.zoom_gate.clear_in_flight();
        self.ax_snapshot = None;
        self.attachment.ax_tree().clear();
        self.reset_input(InputResetReason::Closing);
        self.async_ops.retire_and_cancel_all();
        self.token.retire();
    }
}

// Expose the existing logical focus handle through GPUI's ordinary widget
// trait. This does not focus/activate an OS window or submit a CEF command.
impl gpui::Focusable for CefBrowser {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EntityInputHandler for CefBrowser {
    fn accepts_text_input(&self, window: &mut Window, _cx: &mut Context<Self>) -> bool {
        self.text_context(window).is_ok()
    }

    fn text_for_range(
        &mut self,
        range: std::ops::Range<usize>,
        _adjusted: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let Some(text) = &self.composition else {
            return Some(String::new());
        };
        let units: Vec<u16> = text.encode_utf16().collect();
        let start = range.start.min(units.len());
        let end = range.end.min(units.len()).max(start);
        Some(String::from_utf16_lossy(&units[start..end]))
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.composition_selection.clone(),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        self.composition
            .as_ref()
            .map(|text| 0..text.encode_utf16().count())
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pending_mark = self.input.borrow().pending_mark();
        if pending_mark {
            self.input.borrow_mut().note_overflow();
            self.clear_local_input();
            return;
        }
        if self.composition.is_none() {
            return;
        }
        if !self.accepts_text_input(window, cx) {
            self.reset_input(InputResetReason::FocusLost);
            return;
        }
        let _ = self.send_input_with_mirror(
            InputKind::ImeFinish {
                keep_selection: true,
            },
            false,
            Some(MirrorEffect::Clear),
            window,
        );
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<std::ops::Range<usize>>,
        text: &str,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.input
            .borrow_mut()
            .note_callback_entered(InputCallback::ReplaceText);
        let pending = self.input.borrow().pending();
        if !pending {
            if let Err(gate) = self.ensure_cef_focus(window) {
                self.input
                    .borrow_mut()
                    .note_callback_rejected(InputCallback::ReplaceText, gate);
                return;
            }
        }
        if let Err(gate) = self.text_context(window) {
            self.input
                .borrow_mut()
                .note_callback_rejected(InputCallback::ReplaceText, gate);
            self.reset_input(InputResetReason::FocusLost);
            return;
        }
        if text.encode_utf16().count() > MAX_INPUT_TEXT_UTF16 {
            self.reject_invalid_input();
            return;
        }
        let clear = self.composition.as_ref().map(|_| MirrorEffect::Clear);
        if let Some(range) = range {
            let Some(replacement) = bounded_replacement(range) else {
                self.reject_invalid_input();
                return;
            };
            // Preserve the caller's range: Windows CEF 146 cannot honor it,
            // so Runtime rejects this typed command rather than inserting
            // unscoped Text or claiming a document edit succeeded.
            let _ = self.send_input_with_mirror(
                InputKind::ReplaceText {
                    text: text.to_owned(),
                    replacement,
                },
                false,
                clear,
                window,
            );
        } else if text.is_empty() {
            let pending_mark = self.input.borrow().pending_mark();
            if pending_mark {
                self.input.borrow_mut().note_overflow();
                self.clear_local_input();
            } else if clear.is_some() {
                let _ = self.send_input_with_mirror(InputKind::ImeCancel, false, clear, window);
            }
        } else {
            let _ =
                self.send_input_with_mirror(InputKind::Text(text.to_owned()), false, clear, window);
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<std::ops::Range<usize>>,
        text: &str,
        selection: Option<std::ops::Range<usize>>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.input
            .borrow_mut()
            .note_callback_entered(InputCallback::ReplaceAndMarkText);
        let pending = self.input.borrow().pending();
        if !pending {
            if let Err(gate) = self.ensure_cef_focus(window) {
                self.input
                    .borrow_mut()
                    .note_callback_rejected(InputCallback::ReplaceAndMarkText, gate);
                return;
            }
        }
        if let Err(gate) = self.text_context(window) {
            self.input
                .borrow_mut()
                .note_callback_rejected(InputCallback::ReplaceAndMarkText, gate);
            self.reset_input(InputResetReason::FocusLost);
            return;
        }
        if text.is_empty() {
            let pending_mark = self.input.borrow().pending_mark();
            if pending_mark {
                self.input.borrow_mut().note_overflow();
                self.clear_local_input();
            } else if self.composition.is_some() {
                let _ = self.send_input_with_mirror(
                    InputKind::ImeCancel,
                    false,
                    Some(MirrorEffect::Clear),
                    window,
                );
            }
            return;
        }
        let len = text.encode_utf16().count();
        let selected = selection.unwrap_or(len..len);
        let replacement = match range {
            Some(range) => match bounded_replacement(range) {
                Some(range) => Some(range),
                None => {
                    self.reject_invalid_input();
                    return;
                }
            },
            None => None,
        };
        if len > MAX_INPUT_TEXT_UTF16 || selected.start > selected.end || selected.end > len {
            self.reject_invalid_input();
            return;
        }
        // A Some replacement is forwarded unchanged for Runtime's typed
        // Windows rejection; never silently omit the requested range.
        let _ = self.send_input_with_mirror(
            InputKind::ImeSet {
                text: text.to_owned(),
                selection: Utf16Range {
                    start: selected.start as u32,
                    end: selected.end as u32,
                },
                replacement,
            },
            false,
            Some(MirrorEffect::Mark {
                text: text.to_owned(),
                selection: selected,
            }),
            window,
        );
    }

    fn bounds_for_range(
        &mut self,
        range: std::ops::Range<usize>,
        element_bounds: Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // Only the accepted composition selection is represented; this is not
        // a remote document layout API for arbitrary requested ranges.
        if range != self.composition_selection {
            return None;
        }
        let (_, _, geometry, _) = self.ime_geometry(window, element_bounds)?;
        Some(Bounds {
            origin: Point {
                x: px(geometry.x),
                y: px(geometry.y),
            },
            size: size(px(geometry.width), px(geometry.height)),
        })
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Render for CefBrowser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let actual = window_identity(window);
        if window.window_handle() != self.window_handle {
            // Rendering in a foreign window would read this view's image and
            // paint it at the wrong surface; refuse and leave that window empty.
            self.note_rejected_paint(
                ViewError::WrongWindow {
                    expected: self.window_handle.window_id().as_u64(),
                    actual,
                }
                .to_string(),
            );
            return div().size_full().into_any_element();
        }
        if crate::native_view::is_window_in_sizemove(window)
            && !self.minimized_hidden.get()
            && self.token.is_live()
            && self.attachment.frame_source().has_pending_update()
            && let Some(update) = self.attachment.frame_source().take_update()
        {
            self.consumed_updates = self.consumed_updates.saturating_add(1);
            let _ = self.apply_update(window, update);
        }
        let Some(stamp) = self.token.stamp() else {
            self.note_rejected_paint(ViewError::RetiredBinding.to_string());
            return div().size_full().into_any_element();
        };
        if !self.token.permits(stamp, actual) {
            self.note_rejected_paint(ViewError::RetiredBinding.to_string());
            return div().size_full().into_any_element();
        }

        let backdrop_rgb = self
            .image
            .as_ref()
            .map_or(0xffffff, |slot| slot.backdrop_rgb);
        let surface_opacity = self.surface_opacity;
        let rotation_angle_deg = self.rotation_angle_deg;
        let nearest_neighbor_scaling = self.nearest_neighbor_scaling;
        let fit_rotated_to_bounds = self.fit_rotated_to_bounds;
        let use_opaque_backdrop = !self.transparent_backdrop
            && surface_opacity >= 1.0 - 1e-4
            && rotation_angle_deg.abs() <= 1e-4;
        let paint_frame = self.image.as_ref().map(|slot| PaintFrame {
            content: slot.content.clone(),
            sequence: slot.sequence,
            generation: slot.generation,
            captured_scale: slot.captured_scale,
            stamp,
        });
        let popup_frame = if self.popup_visible && !self.popup_invalid {
            self.popup_image.as_ref().map(|slot| PaintFrame {
                content: slot.content.clone(),
                sequence: slot.sequence,
                generation: slot.generation,
                captured_scale: None,
                stamp,
            })
        } else {
            None
        };
        let popup_bounds = self.popup_bounds;
        let file_hitbox = self.file_hitbox.clone();
        let diag_prepaint = self.paint.clone();
        let diag_paint = self.paint.clone();
        let diag_a11y = self.paint.clone();
        let ax_snapshot = self.ax_snapshot.clone().filter(|s| !s.nodes.is_empty());
        let token_prepaint = self.token.clone();
        let token_paint = self.token.clone();
        let expected_window = actual;
        let browser_key = surface_cursor_key(self.browser);
        let handle = self.attachment.handle().clone();
        let sent_viewport = self.sent_viewport.clone();
        let in_flight_viewport = self.in_flight_viewport.clone();
        let sent_screen_origin = self.sent_screen_origin.clone();
        let sent_client_offset = self.sent_client_offset.clone();
        let minimized_hidden = self.minimized_hidden.clone();
        let drop_surface_geometry = self.drop_surface_geometry.clone();

        let active_drag_op = self.active_drag.as_ref().map(|d| d.current_op);
        let drag_overlay = self.active_drag.as_ref().and_then(|d| {
            d.preview_image.as_ref().map(|img| DragPaintOverlay {
                cursor_win_dip: d.cursor_win_dip,
                preview_image: img.clone(),
                preview_size_dip: d.preview_size_dip,
                hotspot_dip: d.hotspot_dip,
            })
        });
        let drag_fallback_card = self.active_drag.as_ref().and_then(|d| {
            if d.preview_image.is_none() {
                d.fallback_label
                    .as_ref()
                    .map(|lbl| (d.cursor_win_dip.0, d.cursor_win_dip.1, lbl.clone()))
            } else {
                None
            }
        });
        let drag_flag_move = self.active_drag_flag.clone();

        self.sync_focus(window, cx);
        let focus = self.focus.clone();
        let input_paint = self.input.clone();
        let entity = cx.entity();
        let weak_down = entity.downgrade();
        let weak_up = weak_down.clone();
        let weak_move = weak_down.clone();
        let weak_context = weak_down.clone();
        let global_down = weak_down.clone();
        let root_div = div()
            .id(("cef-browser-input", cx.entity_id()))
            .relative()
            .size_full()
            .when(use_opaque_backdrop, |el| el.bg(gpui::rgb(backdrop_rgb)))
            .role(Role::WebView)
            .cursor(
                active_drag_op.map_or_else(|| cursor_style_for(self.ux.cursor), drag_cursor_style),
            )
            .track_focus(&focus)
            .on_any_mouse_down(move |event, window, app| {
                // Only eligible GPUI bubble/hitbox input may begin a click; an
                // overlaid host widget must not click through into CEF.
                if let Some(entity) = weak_down.upgrade() {
                    entity.update(app, |view, cx| view.pointer_down(event, window, cx));
                }
            })
            .on_drag_move::<ExternalPaths>(cx.listener(|view, event, window, _| {
                view.file_move(event, window);
            }))
            .on_drop::<ExternalPaths>(cx.listener(|view, paths, window, _| {
                view.file_drop(paths, window);
            }))
            .on_mouse_move(cx.listener(|view, event: &MouseMoveEvent, window, cx| {
                if view.active_drag.is_none()
                    && !view.active_drag_flag.get()
                    && !cx.has_active_drag()
                {
                    view.pointer_move(event, window, false);
                }
            }))
            .on_hover(cx.listener(|view, hovered: &bool, window, cx| {
                view.pointer_over_surface = *hovered;
                if view.active_drag.is_none() && !view.active_drag_flag.get() {
                    crate::native_view::set_surface_cursor_hover(
                        window,
                        surface_cursor_key(view.browser),
                        *hovered,
                    );
                }
                if !*hovered {
                    if view.active_drag.is_some() {
                        view.drag_pointer_leave(window, cx);
                    } else if !view.active_drag_flag.get() && !cx.has_active_drag() {
                        view.pointer_leave(window);
                    }
                }
            }))
            .on_scroll(cx.listener(|view, event: &ScrollEvent, window, cx| {
                view.pointer_scroll(event, window, cx);
            }))
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                view.key_down(event, window, cx);
            }))
            .on_key_up(cx.listener(|view, event: &KeyUpEvent, window, _| {
                view.key_up(event, window);
            }))
            .on_pinch(cx.listener(|view, event: &PinchEvent, window, cx| {
                view.pointer_pinch(event, window, cx);
            }));
        let root_div = if let Some(snapshot) = ax_snapshot {
            root_div.a11y_synthetic_children(move |builder| {
                let diag = diag_a11y.borrow();
                crate::accessibility::project_ax_snapshot(
                    &snapshot,
                    (diag.surface_bounds.0, diag.surface_bounds.1),
                    diag.scale_factor,
                    builder,
                );
            })
        } else {
            root_div
        };
        root_div
            .child(
                canvas(
                    move |bounds, window, app| -> Option<PaintStamp> {
                        file_hitbox.borrow_mut().take();
                        let actual = window_identity(window);
                        if actual != expected_window || !token_prepaint.permits(stamp, actual) {
                            diag_prepaint.borrow_mut().reject_stamp(
                                &token_prepaint,
                                stamp,
                                actual,
                                expected_window,
                                "SetViewport",
                            );
                            return None;
                        }
                        let scale = window.scale_factor();
                        let next = match viewport_for(bounds, scale) {
                            Ok(next) => next,
                            Err(error) => {
                                diag_prepaint.borrow_mut().last_error = Some(error.to_string());
                                return None;
                            }
                        };
                        let Some(entity) = weak_context.upgrade() else {
                            return None;
                        };
                        let stamp = match entity.update(app, |view, cx| {
                            view.reconcile_popup_context(next, bounds, window, cx)
                        }) {
                            Ok(stamp) => stamp,
                            Err(error) => {
                                diag_prepaint.borrow_mut().last_error = Some(error.to_string());
                                return None;
                            }
                        };
                        {
                            let mut diag = diag_prepaint.borrow_mut();
                            let rect = bounds_dip(bounds);
                            diag.surface_bounds = rect;
                            diag.scale_factor = scale;
                            drop_surface_geometry.set((rect.0, rect.1, rect.2, rect.3, scale));
                            crate::native_view::update_surface_cursor_bounds(
                                window,
                                browser_key,
                                surface_bounds_to_px(rect, scale),
                            );
                        }
                        entity.update(app, |view, cx| view.refresh_ime(window, cx));
                        if !handle.is_fast_input_ready()
                            && handle.snapshot().state != SessionState::Alive
                        {
                            return Some(stamp);
                        }
                        // Public GPUI hitbox belongs to this actual prepaint frame.
                        // Capture drag_move bounds alone do not exclude host overlays.
                        if token_prepaint.permits(stamp, actual) {
                            let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
                            *file_hitbox.borrow_mut() = Some((stamp, hitbox));
                        }
                        let minimized = crate::native_view::is_gpui_window_minimized(window);
                        if minimized != minimized_hidden.get()
                            && handle
                                .submit(BrowserCommand::SetVisible(!minimized))
                                .is_ok()
                        {
                            minimized_hidden.set(minimized);
                        }
                        let in_sizemove = crate::native_view::is_window_in_sizemove(window);
                        if !in_sizemove || minimized {
                            in_flight_viewport.set(None);
                        }
                        let client_offset =
                            (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
                        if sent_client_offset.get() != Some(client_offset)
                            && handle
                                .submit(BrowserCommand::SetClientOffset {
                                    x_dip: client_offset.0,
                                    y_dip: client_offset.1,
                                })
                                .is_ok()
                        {
                            sent_client_offset.set(Some(client_offset));
                        }
                        if !in_sizemove
                            && !minimized
                            && let Some((cx_px, cy_px)) =
                                crate::native_view::window_client_screen_origin(window)
                        {
                            let ox =
                                cx_px.saturating_add(
                                    (f32::from(bounds.origin.x) * scale).round() as i32
                                );
                            let oy =
                                cy_px.saturating_add(
                                    (f32::from(bounds.origin.y) * scale).round() as i32
                                );
                            let screen_origin = (ox, oy);
                            if sent_screen_origin.get() != Some(screen_origin)
                                && handle
                                    .submit(BrowserCommand::SetScreenOrigin {
                                        x_px: screen_origin.0,
                                        y_px: screen_origin.1,
                                    })
                                    .is_ok()
                            {
                                sent_screen_origin.set(Some(screen_origin));
                            }
                        }
                        if minimized || sent_viewport.get() == Some(next) {
                            return Some(stamp);
                        }
                        const IN_FLIGHT_VIEWPORT_TIMEOUT: std::time::Duration =
                            std::time::Duration::from_millis(180);
                        if in_sizemove
                            && let Some((inflight_vp, sent_at)) = in_flight_viewport.get()
                            && inflight_vp.scale == next.scale
                            && sent_at.elapsed() < IN_FLIGHT_VIEWPORT_TIMEOUT
                        {
                            return Some(stamp);
                        }
                        // Re-check immediately before handing the command to the
                        // Runtime: a retirement/replacement between render and
                        // prepaint must not post a stale viewport.
                        if !token_prepaint.permits(stamp, actual) {
                            let mut diag = diag_prepaint.borrow_mut();
                            diag.rejected_paints = diag.rejected_paints.saturating_add(1);
                            diag.last_error =
                                Some("SetViewport skipped: paint stamp was invalidated".to_owned());
                            return None;
                        }
                        // Owned command to the Runtime; no CEF object is touched here
                        // or in paint. Accepted values are remembered so a stable
                        // layout does not resubmit the same viewport every frame.
                        match handle.submit(BrowserCommand::SetViewport(next)) {
                            Ok(_) => {
                                sent_viewport.set(Some(next));
                                if in_sizemove {
                                    in_flight_viewport.set(Some((next, std::time::Instant::now())));
                                } else {
                                    in_flight_viewport.set(None);
                                }
                            }
                            Err(error) => {
                                diag_prepaint.borrow_mut().last_error =
                                    Some(format!("SetViewport rejected: {error:?}"));
                            }
                        }
                        Some(stamp)
                    },
                    move |bounds, prepaint_stamp: Option<PaintStamp>, window, app| {
                        // A paint-phase attempt is not confirmation GPUI installed a
                        // platform handler or delivered WM_CHAR to this entity.
                        if focus.is_focused(window) {
                            input_paint
                                .borrow_mut()
                                .note_focused_handler_paint_attempt();
                        }
                        window.handle_input(
                            &focus,
                            ElementInputHandler::new(bounds, entity.clone()),
                            app,
                        );
                        // Window capture receives outside release for an owned gesture;
                        // no overlay may start a new CEF gesture through these listeners.
                        let down = global_down.clone();
                        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, app| {
                            if phase == DispatchPhase::Capture {
                                if let Some(entity) = down.upgrade() {
                                    entity.update(app, |view, cx| {
                                        if view.active_drag.is_some() {
                                            view.finish_active_osr_drag(
                                                event.position,
                                                event.modifiers,
                                                true,
                                                window,
                                                cx,
                                            );
                                        }
                                        // An overlapping/overlay down invalidates an earlier held gesture.
                                        let has_ownership = {
                                            let input = view.input.borrow();
                                            input.held_button() || input.pending()
                                        };
                                        if has_ownership {
                                            let pending = view.input.borrow().pending();
                                            if pending {
                                                view.input.borrow_mut().note_overflow();
                                                view.clear_local_input();
                                            } else {
                                                view.reset_input(InputResetReason::FocusLost);
                                            }
                                        }
                                        view.click.borrow_mut().cancel();
                                        if !crate::native_view::is_window_effectively_active(window)
                                        {
                                            if view.attachment.native_context_menu().is_open() {
                                                view.cef_focus_accepted.set(false);
                                                view.right_mouse_down = false;
                                            } else {
                                                view.reset_input(InputResetReason::FocusLost);
                                            }
                                        }
                                    });
                                }
                            }
                        });
                        let file_exit = entity.downgrade();
                        window.on_mouse_event(move |event: &FileDropEvent, phase, window, app| {
                            if phase == DispatchPhase::Capture
                                && matches!(event, FileDropEvent::Exited | FileDropEvent::Ended)
                            {
                                let _ = file_exit.update(app, |view, _| {
                                    if view.check_window(window).is_ok() {
                                        view.cancel_file_drag();
                                    }
                                });
                            }
                        });
                        let file_up = entity.downgrade();
                        window.on_mouse_event(move |_event: &MouseUpEvent, phase, window, app| {
                            if phase != DispatchPhase::Capture {
                                return;
                            }
                            let serial = file_up
                                .update(app, |view, _| view.file_release_fence(window))
                                .ok()
                                .flatten();
                            if let Some(serial) = serial {
                                let weak = file_up.clone();
                                // One finite post-dispatch fence, NOT an input retry/receiver consumer.
                                app.defer(move |app| {
                                    let _ = weak
                                        .update(app, |view, _| view.finish_file_release(serial));
                                });
                            }
                        });
                        let up = weak_up.clone();
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, app| {
                            if phase == DispatchPhase::Capture {
                                if let Some(entity) = up.upgrade() {
                                    entity
                                        .update(app, |view, cx| view.pointer_up(event, window, cx));
                                }
                            }
                        });
                        let moving = weak_move.clone();
                        let input_move = input_paint.clone();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, app| {
                            if phase == DispatchPhase::Capture {
                                if drag_flag_move.get() {
                                    if let Some(entity) = moving.upgrade() {
                                        entity.update(app, |view, cx| {
                                            view.drag_pointer_move(event, window, cx)
                                        });
                                    }
                                } else if input_move.borrow().held_button()
                                    && !app.has_active_drag()
                                {
                                    if let Some(entity) = moving.upgrade() {
                                        entity.update(app, |view, _| {
                                            view.pointer_move(event, window, true)
                                        });
                                    }
                                }
                            }
                        });
                        let Some(prepaint_stamp) = prepaint_stamp else {
                            return;
                        };
                        for (frame, is_popup) in [(paint_frame, false), (popup_frame, true)] {
                            let Some(frame) = frame else {
                                continue;
                            };
                            let check_stamp = if is_popup {
                                frame.stamp
                            } else {
                                prepaint_stamp
                            };
                            // Re-check before reading the image or importing it into the
                            // atlas. An already-alive image Arc is not proof of liveness.
                            let actual = window_identity(window);
                            if actual != expected_window
                                || !token_paint.permits(check_stamp, actual)
                            {
                                diag_paint.borrow_mut().reject_stamp(
                                    &token_paint,
                                    check_stamp,
                                    actual,
                                    expected_window,
                                    "paint_image",
                                );
                                if is_popup {
                                    continue;
                                }
                                return;
                            }
                            let scale = window.scale_factor();
                            let (frame_w, frame_h) = frame.content.size_px();
                            let target = if is_popup {
                                frame_target(
                                    popup_origin(bounds.origin, popup_bounds),
                                    frame_w,
                                    frame_h,
                                    scale,
                                )
                            } else {
                                align_view_frame_target(
                                    bounds,
                                    frame_w,
                                    frame_h,
                                    scale,
                                    frame.captured_scale,
                                )
                            };
                            let target = match target {
                                Ok(target) => target,
                                Err(error) => {
                                    diag_paint.borrow_mut().last_error = Some(error.to_string());
                                    return;
                                }
                            };
                            let has_transform = rotation_angle_deg.abs() > 1e-4;
                            if has_transform {
                                if f32::from(bounds.size.width) <= 0.0
                                    || f32::from(bounds.size.height) <= 0.0
                                    || f32::from(target.size.width) <= 0.0
                                    || f32::from(target.size.height) <= 0.0
                                {
                                    continue;
                                }
                            } else {
                                let visible = bounds.intersect(&target);
                                if f32::from(visible.size.width) <= 0.0
                                    || f32::from(visible.size.height) <= 0.0
                                {
                                    continue;
                                }
                            }
                            if !token_paint.permits(check_stamp, window_identity(window)) {
                                diag_paint.borrow_mut().reject_stamp(
                                    &token_paint,
                                    check_stamp,
                                    window_identity(window),
                                    expected_window,
                                    "paint_image",
                                );
                                continue;
                            }
                            let (rad, fit_scale) = compute_surface_transform_params(
                                bounds_dip(bounds),
                                rotation_angle_deg,
                                fit_rotated_to_bounds,
                            );
                            let transformation = if has_transform {
                                let cx_px = (f32::from(bounds.origin.x)
                                    + f32::from(bounds.size.width) * 0.5)
                                    * scale;
                                let cy_px = (f32::from(bounds.origin.y)
                                    + f32::from(bounds.size.height) * 0.5)
                                    * scale;
                                let center = gpui::point(
                                    gpui::ScaledPixels(cx_px),
                                    gpui::ScaledPixels(cy_px),
                                );
                                let neg_center = gpui::point(
                                    gpui::ScaledPixels(-cx_px),
                                    gpui::ScaledPixels(-cy_px),
                                );
                                gpui::TransformationMatrix::unit()
                                    .translate(center)
                                    .rotate(gpui::Radians(rad))
                                    .scale(gpui::size(fit_scale, fit_scale))
                                    .translate(neg_center)
                            } else {
                                gpui::TransformationMatrix::unit()
                            };
                            // `bounds` clips; `target` keeps the frame at its captured
                            // device-pixel size divided by the window scale, anchored at
                            // the surface origin. No stretching of a stale image.
                            let paint_result = match &frame.content {
                                PresentedContent::CpuImage(image) => window
                                    .paint_image_transformed(
                                        bounds,
                                        target,
                                        Corners::default(),
                                        image.clone(),
                                        0,
                                        false,
                                        Some(surface_opacity),
                                        transformation,
                                        nearest_neighbor_scaling,
                                    ),
                                PresentedContent::GpuTexture(owned) => {
                                    #[cfg(target_os = "windows")]
                                    {
                                        if let Some(gpu) = owned.gpu_texture() {
                                            window.paint_shared_texture_transformed(
                                                bounds,
                                                target,
                                                gpui::WindowsSharedTexture {
                                                    pool_id: gpu.pool_id,
                                                    slot_index: gpu.slot_index,
                                                    generation: gpu.generation,
                                                    shared_handle: gpu.shared_handle as usize,
                                                    is_nt_handle: gpu.is_nt_handle,
                                                    keyed_mutex_key: gpu.keyed_mutex_key,
                                                    width: gpu.width,
                                                    height: gpu.height,
                                                    src_origin: gpui::point(
                                                        gpui::DevicePixels(0),
                                                        gpui::DevicePixels(0),
                                                    ),
                                                    src_size: gpui::size(
                                                        gpui::DevicePixels(gpu.width as i32),
                                                        gpui::DevicePixels(gpu.height as i32),
                                                    ),
                                                    resource_keepalive: gpu
                                                        .resource_keepalive
                                                        .clone(),
                                                    handle_guard: Some(Arc::clone(
                                                        &gpu.handle_owner,
                                                    )),
                                                },
                                                Some(surface_opacity),
                                                transformation,
                                                nearest_neighbor_scaling,
                                            );
                                        }
                                        Ok(())
                                    }
                                    #[cfg(not(target_os = "windows"))]
                                    {
                                        let _ = owned;
                                        Ok(())
                                    }
                                }
                            };
                            match paint_result {
                                Ok(()) => {
                                    let mut diag = diag_paint.borrow_mut();
                                    if is_popup {
                                        diag.popup_calls = diag.popup_calls.saturating_add(1);
                                        diag.popup_sequence = frame.sequence;
                                        diag.popup_generation = frame.generation;
                                        diag.popup_bounds = bounds_dip(target);
                                    } else {
                                        diag.paint_calls = diag.paint_calls.saturating_add(1);
                                        diag.painted_sequence = frame.sequence;
                                        diag.painted_generation = frame.generation;
                                        #[cfg(feature = "validation")]
                                        if let Some(timing) = diag.view_timing.as_mut()
                                            && timing.generation == frame.generation
                                            && timing.last_sequence < frame.sequence
                                            && timing.samples.len()
                                                < crate::test_support::VIEW_COPY_SAMPLE_CAPACITY
                                        {
                                            timing.last_sequence = frame.sequence;
                                            timing.samples.push(ViewPaintSample {
                                                browser: token_paint.binding().browser,
                                                window: actual,
                                                generation: frame.generation,
                                                sequence: frame.sequence,
                                                width: frame_w,
                                                height: frame_h,
                                                paint_finished_at: std::time::Instant::now(),
                                            });
                                        }
                                    }
                                }
                                Err(error) => {
                                    diag_paint.borrow_mut().last_error.get_or_insert_with(|| {
                                        format!("paint_image failed: {error:#}")
                                    });
                                }
                            }
                        }
                        if let Some(overlay) = drag_overlay
                            && overlay.preview_size_dip.0 > 0.0
                            && overlay.preview_size_dip.1 > 0.0
                            && token_paint.permits(prepaint_stamp, window_identity(window))
                        {
                            let ghost_bounds = Bounds {
                                origin: gpui::point(
                                    px(overlay.cursor_win_dip.0 - overlay.hotspot_dip.0),
                                    px(overlay.cursor_win_dip.1 - overlay.hotspot_dip.1),
                                ),
                                size: size(
                                    px(overlay.preview_size_dip.0),
                                    px(overlay.preview_size_dip.1),
                                ),
                            };
                            let client_bounds = Bounds {
                                origin: gpui::point(px(0.0), px(0.0)),
                                size: window.viewport_size(),
                            };
                            let visible = client_bounds.intersect(&ghost_bounds);
                            if f32::from(visible.size.width) > 0.0
                                && f32::from(visible.size.height) > 0.0
                            {
                                let _ = window.paint_image(
                                    visible,
                                    ghost_bounds,
                                    Corners::default(),
                                    overlay.preview_image,
                                    0,
                                    false,
                                );
                            }
                        }
                    },
                )
                .size_full(),
            )
            .when(
                self.active_drag.is_none()
                    && !self.ux.tooltip.is_empty()
                    && !self.popup_visible
                    && !self.input.borrow().held_button()
                    && self.last_hover_dip.is_some(),
                |el| {
                    let (hx, hy) = self.last_hover_dip.unwrap_or((16.0, 16.0));
                    let (_, _, sw, sh) = self.paint.borrow().surface_bounds;
                    let max_x = if sw > 260.0 { sw - 260.0 } else { 4.0 };
                    let max_y = if sh > 36.0 { sh - 36.0 } else { 4.0 };
                    let left = (hx + 12.0).clamp(4.0, max_x.max(4.0));
                    let top = (hy + 20.0).clamp(4.0, max_y.max(4.0));
                    el.child(
                        div()
                            .absolute()
                            .left(px(left))
                            .top(px(top))
                            .max_w(px(420.0))
                            .px_2()
                            .py_1()
                            .rounded(px(4.0))
                            .bg(rgb(0x1e222a))
                            .border_1()
                            .border_color(rgb(0x3e4452))
                            .text_xs()
                            .text_color(rgb(0xf0f3f8))
                            .child(self.ux.tooltip.clone()),
                    )
                },
            )
            .when_some(drag_fallback_card, |el, (cx_dip, cy_dip, label)| {
                let (sx, sy, _, _) = self.paint.borrow().surface_bounds;
                el.child(
                    div()
                        .absolute()
                        .left(px((cx_dip - sx + 14.0).max(0.0)))
                        .top(px((cy_dip - sy + 14.0).max(0.0)))
                        .max_w(px(280.0))
                        .px_2()
                        .py_1()
                        .rounded(px(4.0))
                        .bg(gpui::rgba(0x1e222ad8))
                        .border_1()
                        .border_color(gpui::rgba(0x94a3b8aa))
                        .text_xs()
                        .text_color(rgb(0xf8fafc))
                        .child(label),
                )
            })
            .into_any_element()
    }
}

fn apply_mirror_state(
    composition: &mut Option<String>,
    selection: &mut std::ops::Range<usize>,
    effect: Option<MirrorEffect>,
) {
    match effect {
        Some(MirrorEffect::Mark {
            text,
            selection: next,
        }) => {
            *composition = Some(text);
            *selection = next;
        }
        Some(MirrorEffect::Clear) => {
            *composition = None;
            *selection = 0..0;
        }
        None => {}
    }
}

fn input_geometry_changed(
    previous: Option<(f32, f32, f32, f32)>,
    next: (f32, f32, f32, f32),
) -> bool {
    previous.is_some_and(|previous| previous != next)
}

fn bounded_replacement(range: std::ops::Range<usize>) -> Option<Utf16Range> {
    if range.start > range.end || range.end > MAX_INPUT_RANGE_UTF16 {
        return None;
    }
    Some(Utf16Range {
        start: u32::try_from(range.start).ok()?,
        end: u32::try_from(range.end).ok()?,
    })
}

pub(crate) fn compute_surface_transform_params(
    bounds_dip: (f32, f32, f32, f32),
    angle_deg: f32,
    fit_to_bounds: bool,
) -> (f32, f32) {
    if !angle_deg.is_finite() || angle_deg.abs() <= 1e-4 {
        return (0.0, 1.0);
    }
    let rad = angle_deg.to_radians();
    let (_, _, w, h) = bounds_dip;
    if !fit_to_bounds || !w.is_finite() || !h.is_finite() || w <= 0.0 || h <= 0.0 {
        return (rad, 1.0);
    }
    let c = rad.cos().abs();
    let s = rad.sin().abs();
    let denom_x = w * c + h * s;
    let denom_y = w * s + h * c;
    let scale = if denom_x > 1e-4 && denom_y > 1e-4 {
        (w / denom_x).min(h / denom_y).clamp(0.05, 1.0)
    } else {
        1.0
    };
    (rad, scale)
}

pub(crate) fn inverse_transform_surface_point(
    bounds: (f32, f32, f32, f32),
    position: (f32, f32),
    angle_deg: f32,
    fit_to_bounds: bool,
) -> (f32, f32) {
    let (x, y, width, height) = bounds;
    let (px, py) = position;
    if !angle_deg.is_finite()
        || angle_deg.abs() <= 1e-4
        || !width.is_finite()
        || !height.is_finite()
        || width <= 0.0
        || height <= 0.0
        || !px.is_finite()
        || !py.is_finite()
    {
        return position;
    }
    let (rad, fit_scale) = compute_surface_transform_params(bounds, angle_deg, fit_to_bounds);
    let cx = x + width * 0.5;
    let cy = y + height * 0.5;
    let dx = (px - cx) / fit_scale;
    let dy = (py - cy) / fit_scale;
    let (sin_r, cos_r) = rad.sin_cos();
    let ux = dx * cos_r + dy * sin_r;
    let uy = -dx * sin_r + dy * cos_r;
    (cx + ux, cy + uy)
}

fn translate_point_transformed(
    bounds: (f32, f32, f32, f32),
    position: (f32, f32),
    angle_deg: f32,
    fit_to_bounds: bool,
    outside: bool,
) -> Option<(i32, i32)> {
    let mapped = inverse_transform_surface_point(bounds, position, angle_deg, fit_to_bounds);
    translate_point(bounds, mapped, outside)
}

fn translate_point(
    bounds: (f32, f32, f32, f32),
    position: (f32, f32),
    outside: bool,
) -> Option<(i32, i32)> {
    let (x, y, width, height) = bounds;
    let (px, py) = position;
    if ![x, y, width, height, px, py, x + width, y + height]
        .into_iter()
        .all(f32::is_finite)
        || width <= 0.
        || height <= 0.
        || width >= i32::MAX as f32
        || height >= i32::MAX as f32
        || (px - x).abs() >= i32::MAX as f32
        || (py - y).abs() >= i32::MAX as f32
        || (!outside && (px < x || px >= x + width || py < y || py >= y + height))
    {
        return None;
    }
    let (vx, vy) = keymap::to_view_point(position, (x, y), (width, height))?;
    if outside {
        Some((vx, vy))
    } else {
        let max_x = (width.round() as i32 - 1).max(0);
        let max_y = (height.round() as i32 - 1).max(0);
        Some((vx.clamp(0, max_x), vy.clamp(0, max_y)))
    }
}

fn normalize_click_count(button: InputButton, raw_count: u8) -> u8 {
    if button == InputButton::Right || raw_count > 3 {
        1
    } else {
        raw_count
    }
}

fn os_any_mouse_button_down() -> bool {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetKeyState(nVirtKey: i32) -> i16;
    }
    // VK_LBUTTON (0x01), VK_RBUTTON (0x02), VK_MBUTTON (0x04),
    // VK_XBUTTON1 (0x05), VK_XBUTTON2 (0x06)
    unsafe {
        GetKeyState(0x01) < 0
            || GetKeyState(0x02) < 0
            || GetKeyState(0x04) < 0
            || GetKeyState(0x05) < 0
            || GetKeyState(0x06) < 0
    }
}

fn input_button(button: MouseButton) -> Option<InputButton> {
    match button {
        MouseButton::Left => Some(InputButton::Left),
        MouseButton::Middle => Some(InputButton::Middle),
        MouseButton::Right => Some(InputButton::Right),
        MouseButton::Navigate(gpui::NavigationDirection::Back) => Some(InputButton::Back),
        MouseButton::Navigate(gpui::NavigationDirection::Forward) => Some(InputButton::Forward),
    }
}

fn mouse_modifiers(modifiers: gpui::Modifiers) -> MouseModifiers {
    MouseModifiers {
        shift: modifiers.shift,
        control: modifiers.control,
        alt: modifiers.alt,
        platform: modifiers.platform,
    }
}

pub(crate) fn compute_pinch_zoom(current_level: f64, delta: f32) -> Option<f64> {
    if !current_level.is_finite() || !delta.is_finite() || delta == 0.0 {
        return None;
    }
    let factor = (1.0 + f64::from(delta)).max(0.05);
    let next = current_level + factor.ln() / 1.2_f64.ln();
    Some(next.clamp(MIN_ZOOM_LEVEL, MAX_ZOOM_LEVEL))
}

pub(crate) fn align_view_frame_target(
    bounds: Bounds<Pixels>,
    frame_w: u32,
    frame_h: u32,
    window_scale: f32,
    captured_scale: Option<f32>,
) -> Result<Bounds<Pixels>, ViewError> {
    let window_scale = checked_scale(window_scale)?;
    let raw = frame_target(bounds.origin, frame_w, frame_h, window_scale)?;
    let bw = f32::from(bounds.size.width);
    let bh = f32::from(bounds.size.height);
    if bw.is_finite() && bh.is_finite() && bw > 0.0 && bh > 0.0 {
        // Transitional frame captured at a previous monitor DPI: if it filled the
        // container at its capture scale, fill the container bounds instead of
        // shrinking into the top-left corner until the next DPI frame arrives.
        if let Some(cap) = captured_scale
            && cap.is_finite()
            && cap > 0.0
            && (cap - window_scale).abs() > 1e-3
        {
            let cap_w = frame_w as f32 / cap;
            let cap_h = frame_h as f32 / cap;
            if (cap_w - bw).abs() <= 2.0 && (cap_h - bh).abs() <= 2.0 {
                return Ok(bounds);
            }
        }
        // Same-DPI ceil/subpixel rounding tolerance (e.g. 1.25 / 1.5 / 1.75 DPI).
        if (f32::from(raw.size.width) - bw).abs() <= 1.5
            && (f32::from(raw.size.height) - bh).abs() <= 1.5
        {
            return Ok(bounds);
        }
    }
    Ok(raw)
}

fn reconcile_zoom_from_snapshot(
    nav_zoom_milli: i32,
    now: Instant,
    ux: &mut ViewUxState,
    pending_zoom: &mut Option<PendingZoom>,
) {
    if let Some(pending) = *pending_zoom {
        if (nav_zoom_milli - pending.target_milli).abs() <= 2 {
            ux.zoom_level_milli = nav_zoom_milli;
            *pending_zoom = None;
            return;
        }
        if now.saturating_duration_since(pending.submitted_at) >= Duration::from_millis(500) {
            ux.zoom_level_milli = nav_zoom_milli;
            *pending_zoom = None;
            return;
        }
        ux.zoom_level_milli = pending.target_milli;
        return;
    }
    ux.zoom_level_milli = nav_zoom_milli;
}

/// One snapshot/update read used by `consume`.
///
/// `Resync` is deliberately distinguishable from `Update`: a snapshot attempt
/// must not be counted as a new CEF frame. Only when there is no pending update
/// and the previous conversion asked for a retry is `snapshot()` consulted.
enum SourcePoll {
    Update(FrameUpdate),
    Resync(FrameUpdate),
    Empty,
}

fn poll_source(source: &dyn FrameSource, needs_resync: bool) -> SourcePoll {
    if let Some(update) = source.take_update() {
        return SourcePoll::Update(update);
    }
    if needs_resync {
        return SourcePoll::Resync(source.snapshot());
    }
    SourcePoll::Empty
}

/// Replaces or clears an image slot as one transaction.
///
/// The old owner is cloned for the window operation and remains in `slot`
/// until that operation succeeds. On failure the complete old slot (image and
/// metadata) is restored and the candidate is dropped; the caller reports the
/// error instead of accepting a half-updated image. `drop_old` is injected so
/// the real view path and the unit test exercise the same implementation.
fn commit_image_change(
    slot: &mut Option<ImageSlot>,
    candidate: Option<ImageSlot>,
    mut drop_old: impl FnMut(Arc<RenderImage>) -> Result<(), String>,
) -> Result<bool, String> {
    let mark_candidate_presenting = |cand: &Option<ImageSlot>| {
        if let Some(next) = cand
            && let PresentedContent::GpuTexture(frame) = &next.content
            && let Some(gpu) = frame.gpu_texture()
        {
            gpu.lease.tracker().mark_presenting(gpu.slot_index);
        }
    };
    let Some(old) = slot.take() else {
        mark_candidate_presenting(&candidate);
        *slot = candidate;
        return Ok(false);
    };
    let drop_res = match &old.content {
        PresentedContent::CpuImage(image) => drop_old(image.clone()),
        PresentedContent::GpuTexture(_) => Ok(()),
    };
    match drop_res {
        Ok(()) => {
            mark_candidate_presenting(&candidate);
            *slot = candidate;
            Ok(true)
        }
        Err(error) => {
            *slot = Some(old);
            Err(error)
        }
    }
}

fn drop_atlas_image(window: &mut Window, image: Arc<RenderImage>) -> Result<(), String> {
    window
        .drop_image(image)
        .map_err(|error| format!("drop_image failed: {error:#}"))
}

/// Builds the real `RenderImage` from a budgeted frame.
///
/// The exclusive CPU copy is reserved from the Browser's ledger, then moved
/// into `image::Frame` via `try_into_owner`. The ledger keeps a strong owner
/// reference, so the allocation is refunded only after the last `RenderImage`
/// Arc is actually destroyed. This mirrors the verified root UI adapter and
/// adds no parallel frame format or zero-copy assumption.
#[cfg(test)]
fn render_image_from_frame(frame: &OwnedFrame) -> Result<Arc<RenderImage>, FrameError> {
    render_image_from_surface(frame, SurfaceKind::View)
}

fn render_image_from_surface(
    frame: &OwnedFrame,
    kind: SurfaceKind,
) -> Result<Arc<RenderImage>, FrameError> {
    let info = frame.info;
    if info.surface != kind || info.format != PixelFormat::PremultipliedBgra8 {
        return Err(FrameError::WrongIdentity);
    }
    if info.width == 0 || info.height == 0 || info.sequence == 0 {
        return Err(FrameError::InvalidDimensions);
    }
    let expected = (info.width as usize)
        .checked_mul(info.height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(FrameError::InvalidDimensions)?;
    let pixels = frame.cpu_pixels().ok_or(FrameError::WrongIdentity)?;
    if pixels.len() != expected {
        return Err(FrameError::InvalidDimensions);
    }
    let (width, height) = (info.width, info.height);
    pixels.copy_buffer()?.try_into_owner(|pixels| {
        let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(width, height, pixels)
            .ok_or(FrameError::InvalidDimensions)?;
        Ok::<_, FrameError>(Arc::new(RenderImage::new(smallvec![Frame::new(buffer)])))
    })
}

fn image_slot_from_frame(frame: &Arc<OwnedFrame>) -> Result<ImageSlot, FrameError> {
    image_slot_from_surface(frame, SurfaceKind::View)
}

fn image_slot_from_surface(
    frame: &Arc<OwnedFrame>,
    kind: SurfaceKind,
) -> Result<ImageSlot, FrameError> {
    match &frame.payload {
        FramePayload::CpuBgra { .. } => {
            image_slot_with_image(frame, render_image_from_surface(frame, kind)?)
        }
        FramePayload::GpuSharedTexture(gpu) => {
            let info = frame.info;
            if info.surface != kind || info.format != PixelFormat::PremultipliedBgra8 {
                return Err(FrameError::WrongIdentity);
            }
            if info.width == 0
                || info.height == 0
                || info.sequence == 0
                || gpu.width != info.width
                || gpu.height != info.height
                || gpu.shared_handle == 0
            {
                return Err(FrameError::InvalidDimensions);
            }
            Ok(ImageSlot {
                content: PresentedContent::GpuTexture(Arc::clone(frame)),
                sequence: info.sequence,
                generation: info.surface_generation,
                width: info.width,
                height: info.height,
                captured_scale: None,
                backdrop_rgb: 0xffffff,
            })
        }
    }
}

fn sample_frame_backdrop_rgb(frame: &OwnedFrame) -> u32 {
    let w = frame.info.width as usize;
    let h = frame.info.height as usize;
    let Some(bytes) = frame.cpu_pixels().map(|buffer| buffer.as_slice()) else {
        return 0xffffff;
    };
    if w == 0 || h == 0 || bytes.len() < w.saturating_mul(h).saturating_mul(4) {
        return 0xffffff;
    }
    let read_px = |x: usize, y: usize| -> u32 {
        let idx = (y * w + x) * 4;
        let (b, g, r, a) = (bytes[idx], bytes[idx + 1], bytes[idx + 2], bytes[idx + 3]);
        if a == 0 {
            return 0xffffff;
        }
        if a == 255 {
            return ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
        }
        let unprem = |c: u8| ((c as u32 * 255 + (a as u32 / 2)) / a as u32).min(255);
        (unprem(r) << 16) | (unprem(g) << 8) | unprem(b)
    };
    let c0 = read_px(w - 1, h / 2);
    let c1 = read_px(w / 2, h - 1);
    let c2 = read_px(w - 1, h - 1);
    if c0 == c1 || c0 == c2 { c0 } else { c1 }
}

fn image_slot_with_image(
    frame: &OwnedFrame,
    image: Arc<RenderImage>,
) -> Result<ImageSlot, FrameError> {
    Ok(ImageSlot {
        content: PresentedContent::CpuImage(image),
        sequence: frame.info.sequence,
        generation: frame.info.surface_generation,
        width: frame.info.width,
        height: frame.info.height,
        captured_scale: None,
        backdrop_rgb: sample_frame_backdrop_rgb(frame),
    })
}

/// Whether a retained frame may be presented for this exact VIEW target.
/// A frame from another Browser, surface, generation or pixel target is stale
/// and must not replace good pixels.
fn frame_is_current(browser: BrowserId, view: &SurfaceSnapshot, info: &FrameInfo) -> bool {
    surface_frame_is_current(browser, view, info, SurfaceKind::View)
}

fn surface_frame_is_current(
    browser: BrowserId,
    view: &SurfaceSnapshot,
    info: &FrameInfo,
    kind: SurfaceKind,
) -> bool {
    info.browser == browser
        && info.surface == kind
        && info.surface_generation == view.generation
        && info.width == view.pixel_width
        && info.height == view.pixel_height
        && info.format == PixelFormat::PremultipliedBgra8
        && info.sequence != 0
}

/// VIEW DIP extent uses the original adapter's rounding. Invalid platform
/// geometry is refused rather than silently inventing a scale of one.
fn viewport_for(bounds: Bounds<Pixels>, scale: f32) -> Result<Viewport, ViewError> {
    let width = f32::from(bounds.size.width).round().max(1.0);
    let height = f32::from(bounds.size.height).round().max(1.0);
    if !f32::from(bounds.size.width).is_finite()
        || !f32::from(bounds.size.height).is_finite()
        || width >= i32::MAX as f32
        || height >= i32::MAX as f32
    {
        return Err(ViewError::InvalidGeometry);
    }
    Ok(Viewport {
        width: width as i32,
        height: height as i32,
        scale: checked_scale(scale)?,
    })
}

/// 1:1 placement of a captured frame inside the surface. Device pixels divided
/// by the window scale give the DIP box the frame was painted for.
fn frame_target(
    origin: Point<Pixels>,
    width: u32,
    height: u32,
    scale: f32,
) -> Result<Bounds<Pixels>, ViewError> {
    let scale = checked_scale(scale)?;
    Ok(Bounds {
        origin,
        size: size(px(width as f32 / scale), px(height as f32 / scale)),
    })
}

fn source_matches_viewport(source: &FrameUpdate, target: Viewport) -> bool {
    let width = (target.width as f32 * target.scale).ceil();
    let height = (target.height as f32 * target.scale).ceil();
    source.viewport == Some(target)
        && width.is_finite()
        && height.is_finite()
        && width > 0.
        && height > 0.
        && source.view.bounds.width == target.width
        && source.view.bounds.height == target.height
        && source.view.pixel_width == width as u32
        && source.view.pixel_height == height as u32
}

fn popup_requires_clear(
    old_generation: u64,
    old_bounds: (i32, i32, i32, i32),
    generation: u64,
    bounds: (i32, i32, i32, i32),
    visible: bool,
) -> bool {
    !visible || old_generation != generation || old_bounds != bounds
}

fn popup_origin(origin: Point<Pixels>, bounds: (i32, i32, i32, i32)) -> Point<Pixels> {
    gpui::point(
        origin.x + px(bounds.0 as f32),
        origin.y + px(bounds.1 as f32),
    )
}

pub(crate) fn checked_scale(scale: f32) -> Result<f32, ViewError> {
    if scale.is_finite() && scale > 0.0 {
        Ok(scale)
    } else {
        Err(ViewError::InvalidGeometry)
    }
}

pub(crate) fn window_identity(window: &Window) -> u64 {
    window.window_handle().window_id().as_u64()
}

pub(crate) fn bounds_dip(bounds: Bounds<Pixels>) -> (f32, f32, f32, f32) {
    (
        f32::from(bounds.origin.x),
        f32::from(bounds.origin.y),
        f32::from(bounds.size.width),
        f32::from(bounds.size.height),
    )
}

fn surface_cursor_key(browser: BrowserId) -> u64 {
    browser.runtime().value().rotate_left(32) ^ browser.local()
}

fn surface_bounds_to_px(bounds: (f32, f32, f32, f32), scale: f32) -> (i32, i32, i32, i32) {
    let (x, y, w, h) = bounds;
    if ![x, y, w, h, scale].into_iter().all(f32::is_finite) || scale <= 0.0 || w <= 0.0 || h <= 0.0
    {
        return (0, 0, 0, 0);
    }
    let left = (x * scale).floor() as i32;
    let top = (y * scale).floor() as i32;
    let right = ((x + w) * scale).ceil() as i32;
    let bottom = ((y + h) * scale).ceil() as i32;
    (left, top, right, bottom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::point;
    use rust_cef_core::{
        contracts::RuntimeGeneration,
        frame_budget::FrameBudget,
        mailbox::{FrameMailbox, SurfaceBounds},
    };

    fn browser(local: u64) -> BrowserId {
        BrowserId::new(RuntimeGeneration::new(7), local)
    }

    fn surface(generation: u64, pixel_width: u32, pixel_height: u32) -> SurfaceSnapshot {
        SurfaceSnapshot {
            generation,
            bounds: SurfaceBounds {
                x: 0,
                y: 0,
                width: pixel_width as i32,
                height: pixel_height as i32,
            },
            visible: true,
            pixel_width,
            pixel_height,
            frame: None,
        }
    }

    fn info(browser: BrowserId, generation: u64, width: u32, height: u32) -> FrameInfo {
        FrameInfo {
            browser,
            surface: SurfaceKind::View,
            surface_generation: generation,
            sequence: 9,
            width,
            height,
            format: PixelFormat::PremultipliedBgra8,
        }
    }

    fn test_image(width: u32, height: u32) -> Arc<RenderImage> {
        let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(
            width,
            height,
            vec![0u8; (width as usize) * (height as usize) * 4],
        )
        .expect("test image dimensions are valid");
        Arc::new(RenderImage::new(smallvec![Frame::new(buffer)]))
    }

    #[test]
    fn obsolete_canvas_is_counted_without_erasing_real_errors_or_weakening_identity() {
        let token = WindowToken::new(WindowBinding {
            browser: browser(3),
            window: 7,
        });
        let stamp = token.stamp().unwrap();
        token.advance().unwrap();
        let mut diag = PaintDiagnostics::default();
        diag.reject_stamp(&token, stamp, 7, 7, "paint_image");
        assert_eq!(diag.rejected_paints, 1);
        assert!(diag.last_error.is_none());
        assert!(!token.permits(stamp, 7));
        diag.last_error = Some("real native image failure".into());
        diag.reject_stamp(&token, stamp, 7, 7, "paint_image");
        assert_eq!(diag.rejected_paints, 2);
        assert_eq!(
            diag.last_error.as_deref(),
            Some("real native image failure")
        );
        let mut wrong = PaintDiagnostics::default();
        wrong.reject_stamp(&token, stamp, 8, 7, "paint_image");
        assert_eq!(wrong.rejected_paints, 1);
        assert!(wrong.last_error.is_some());
        token.retire();
        let mut retired = PaintDiagnostics::default();
        retired.reject_stamp(&token, stamp, 7, 7, "paint_image");
        assert_eq!(retired.rejected_paints, 1);
        assert!(retired.last_error.is_some());
    }

    #[test]
    fn mirror_changes_only_when_a_real_admission_returns_its_effect() {
        let mut composition = Some("old".to_owned());
        let mut selection = 0..3;
        let deferred = InputAdmission::Deferred;
        if let InputAdmission::Admitted(effect) = deferred {
            apply_mirror_state(&mut composition, &mut selection, effect);
        }
        assert_eq!(composition.as_deref(), Some("old"));
        assert_eq!(selection, 0..3);
        apply_mirror_state(
            &mut composition,
            &mut selection,
            Some(MirrorEffect::Mark {
                text: "😀".to_owned(),
                selection: 0..2,
            }),
        );
        assert_eq!(composition.as_deref(), Some("😀"));
        assert_eq!(selection, 0..2);
        apply_mirror_state(&mut composition, &mut selection, Some(MirrorEffect::Clear));
        assert_eq!(composition, None);
        assert_eq!(selection, 0..0);
    }

    #[test]
    fn origin_only_change_revokes_input_even_when_viewport_extent_is_equal() {
        assert!(!input_geometry_changed(None, (200., 48., 100., 100.)));
        assert!(!input_geometry_changed(
            Some((200., 48., 100., 100.)),
            (200., 48., 100., 100.)
        ));
        assert!(input_geometry_changed(
            Some((200., 48., 100., 100.)),
            (201., 48., 100., 100.)
        ));
        assert_eq!(
            bounded_replacement(50_000..50_001),
            Some(Utf16Range {
                start: 50_000,
                end: 50_001
            })
        );
        assert_eq!(bounded_replacement(0..MAX_INPUT_RANGE_UTF16 + 1), None);
    }

    #[test]
    fn input_coordinates_are_window_dip_minus_content_origin_once() {
        let bounds = (200., 48., 100., 100.);
        assert_eq!(translate_point(bounds, (265., 88.), false), Some((65, 40)));
        assert_eq!(translate_point(bounds, (310., 180.), false), None);
        assert_eq!(
            translate_point(bounds, (310., 180.), true),
            Some((110, 132))
        );
        assert_eq!(translate_point(bounds, (f32::NAN, 88.), true), None);
    }

    #[test]
    fn popup_geometry_clips_without_repositioning_or_stretching() {
        let content = Bounds {
            origin: point(px(10.), px(20.)),
            size: size(px(40.), px(30.)),
        };
        let target =
            frame_target(popup_origin(content.origin, (-5, 7, 20, 10)), 40, 20, 2.0).unwrap();
        assert_eq!(bounds_dip(target), (5., 27., 20., 10.));
        assert_eq!(bounds_dip(content.intersect(&target)), (10., 27., 15., 10.));
    }

    #[test]
    fn popup_transition_retires_hidden_moved_and_old_generations() {
        let bounds = (1, 2, 3, 4);
        assert!(!popup_requires_clear(4, bounds, 4, bounds, true));
        assert!(popup_requires_clear(4, bounds, 4, bounds, false));
        assert!(popup_requires_clear(4, bounds, 5, bounds, true));
        assert!(popup_requires_clear(4, bounds, 4, (2, 2, 3, 4), true));
        let owner = browser(3);
        let snapshot = surface(5, 2, 2);
        let mut frame = info(owner, 4, 2, 2);
        frame.surface = SurfaceKind::Popup;
        assert!(!surface_frame_is_current(
            owner,
            &snapshot,
            &frame,
            SurfaceKind::Popup
        ));
        frame.surface_generation = 5;
        assert!(surface_frame_is_current(
            owner,
            &snapshot,
            &frame,
            SurfaceKind::Popup
        ));
        assert!(!frame_is_current(owner, &snapshot, &frame));
        // Distinct scales have identical DIP and ceil pixel extents here.
        // Only exact producer-applied context can authorize restoration; queued
        // or rejected desired targets never change the source acknowledgement.
        let a = Viewport {
            width: 1,
            height: 1,
            scale: 1.1,
        };
        let b = Viewport {
            width: 1,
            height: 1,
            scale: 1.2,
        };
        let c = Viewport {
            width: 1,
            height: 1,
            scale: 1.3,
        };
        let source = FrameMailbox::new(owner);
        let target = SurfaceBounds {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        source
            .configure_view_with_scale(target, 2, 2, a.scale)
            .unwrap();
        assert!(source_matches_viewport(&source.snapshot(), a));
        assert!(!source_matches_viewport(&source.snapshot(), b));
        source
            .configure_view_with_scale(target, 2, 2, b.scale)
            .unwrap();
        assert!(source_matches_viewport(&source.snapshot(), b));
        assert!(!source_matches_viewport(&source.snapshot(), c));
        // Unknown legacy metadata cannot be inferred from those same pixels.
        source.configure_view(target, 2, 2).unwrap();
        assert!(!source_matches_viewport(&source.snapshot(), b));
    }

    #[test]
    fn popup_conversion_uses_original_budget() {
        let budget = FrameBudget::new(8);
        let pixels = budget.allocate(4).unwrap();
        let mut identity = info(browser(3), 4, 1, 1);
        identity.surface = SurfaceKind::Popup;
        let frame = OwnedFrame::from_cpu_pixels(identity, pixels.freeze());
        let image = render_image_from_surface(&frame, SurfaceKind::Popup).unwrap();
        assert_eq!(budget.stats().used, 8);
        assert!(matches!(
            render_image_from_surface(&frame, SurfaceKind::Popup),
            Err(FrameError::BudgetExceeded)
        ));
        assert!(matches!(
            render_image_from_frame(&frame),
            Err(FrameError::WrongIdentity)
        ));
        drop(image);
    }

    #[test]
    fn render_image_uses_the_browser_budget_until_its_last_owner_drops() {
        let budget = FrameBudget::new(32);
        let mut pixels = budget.allocate(16).unwrap();
        pixels.as_mut_slice().fill(123);
        let frame = OwnedFrame::from_cpu_pixels(info(browser(3), 4, 2, 2), pixels.freeze());
        let image = render_image_from_frame(&frame).unwrap();
        assert_eq!(budget.stats().used, 32);
        // A second consumer copy must be refused; it cannot escape this ledger.
        assert!(matches!(
            render_image_from_frame(&frame),
            Err(FrameError::BudgetExceeded)
        ));
        assert_eq!(frame.cpu_pixels().unwrap().as_slice(), &[123; 16]);
        drop(frame);
        assert_eq!(budget.stats().used, 16);
        let retained = image.clone();
        drop(image);
        assert_eq!(budget.stats().used, 16);
        drop(retained);
        assert_eq!(budget.stats().used, 0);
    }

    #[test]
    fn viewport_rounds_dip_extent_and_clamps_to_one() {
        let bounds = Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(800.4), px(600.6)),
        };
        let viewport = viewport_for(bounds, 1.5).unwrap();
        assert_eq!(viewport.width, 800);
        assert_eq!(viewport.height, 601);
        assert_eq!(viewport.scale, 1.5);

        let collapsed = Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(0.2), px(0.2)),
        };
        let viewport = viewport_for(collapsed, 1.25).unwrap();
        assert_eq!((viewport.width, viewport.height), (1, 1));
        assert_eq!(viewport.scale, 1.25);
    }

    #[test]
    fn invalid_scale_is_refused_instead_of_reaching_cef() {
        for scale in [0.0, -2.0, f32::NAN, f32::INFINITY] {
            let bounds = Bounds {
                origin: point(px(0.), px(0.)),
                size: size(px(10.), px(10.)),
            };
            assert_eq!(viewport_for(bounds, scale), Err(ViewError::InvalidGeometry));
        }
    }

    #[test]
    fn frame_target_divides_device_pixels_by_scale() {
        let target = frame_target(point(px(5.), px(7.)), 100, 50, 2.0).unwrap();
        assert_eq!(target.origin, point(px(5.), px(7.)));
        assert_eq!(target.size, size(px(50.), px(25.)));
        // A bad window scale must not silently invent another pixel geometry.
        assert_eq!(
            frame_target(point(px(0.), px(0.)), 100, 50, f32::NAN),
            Err(ViewError::InvalidGeometry)
        );
    }

    #[test]
    fn frame_identity_requires_browser_surface_generation_target_and_format() {
        let owner = browser(3);
        let view = surface(4, 320, 200);
        assert!(frame_is_current(owner, &view, &info(owner, 4, 320, 200)));

        assert!(!frame_is_current(
            browser(4),
            &view,
            &info(owner, 4, 320, 200)
        ));
        assert!(!frame_is_current(owner, &view, &info(owner, 3, 320, 200)));
        assert!(!frame_is_current(owner, &view, &info(owner, 4, 640, 400)));

        let mut foreign_surface = info(owner, 4, 320, 200);
        foreign_surface.surface = SurfaceKind::Popup;
        assert!(!frame_is_current(owner, &view, &foreign_surface));

        let mut foreign_format = info(owner, 4, 320, 200);
        foreign_format.format = PixelFormat::PremultipliedBgra8;
        // The only supported format is BGRA; keep the assertion meaningful if
        // another format is ever introduced without updating this view.
        assert!(frame_is_current(owner, &view, &foreign_format));

        let mut zero_sequence = info(owner, 4, 320, 200);
        zero_sequence.sequence = 0;
        assert!(!frame_is_current(owner, &view, &zero_sequence));
    }

    #[test]
    fn failed_atlas_transaction_keeps_old_image_owner_and_metadata() {
        let old_image = test_image(2, 2);
        let candidate_image = test_image(2, 2);
        let mut slot = Some(ImageSlot {
            content: PresentedContent::CpuImage(old_image.clone()),
            sequence: 9,
            generation: 4,
            width: 2,
            height: 2,
            captured_scale: None,
            backdrop_rgb: 0xffffff,
        });
        let candidate = ImageSlot {
            content: PresentedContent::CpuImage(candidate_image.clone()),
            sequence: 10,
            generation: 4,
            width: 2,
            height: 2,
            captured_scale: None,
            backdrop_rgb: 0xffffff,
        };

        let error = commit_image_change(&mut slot, Some(candidate), |_| {
            Err("drop_image failed".to_owned())
        })
        .expect_err("a failed old atlas removal is an error");
        assert_eq!(error, "drop_image failed");

        let kept = slot.as_ref().expect("old image is restored");
        assert_eq!((kept.sequence, kept.generation), (9, 4));
        assert_eq!((kept.width, kept.height), (2, 2));
        assert!(matches!(
            &kept.content,
            PresentedContent::CpuImage(img) if Arc::ptr_eq(img, &old_image)
        ));
        assert_eq!(
            Arc::strong_count(&candidate_image),
            1,
            "the rejected candidate owner is dropped"
        );
        assert_eq!(
            Arc::strong_count(&old_image),
            2,
            "the old owner stays in the slot and the test"
        );

        let error = commit_image_change(&mut slot, None, |_| Err("drop_image failed".to_owned()))
            .expect_err("a failed clear is an error");
        assert_eq!(error, "drop_image failed");
        let kept = slot.as_ref().expect("old image survives a failed clear");
        assert_eq!((kept.sequence, kept.generation), (9, 4));
        assert!(matches!(
            &kept.content,
            PresentedContent::CpuImage(img) if Arc::ptr_eq(img, &old_image)
        ));
    }

    #[test]
    fn failed_conversion_keeps_the_previous_slot_and_owner() {
        // 8 bytes total: the published frame holds 4 and the first presentation
        // copy takes the remaining 4, so a second copy is a real budget failure.
        let budget = FrameBudget::new(8);
        let mut pixels = budget.allocate(4).unwrap();
        pixels.as_mut_slice().fill(7);
        let frame = Arc::new(OwnedFrame::from_cpu_pixels(
            info(browser(3), 4, 1, 1),
            pixels.freeze(),
        ));
        let old_image = test_image(1, 1);
        let mut slot = Some(ImageSlot {
            content: PresentedContent::CpuImage(old_image.clone()),
            sequence: 3,
            generation: 2,
            width: 1,
            height: 1,
            captured_scale: None,
            backdrop_rgb: 0xffffff,
        });

        let first = image_slot_from_frame(&frame).expect("first conversion fits");
        assert!(matches!(
            image_slot_from_frame(&frame),
            Err(FrameError::BudgetExceeded)
        ));
        assert_eq!(budget.stats().used, 8, "the failed copy was refunded");
        // The failed candidate never reached the slot: old pixels and metadata
        // are still the current presentation.
        let kept = slot.as_ref().expect("old slot remains");
        assert_eq!((kept.sequence, kept.generation), (3, 2));
        assert!(matches!(
            &kept.content,
            PresentedContent::CpuImage(img) if Arc::ptr_eq(img, &old_image)
        ));
        // A later successful transaction can still replace it.
        let replacement = commit_image_change(&mut slot, Some(first), |_| Ok(()))
            .expect("old atlas entry is removed");
        assert!(replacement);
        assert_eq!(slot.as_ref().map(|slot| slot.sequence), Some(9));
    }

    #[test]
    fn gpu_shared_texture_slot_marks_presenting_without_atlas_or_budget() {
        use rust_cef_core::ports::{
            GpuSharedTextureFrame, SharedTextureSlotTracker, TextureFormat,
        };

        let tracker = Arc::new(SharedTextureSlotTracker::new());
        let keepalive = Arc::new(());
        let s0 = tracker.acquire_write_slot().unwrap();
        let f0 = Arc::new(OwnedFrame {
            info: info(browser(5), 2, 128, 72),
            payload: FramePayload::GpuSharedTexture(GpuSharedTextureFrame {
                pool_id: 1,
                shared_handle: 0x1000,
                is_nt_handle: true,
                keyed_mutex_key: Some(0),
                width: 128,
                height: 72,
                format: TextureFormat::Bgra8888,
                frame_seq: 1,
                slot_index: s0,
                generation: 1,
                resource_keepalive: Arc::downgrade(&keepalive),
                handle_owner: Arc::new(()),
                lease: tracker.lease(s0),
            }),
        });
        let slot0 = image_slot_from_frame(&f0).expect("valid gpu frame");
        let mut active: Option<ImageSlot> = None;
        let removed = commit_image_change(&mut active, Some(slot0), |_| {
            panic!("GpuTexture replacement must not invoke drop_atlas_image")
        })
        .unwrap();
        assert!(!removed);
        assert_eq!(tracker.presenting_slot(), Some(s0));
        assert_eq!(tracker.consumer_ref_count(s0), 1);

        let cleared = commit_image_change(&mut active, None, |_| {
            panic!("GpuTexture clear must not invoke drop_atlas_image")
        })
        .unwrap();
        assert!(cleared);
        drop(f0);
        assert_eq!(tracker.presenting_slot(), None);
        assert_eq!(tracker.consumer_ref_count(s0), 0);
    }

    #[test]
    fn snapshot_retry_is_explicit_and_never_claims_a_new_frame() {
        let owner = browser(3);
        let mailbox = FrameMailbox::with_budget(owner, FrameBudget::new(64));
        mailbox
            .configure_view(
                SurfaceBounds {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                },
                2,
                2,
            )
            .unwrap();
        let _ = mailbox.take_update();

        let token = mailbox.capture_token(SurfaceKind::View, 2, 2).unwrap();
        let frame = mailbox.copy_frame(&token, &[9u8; 16]).unwrap();
        mailbox.publish(frame).unwrap();

        assert!(matches!(
            poll_source(&mailbox, false),
            SourcePoll::Update(_)
        ));
        // The published update was acknowledged; a normal poll is empty even
        // though `latest` still exists.
        assert!(matches!(poll_source(&mailbox, false), SourcePoll::Empty));

        // Only an explicit needs_resync poll reads snapshot(), and that does
        // not consume a future update or fabricate a frame.
        match poll_source(&mailbox, true) {
            SourcePoll::Resync(update) => {
                assert_eq!(update.browser, owner);
                assert!(!update.closed);
                assert!(update.view.visible);
                assert!(update.view.frame.is_some());
            }
            _ => panic!("expected an explicit snapshot retry"),
        }
        assert!(matches!(poll_source(&mailbox, false), SourcePoll::Empty));

        // Hidden and closed states are visible through the same snapshot path.
        mailbox.set_visible(false).unwrap();
        let _ = mailbox.take_update();
        match poll_source(&mailbox, true) {
            SourcePoll::Resync(update) => assert!(!update.view.visible),
            _ => panic!("expected a hidden snapshot"),
        }

        let _ = mailbox.close();
        let _ = mailbox.take_update();
        match poll_source(&mailbox, true) {
            SourcePoll::Resync(update) => assert!(update.closed),
            _ => panic!("expected a closed snapshot"),
        }
    }

    #[test]
    fn cursor_style_mapping_covers_all_21_standard_variants_and_custom_fallback() {
        let cases = [
            (BrowserCursor::Arrow, CursorStyle::Arrow),
            (BrowserCursor::IBeam, CursorStyle::IBeam),
            (BrowserCursor::Crosshair, CursorStyle::Crosshair),
            (BrowserCursor::ClosedHand, CursorStyle::ClosedHand),
            (BrowserCursor::OpenHand, CursorStyle::OpenHand),
            (BrowserCursor::PointingHand, CursorStyle::PointingHand),
            (BrowserCursor::ResizeLeft, CursorStyle::ResizeLeft),
            (BrowserCursor::ResizeRight, CursorStyle::ResizeRight),
            (BrowserCursor::ResizeLeftRight, CursorStyle::ResizeLeftRight),
            (BrowserCursor::ResizeUp, CursorStyle::ResizeUp),
            (BrowserCursor::ResizeDown, CursorStyle::ResizeDown),
            (BrowserCursor::ResizeUpDown, CursorStyle::ResizeUpDown),
            (
                BrowserCursor::ResizeUpLeftDownRight,
                CursorStyle::ResizeUpLeftDownRight,
            ),
            (
                BrowserCursor::ResizeUpRightDownLeft,
                CursorStyle::ResizeUpRightDownLeft,
            ),
            (BrowserCursor::ResizeColumn, CursorStyle::ResizeColumn),
            (BrowserCursor::ResizeRow, CursorStyle::ResizeRow),
            (
                BrowserCursor::IBeamVertical,
                CursorStyle::IBeamCursorForVerticalLayout,
            ),
            (
                BrowserCursor::OperationNotAllowed,
                CursorStyle::OperationNotAllowed,
            ),
            (BrowserCursor::DragLink, CursorStyle::DragLink),
            (BrowserCursor::DragCopy, CursorStyle::DragCopy),
            (BrowserCursor::ContextualMenu, CursorStyle::ContextualMenu),
            (BrowserCursor::CustomUnsupported, CursorStyle::Arrow),
        ];
        for (input, expected) in cases {
            assert_eq!(cursor_style_for(input), expected);
        }
    }

    #[test]
    fn view_ux_state_updates_bounds_and_dispatches_edge_signals() {
        let mut ux = ViewUxState::default();
        assert_eq!(
            ux.apply_event(BrowserEvent::CursorChanged(BrowserCursor::IBeam)),
            (None, None)
        );
        assert_eq!(ux.cursor, BrowserCursor::IBeam);

        let long_tip = "提示".repeat(2000);
        ux.apply_event(BrowserEvent::TooltipChanged(long_tip));
        assert!(ux.tooltip.len() <= MAX_TOOLTIP_BYTES);
        assert!(std::str::from_utf8(ux.tooltip.as_bytes()).is_ok());

        ux.apply_event(BrowserEvent::FullscreenModeChanged(true));
        ux.apply_event(BrowserEvent::LoadingProgressChanged(1250));
        ux.apply_event(BrowserEvent::ZoomLevelChanged(1500));
        ux.apply_event(BrowserEvent::AudioMutedChanged(true));
        assert!(ux.fullscreen);
        assert_eq!(ux.loading_progress_per_mille, 1000);
        assert_eq!(ux.zoom_level_milli, 1500);
        assert!(ux.audio_muted);

        assert_eq!(
            ux.apply_event(BrowserEvent::FocusTaken { next: false }),
            (Some(false), None)
        );
        assert_eq!(
            ux.apply_event(BrowserEvent::HostShortcutTriggered { id: ShortcutId(9) }),
            (None, Some(ShortcutId(9)))
        );
    }

    #[test]
    fn js_events_queue_stays_independent_from_ux_queue_and_eval_forwards() {
        use rust_cef_core::ime::ImeSnapshot;
        use rust_cef_core::input::InputSnapshot;
        use rust_cef_core::ports::{BrowserControl, BrowserHandle};
        use std::sync::Mutex;

        struct StubControl {
            browser: BrowserId,
            ux: Mutex<Vec<BrowserEvent>>,
            js: Mutex<Vec<BrowserEvent>>,
            submitted: Mutex<Vec<BrowserCommand>>,
        }
        impl BrowserControl for StubControl {
            fn snapshot(&self) -> rust_cef_core::contracts::BrowserSnapshot {
                panic!("unused")
            }
            fn input_snapshot(&self) -> Result<InputSnapshot, EngineError> {
                Err(EngineError::NotReady)
            }
            fn ime_snapshot(&self) -> Result<ImeSnapshot, EngineError> {
                Err(EngineError::NotReady)
            }
            fn submit(&self, command: BrowserCommand) -> Result<CommandId, EngineError> {
                let seq = {
                    let mut guard = self.submitted.lock().unwrap();
                    guard.push(command);
                    guard.len() as u64
                };
                Ok(CommandId {
                    browser: self.browser,
                    sequence: seq,
                })
            }
            fn evaluate_script(
                &self,
                _script: String,
                _frame_scope: JsFrameScope,
                _timeout_ms: u64,
            ) -> Result<EvalRequestId, EngineError> {
                Ok(EvalRequestId {
                    browser: self.browser,
                    sequence: 7,
                })
            }
            fn request_close(&self, _: CloseMode) -> rust_cef_core::ports::CloseTicket {
                rust_cef_core::ports::CloseTicket::pending(self.browser, 1).0
            }
            fn take_ux_events(&self) -> Vec<BrowserEvent> {
                std::mem::take(&mut *self.ux.lock().unwrap())
            }
            fn take_js_events(&self) -> Vec<BrowserEvent> {
                std::mem::take(&mut *self.js.lock().unwrap())
            }
        }

        let bid = browser(4);
        let control = Arc::new(StubControl {
            browser: bid,
            ux: Mutex::new(vec![BrowserEvent::TooltipChanged("tip".into())]),
            js: Mutex::new(vec![
                BrowserEvent::JsMessageReceived {
                    origin: "app://demo".into(),
                    payload_json: "{\"ok\":true}".into(),
                },
                BrowserEvent::EvalCompleted {
                    id: EvalRequestId {
                        browser: bid,
                        sequence: 7,
                    },
                    outcome: EvalOutcome::Value("42".into()),
                },
            ]),
            submitted: Mutex::new(Vec::new()),
        });
        let handle = BrowserHandle::new(control);
        let id = handle
            .evaluate_script("42".into(), JsFrameScope::MainFrame, 1000)
            .unwrap();
        assert_eq!(id.sequence, 7);
        assert_eq!(handle.take_ux_events().len(), 1);
        assert_eq!(handle.take_js_events().len(), 2);
        assert!(handle.take_js_events().is_empty());
    }

    #[test]
    fn compute_pinch_zoom_validates_and_clamps_bounds() {
        assert_eq!(compute_pinch_zoom(0.0, 0.0), None);
        assert_eq!(compute_pinch_zoom(0.0, f32::NAN), None);
        assert_eq!(compute_pinch_zoom(0.0, f32::INFINITY), None);
        assert_eq!(compute_pinch_zoom(f64::NAN, 0.2), None);

        let up = compute_pinch_zoom(0.0, 0.2).unwrap();
        assert!((up - 1.0).abs() < 1e-6);

        let down = compute_pinch_zoom(0.0, -0.2).unwrap();
        assert!(down < 0.0 && down > MIN_ZOOM_LEVEL);

        assert_eq!(
            compute_pinch_zoom(MAX_ZOOM_LEVEL, 5.0),
            Some(MAX_ZOOM_LEVEL)
        );
        assert_eq!(
            compute_pinch_zoom(MIN_ZOOM_LEVEL, -0.99),
            Some(MIN_ZOOM_LEVEL)
        );
    }

    #[test]
    fn align_view_frame_target_fills_bounds_across_dpi_and_subpixel_rounding_without_stretching_resize()
     {
        let bounds = Bounds {
            origin: Point {
                x: px(10.0),
                y: px(20.0),
            },
            size: size(px(1200.0), px(800.0)),
        };

        // Exact same-DPI match fills container bounds.
        assert_eq!(
            align_view_frame_target(bounds, 1200, 800, 1.0, Some(1.0)).unwrap(),
            bounds
        );

        // Fractional DPI (1.25 -> 1500x1000, or ceil rounding by 1px) snaps to bounds.
        assert_eq!(
            align_view_frame_target(bounds, 1501, 1001, 1.25, Some(1.25)).unwrap(),
            bounds
        );

        // Cross-monitor DPI transition: 1.0x frame (1200x800 px) displayed on 2.0x window
        // fills bounds instead of shrinking to 600x400 in the top-left corner.
        assert_eq!(
            align_view_frame_target(bounds, 1200, 800, 2.0, Some(1.0)).unwrap(),
            bounds
        );

        // Same-DPI mid-drag interactive resize stays unstretched 1:1 to avoid distortion.
        let resized = align_view_frame_target(bounds, 800, 600, 1.0, Some(1.0)).unwrap();
        assert_eq!(resized.origin, bounds.origin);
        assert_eq!(resized.size, size(px(800.0), px(600.0)));
    }

    #[test]
    fn reconcile_zoom_prevents_stale_snapshot_snapback_and_clears_on_confirm_or_timeout() {
        let t0 = Instant::now();
        let mut ux = ViewUxState {
            zoom_level_milli: 523,
            ..ViewUxState::default()
        };
        let mut pending = Some(PendingZoom {
            target_milli: 523,
            submitted_at: t0,
        });

        // Stale snapshot 0 while target 523 is in flight must not revert ux.zoom_level_milli.
        reconcile_zoom_from_snapshot(0, t0 + Duration::from_millis(16), &mut ux, &mut pending);
        assert_eq!(ux.zoom_level_milli, 523);
        assert!(pending.is_some());

        // Matching snapshot confirms and clears pending immediately.
        reconcile_zoom_from_snapshot(523, t0 + Duration::from_millis(20), &mut ux, &mut pending);
        assert!(pending.is_none());
        assert_eq!(ux.zoom_level_milli, 523);

        // Timeout fallback after 500ms if CEF never reached target.
        let mut pending_timeout = Some(PendingZoom {
            target_milli: 1000,
            submitted_at: t0,
        });
        ux.zoom_level_milli = 1000;
        reconcile_zoom_from_snapshot(
            523,
            t0 + Duration::from_millis(520),
            &mut ux,
            &mut pending_timeout,
        );
        assert_eq!(ux.zoom_level_milli, 523);
        assert!(pending_timeout.is_none());
    }

    #[test]
    fn sample_frame_backdrop_rgb_picks_edge_background_and_unpremultiplies() {
        let budget = FrameBudget::new(4096);
        let mut buf = budget.allocate(2 * 2 * 4).unwrap();
        // Fill 2x2 with premultiplied BGRA dark slate (#1e293b -> B=0x3b, G=0x29, R=0x1e, A=0xff).
        for px in buf.as_mut_slice().chunks_exact_mut(4) {
            px.copy_from_slice(&[0x3b, 0x29, 0x1e, 0xff]);
        }
        let dark_frame = OwnedFrame::from_cpu_pixels(info(browser(1), 1, 2, 2), buf.freeze());
        assert_eq!(sample_frame_backdrop_rgb(&dark_frame), 0x1e293b);

        let mut half_alpha = budget.allocate(4).unwrap();
        half_alpha
            .as_mut_slice()
            .copy_from_slice(&[0x40, 0x80, 0x80, 0x80]);
        let prem_frame =
            OwnedFrame::from_cpu_pixels(info(browser(1), 2, 1, 1), half_alpha.freeze());
        let sampled = sample_frame_backdrop_rgb(&prem_frame);
        assert_eq!(sampled >> 16, 0xff);
    }

    #[test]
    fn zoom_gate_allows_first_then_coalesces_burst_until_frame_ack_and_flushes_final_once() {
        let mut gate = ZoomGate::default();
        let t0 = Instant::now();
        let seq = 10;

        let z1 = step_canonical_zoom_level(0.0, 1);
        assert_eq!(gate.plan_request(z1, seq, t0), Some(z1));
        gate.commit_sent(z1, seq, t0, None);

        let mut target = z1;
        for i in 2..=15 {
            target = step_canonical_zoom_level(target, 1);
            let t = t0 + Duration::from_millis(i * 4);
            assert_eq!(gate.plan_request(target, seq, t), None);
        }

        assert_eq!(gate.stats().requests, 15);
        assert_eq!(gate.stats().dispatched, 1);
        assert_eq!(gate.stats().coalesced, 14);
        assert!(gate.has_deferred_tail());
        assert!((gate.desired_zoom_level() - target).abs() < 1e-9);

        // No new frame at +65ms: remain gated cleanly.
        assert_eq!(
            gate.poll_flush(seq, t0 + Duration::from_millis(65)),
            ZoomFlushAction::Idle
        );

        // New committed frame (seq 11) unlocks and flushes ONLY final desired level.
        let t_ack = t0 + Duration::from_millis(70);
        assert_eq!(
            gate.poll_flush(seq + 1, t_ack),
            ZoomFlushAction::Dispatch {
                level: target,
                via_timeout: false,
            }
        );
        gate.commit_sent(target, seq + 1, t_ack, Some(false));
        assert_eq!(gate.stats().dispatched, 2);
        assert_eq!(gate.stats().frame_ack_flushes, 1);
        assert!(!gate.has_deferred_tail());

        // Second frame ack completes the cycle cleanly with zero extra sends.
        assert_eq!(
            gate.poll_flush(seq + 2, t0 + Duration::from_millis(140)),
            ZoomFlushAction::UnlockedNoTail
        );
        assert!(!gate.is_busy());
        assert_eq!(gate.stats().dispatched, 2);
    }

    #[test]
    fn zoom_gate_fallback_timeout_at_120ms_unlocks_and_flushes_latest_without_frame_ack() {
        let mut gate = ZoomGate::default();
        let t0 = Instant::now();
        let seq = 5;

        let z1 = step_canonical_zoom_level(0.0, 1);
        assert_eq!(gate.plan_request(z1, seq, t0), Some(z1));
        gate.commit_sent(z1, seq, t0, None);

        let mut target = z1;
        for step in 1..=5 {
            target = step_canonical_zoom_level(target, 1);
            assert_eq!(
                gate.plan_request(target, seq, t0 + Duration::from_millis(step * 15)),
                None
            );
        }

        // At exactly 120ms, still within in-flight window.
        assert_eq!(
            gate.poll_flush(seq, t0 + Duration::from_millis(120)),
            ZoomFlushAction::Idle
        );

        // Past 120ms with no new frame, fallback timeout unlocks and flushes latest.
        let t_timeout = t0 + Duration::from_millis(121);
        assert_eq!(
            gate.poll_flush(seq, t_timeout),
            ZoomFlushAction::Dispatch {
                level: target,
                via_timeout: true,
            }
        );
        gate.commit_sent(target, seq, t_timeout, Some(true));
        assert_eq!(gate.stats().timeout_flushes, 1);
        assert_eq!(gate.stats().dispatched, 2);
    }

    #[test]
    fn zoom_gate_noops_when_burst_returns_to_last_sent_level() {
        let mut gate = ZoomGate::default();
        let t0 = Instant::now();
        let seq = 20;

        let z1 = step_canonical_zoom_level(0.0, 1);
        assert_eq!(gate.plan_request(z1, seq, t0), Some(z1));
        gate.commit_sent(z1, seq, t0, None);

        let z2 = step_canonical_zoom_level(z1, 1);
        let z3 = step_canonical_zoom_level(z2, 1);
        assert_eq!(
            gate.plan_request(z2, seq, t0 + Duration::from_millis(5)),
            None
        );
        assert_eq!(
            gate.plan_request(z3, seq, t0 + Duration::from_millis(10)),
            None
        );
        // User scrolls back to z1 before frame ack arrives.
        assert_eq!(
            gate.plan_request(z1, seq, t0 + Duration::from_millis(15)),
            None
        );
        assert!(!gate.has_deferred_tail());

        // Frame ack unlocks cleanly with no redundant dispatch.
        assert_eq!(
            gate.poll_flush(seq + 1, t0 + Duration::from_millis(50)),
            ZoomFlushAction::UnlockedNoTail
        );
        assert!(!gate.is_busy());
        assert_eq!(gate.stats().dispatched, 1);
    }

    #[test]
    fn zoom_gate_sync_external_blocked_while_busy_and_applies_when_idle() {
        let mut gate = ZoomGate::default();
        let t0 = Instant::now();

        let z1 = step_canonical_zoom_level(0.0, 1);
        assert_eq!(gate.plan_request(z1, 1, t0), Some(z1));
        gate.commit_sent(z1, 1, t0, None);

        // Busy while in flight: external sync must be ignored.
        gate.sync_external(1.5);
        assert!((gate.desired_zoom_level() - z1).abs() < 1e-9);

        // Ack clears in-flight; now external sync updates both desired & last_sent.
        assert_eq!(
            gate.poll_flush(2, t0 + Duration::from_millis(30)),
            ZoomFlushAction::UnlockedNoTail
        );
        assert!(!gate.is_busy());
        gate.sync_external(1.5);
        assert!((gate.desired_zoom_level() - 1.5).abs() < 1e-9);
        assert!(!gate.is_busy());
    }

    #[test]
    fn right_mouse_button_maps_to_input_right_for_down_and_up() {
        assert_eq!(input_button(MouseButton::Right), Some(InputButton::Right));
        let mods = mouse_modifiers(gpui::Modifiers::default());
        assert!(!mods.shift && !mods.control && !mods.alt && !mods.platform);
    }

    #[test]
    fn translate_point_clamps_extreme_corners_inside_view_rect_and_normalizes_right_click_count() {
        let bounds = (0.0, 44.0, 1200.0, 762.0);
        // Fractional 150%-DPI positions near the bottom-right corner round to (1200, 762)
        // in raw float arithmetic, so inside-bounds translation must clamp inside [0..1199, 0..761].
        assert_eq!(translate_point(bounds, (0.2, 44.2), false), Some((0, 0)));
        assert_eq!(
            translate_point(bounds, (1199.6667, 805.6667), false),
            Some((1199, 761))
        );
        assert_eq!(normalize_click_count(InputButton::Right, 1), 1);
        assert_eq!(normalize_click_count(InputButton::Right, 2), 1);
        assert_eq!(normalize_click_count(InputButton::Right, 3), 1);
        assert_eq!(normalize_click_count(InputButton::Left, 2), 2);
        assert_eq!(normalize_click_count(InputButton::Left, 4), 1);
    }

    #[test]
    fn surface_transform_inverse_mouse_mapping_roundtrips_and_rejects_watermark_corners() {
        let bounds = (100.0, 50.0, 800.0, 600.0);
        let cx = 500.0_f32;
        let cy = 350.0_f32;

        // 0 deg is identity.
        assert_eq!(
            translate_point_transformed(bounds, (260.0, 190.0), 0.0, true, false),
            Some((160, 140))
        );

        // Any angle preserves the center.
        for angle in [-180.0_f32, -90.0, -32.5, 25.0, 90.0, 180.0] {
            let center_hit = translate_point_transformed(bounds, (cx, cy), angle, true, false);
            assert_eq!(center_hit, Some((400, 300)), "angle={angle}");

            // Forward transform local point (600, 150) -> (500, 100) in view coords, then invert.
            let (rad, fit_scale) = compute_surface_transform_params(bounds, angle, true);
            let local_x = 100.0_f32; // offset from center
            let local_y = -120.0_f32;
            let (sin_r, cos_r) = rad.sin_cos();
            let screen_x = cx + fit_scale * (local_x * cos_r - local_y * sin_r);
            let screen_y = cy + fit_scale * (local_x * sin_r + local_y * cos_r);
            let mapped =
                translate_point_transformed(bounds, (screen_x, screen_y), angle, true, false)
                    .expect("rotated quad interior must hit");
            assert!((mapped.0 - 500).abs() <= 1, "angle={angle} x={}", mapped.0);
            assert!((mapped.1 - 180).abs() <= 1, "angle={angle} y={}", mapped.1);
        }

        // Exposed watermark corner at 35 deg falls outside rotated quad when outside=false.
        assert_eq!(
            translate_point_transformed(bounds, (105.0, 55.0), 35.0, true, false),
            None
        );
    }

    #[test]
    fn browser_handle_routes_popup_policy_filters_and_ui_listener() {
        use rust_cef_core::host_capabilities::{
            DragEnterDecision, FocusDecision, KeyEventDecision, NavigationDecision,
            NavigationFilter, PopupDecision, PopupDisposition, PopupFilter, PopupPolicy,
            PreKeyDecision, ProtocolExecutionDecision,
        };
        use rust_cef_core::ime::ImeSnapshot;
        use rust_cef_core::input::InputSnapshot;
        use rust_cef_core::ports::{BrowserControl, BrowserHandle, EXT_EVENT_FRAME_LIFECYCLE};
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU32, Ordering},
        };

        struct RoutingStub {
            policy: Mutex<PopupPolicy>,
            ui_active: AtomicBool,
            popup_filter_set: AtomicBool,
            nav_filter_set: AtomicBool,
            extended_mask: AtomicU32,
            drag_filter_set: AtomicBool,
            focus_filter_set: AtomicBool,
            pre_key_filter_set: AtomicBool,
            key_filter_set: AtomicBool,
            protocol_filter_set: AtomicBool,
        }

        impl BrowserControl for RoutingStub {
            fn snapshot(&self) -> rust_cef_core::contracts::BrowserSnapshot {
                panic!("unused")
            }
            fn input_snapshot(&self) -> Result<InputSnapshot, EngineError> {
                Err(EngineError::NotReady)
            }
            fn ime_snapshot(&self) -> Result<ImeSnapshot, EngineError> {
                Err(EngineError::NotReady)
            }
            fn submit(&self, _: BrowserCommand) -> Result<CommandId, EngineError> {
                Err(EngineError::NotReady)
            }
            fn evaluate_script(
                &self,
                _: String,
                _: JsFrameScope,
                _: u64,
            ) -> Result<EvalRequestId, EngineError> {
                Err(EngineError::NotReady)
            }
            fn request_close(&self, _: CloseMode) -> rust_cef_core::ports::CloseTicket {
                rust_cef_core::ports::CloseTicket::pending(browser(9), 1).0
            }
            fn take_ux_events(&self) -> Vec<BrowserEvent> {
                Vec::new()
            }
            fn take_js_events(&self) -> Vec<BrowserEvent> {
                Vec::new()
            }
            fn set_popup_policy(&self, policy: PopupPolicy) {
                *self.policy.lock().unwrap() = policy;
            }
            fn popup_policy(&self) -> PopupPolicy {
                *self.policy.lock().unwrap()
            }
            fn set_popup_filter(&self, filter: Option<PopupFilter>) {
                self.popup_filter_set
                    .store(filter.is_some(), Ordering::Release);
            }
            fn set_popup_ui_listener_active(&self, active: bool) {
                self.ui_active.store(active, Ordering::Release);
            }
            fn set_navigation_filter(&self, filter: Option<NavigationFilter>) {
                self.nav_filter_set
                    .store(filter.is_some(), Ordering::Release);
            }
            fn set_extended_event_mask(&self, mask: u32) {
                self.extended_mask.store(mask, Ordering::Release);
            }
            fn set_drag_enter_filter(
                &self,
                filter: Option<rust_cef_core::host_capabilities::DragEnterFilter>,
            ) {
                self.drag_filter_set
                    .store(filter.is_some(), Ordering::Release);
            }
            fn set_focus_filter(
                &self,
                filter: Option<rust_cef_core::host_capabilities::SetFocusFilter>,
            ) {
                self.focus_filter_set
                    .store(filter.is_some(), Ordering::Release);
            }
            fn set_pre_key_filter(
                &self,
                filter: Option<rust_cef_core::host_capabilities::PreKeyFilter>,
            ) {
                self.pre_key_filter_set
                    .store(filter.is_some(), Ordering::Release);
            }
            fn set_key_event_filter(
                &self,
                filter: Option<rust_cef_core::host_capabilities::KeyEventFilter>,
            ) {
                self.key_filter_set
                    .store(filter.is_some(), Ordering::Release);
            }
            fn set_protocol_execution_filter(
                &self,
                filter: Option<rust_cef_core::host_capabilities::ProtocolExecutionFilter>,
            ) {
                self.protocol_filter_set
                    .store(filter.is_some(), Ordering::Release);
            }
        }

        let stub = Arc::new(RoutingStub {
            policy: Mutex::new(PopupPolicy::Auto),
            ui_active: AtomicBool::new(false),
            popup_filter_set: AtomicBool::new(false),
            nav_filter_set: AtomicBool::new(false),
            extended_mask: AtomicU32::new(0),
            drag_filter_set: AtomicBool::new(false),
            focus_filter_set: AtomicBool::new(false),
            pre_key_filter_set: AtomicBool::new(false),
            key_filter_set: AtomicBool::new(false),
            protocol_filter_set: AtomicBool::new(false),
        });
        let handle = BrowserHandle::new(stub.clone());
        assert_eq!(handle.popup_policy(), PopupPolicy::Auto);
        handle.set_popup_policy(PopupPolicy::NavigateCurrentTab);
        assert_eq!(handle.popup_policy(), PopupPolicy::NavigateCurrentTab);

        handle.set_popup_ui_listener_active(true);
        assert!(stub.ui_active.load(Ordering::Acquire));
        handle.set_extended_event_mask(EXT_EVENT_FRAME_LIFECYCLE);
        assert_eq!(
            stub.extended_mask.load(Ordering::Acquire),
            EXT_EVENT_FRAME_LIFECYCLE
        );

        let pfilter: PopupFilter = Arc::new(|req| {
            assert_eq!(req.disposition, PopupDisposition::NewForegroundTab);
            PopupDecision::NavigateCurrentTab
        });
        handle.set_popup_filter(Some(pfilter));
        assert!(stub.popup_filter_set.load(Ordering::Acquire));

        let nfilter: NavigationFilter = Arc::new(|req| {
            if req.url.contains("block") {
                NavigationDecision::Cancel
            } else {
                NavigationDecision::Allow
            }
        });
        handle.set_navigation_filter(Some(nfilter));
        assert!(stub.nav_filter_set.load(Ordering::Acquire));

        handle.set_drag_enter_filter(Some(Arc::new(|_| DragEnterDecision::Allow)));
        handle.set_focus_filter(Some(Arc::new(|_, _| FocusDecision::Allow)));
        handle.set_pre_key_filter(Some(Arc::new(|_, _| PreKeyDecision::PassThrough)));
        handle.set_key_event_filter(Some(Arc::new(|_, _| KeyEventDecision::PassThrough)));
        handle.set_protocol_execution_filter(Some(Arc::new(|_, _| {
            ProtocolExecutionDecision::AllowOsExecution
        })));
        assert!(stub.drag_filter_set.load(Ordering::Acquire));
        assert!(stub.focus_filter_set.load(Ordering::Acquire));
        assert!(stub.pre_key_filter_set.load(Ordering::Acquire));
        assert!(stub.key_filter_set.load(Ordering::Acquire));
        assert!(stub.protocol_filter_set.load(Ordering::Acquire));

        clear_routing_and_filters(&handle);
        assert!(!stub.ui_active.load(Ordering::Acquire));
        assert_eq!(stub.extended_mask.load(Ordering::Acquire), 0);
        assert!(!stub.popup_filter_set.load(Ordering::Acquire));
        assert!(!stub.nav_filter_set.load(Ordering::Acquire));
        assert!(!stub.drag_filter_set.load(Ordering::Acquire));
        assert!(!stub.focus_filter_set.load(Ordering::Acquire));
        assert!(!stub.pre_key_filter_set.load(Ordering::Acquire));
        assert!(!stub.key_filter_set.load(Ordering::Acquire));
        assert!(!stub.protocol_filter_set.load(Ordering::Acquire));
    }

    #[test]
    fn view_ux_state_apply_snapshot_updates_extended_cefsharp_fields() {
        let mut ux = ViewUxState::default();
        let mut nav = NavigationSnapshot::default();
        nav.address = "https://example.test/page".into();
        nav.title = "Example Page".into();
        nav.is_loading = true;
        nav.can_go_back = true;
        nav.can_go_forward = false;
        nav.main_http_status = Some(200);
        nav.last_main_frame_error = Some(-105);
        nav.cursor = BrowserCursor::PointingHand;
        nav.tooltip = "Open link".into();
        nav.status_message = "Connecting".into();
        nav.favicon_urls = vec!["https://example.test/favicon.ico".into()];
        nav.fullscreen = true;
        nav.loading_progress_per_mille = 1450;
        nav.zoom_level_milli = 1250;
        nav.audio_muted = true;

        ux.apply_snapshot(&nav);
        assert_eq!(ux.address, "https://example.test/page");
        assert_eq!(ux.title, "Example Page");
        assert!(ux.is_loading);
        assert!(ux.can_go_back);
        assert!(!ux.can_go_forward);
        assert_eq!(ux.main_http_status, Some(200));
        assert_eq!(ux.last_main_frame_error, Some(-105));
        assert_eq!(ux.cursor, BrowserCursor::PointingHand);
        assert_eq!(ux.tooltip, "Open link");
        assert_eq!(ux.status_text, "Connecting");
        assert_eq!(ux.favicon_urls, vec!["https://example.test/favicon.ico"]);
        assert!(ux.fullscreen);
        assert_eq!(ux.loading_progress_per_mille, 1000);
        assert_eq!(ux.zoom_level_milli, 1250);
        assert!(ux.audio_muted);
    }
}
