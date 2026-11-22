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
};

use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Corners, CursorStyle, DispatchPhase,
    DragMoveEvent, ElementInputHandler, EntityInputHandler, ExternalPaths, FileDropEvent,
    FocusHandle, Hitbox, HitboxBehavior, InteractiveElement as _, IntoElement, KeyDownEvent,
    KeyUpEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _,
    PinchEvent, Pixels, Point, Render, RenderImage, Role, ScrollDelta, ScrollEvent,
    StatefulInteractiveElement as _, Styled as _, Subscription, UTF16Selection, Window, canvas,
    div, px, size,
};
use image::{Frame, ImageBuffer, Rgba};
use rust_cef_core::{
    accessibility::AxTreeSnapshot,
    contracts::{
        BrowserCursor, BrowserId, CefFeatureState, MAX_FAVICON_URL_BYTES, MAX_FAVICON_URLS,
        MAX_STATUS_MESSAGE_BYTES, MAX_TOOLTIP_BYTES, MAX_ZOOM_LEVEL, MIN_ZOOM_LEVEL,
        NavigationSnapshot, RendererState, SessionState, ShortcutId, Viewport, truncate_utf8_bytes,
        zoom_level_to_milli,
    },
    file_drop::{FileDragPhase, FileDropPaths},
    frame_budget::FrameError,
    input::{
        BrowserTouchEvent, InputKind, InputResetReason, MAX_INPUT_RANGE_UTF16,
        MAX_INPUT_TEXT_UTF16, MouseButton as InputButton, TouchEventType, Utf16Range,
    },
    js_bridge::{EvalOutcome, EvalRequestId, JsFrameScope},
    keymap,
    mailbox::{FrameUpdate, SurfaceSnapshot},
    outbound_drag::{OutboundDragFinish, OutboundDragStart},
    ports::{
        BrowserCommand, BrowserEvent, BrowserHandle, CloseMode, CommandId, EngineError, FrameInfo,
        FrameSource, ImeGeometry, MouseModifiers, OwnedFrame, PixelFormat, SurfaceKind,
        WindowBinding,
    },
};
use smallvec::smallvec;

use crate::{
    BrowserAttachment,
    binding::{BindingError, PaintStamp, WindowToken},
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

/// The displayed image plus the exact identity it carries.
///
/// Metadata is committed or retained together with the image owner: a failed
/// atlas drop restores the complete old slot, never a half-cleared one.
struct ImageSlot {
    image: Arc<RenderImage>,
    sequence: u64,
    generation: u64,
    width: u32,
    height: u32,
}

/// The exact image a render pass will paint, plus the identity and stamp it
/// needs to pass the prepaint/paint gate.
struct PaintFrame {
    image: Arc<RenderImage>,
    sequence: u64,
    generation: u64,
    stamp: PaintStamp,
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
    pub(crate) cursor: BrowserCursor,
    pub(crate) tooltip: String,
    pub(crate) status_text: String,
    pub(crate) favicon_urls: Vec<String>,
    pub(crate) fullscreen: bool,
    pub(crate) loading_progress_per_mille: u16,
    pub(crate) zoom_level_milli: i32,
    pub(crate) audio_muted: bool,
}

impl ViewUxState {
    pub(crate) fn apply_snapshot(&mut self, nav: &NavigationSnapshot) {
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
    ax_snapshot: Option<AxTreeSnapshot>,
    pinch_zoom_enabled: bool,
    on_take_focus: Option<TakeFocusCallback>,
    on_shortcut: Option<ShortcutCallback>,
    on_js_message: Option<JsMessageCallback>,
    on_eval_completed: Option<EvalCompletedCallback>,
}

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
        let window_id = window_handle.window_id().as_u64();
        let scale_factor = checked_scale(window.scale_factor())?;
        let sent_viewport = Rc::new(Cell::new(None));
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
            subscriptions: [None, None, None, None, None, None],
            menu,
            ux: ViewUxState::default(),
            outbound_drag: crate::outbound_drag::OutboundDragState::new(),
            outbound_drag_pump: None,
            outbound_drag_task: None,
            on_outbound_drag: None,
            ax_snapshot: initial_ax_snapshot,
            pinch_zoom_enabled: false,
            on_take_focus: None,
            on_shortcut: None,
            on_js_message: None,
            on_eval_completed: None,
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
            if close_view
                .update(app, |view, _| {
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
            close_handle.request_close(CloseMode::Graceful);
        });
        view.update(cx, move |view, cx| {
            let activation = cx.observe_window_activation(window, |view, window, cx| {
                if !window.is_window_active() {
                    if let Some(b) = crate::menu::project(&view.menu) {
                        b.readiness(false);
                    }
                    view.reset_input(InputResetReason::FocusLost);
                    view.was_focused = false;
                    cx.notify();
                } else {
                    view.ime_request = None;
                    view.refresh_ime(window, cx);
                    view.project_menu_focus(window, cx);
                }
            });
            let bounds = cx.observe_window_bounds(window, |view, window, cx| {
                if let Some(b) = crate::menu::project(&view.menu) {
                    b.geometry_changed();
                }
                view.ime_request = None;
                view.refresh_ime(window, cx);
                cx.notify();
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
                && window.is_window_active()
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

    pub(crate) fn native_touch_event(
        &mut self,
        mut event: BrowserTouchEvent,
        window: &Window,
    ) -> Result<CommandId, ViewError> {
        self.check_window(window)?;
        self.check_live()?;
        if event.event_type == TouchEventType::Pressed && !window.is_window_active() {
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

    pub fn audio_muted(&self) -> bool {
        self.ux.audio_muted
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
        self.attachment
            .handle()
            .submit(BrowserCommand::SetZoomLevel(level))
            .map_err(ViewError::Input)
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

    pub fn send_touch_event(&mut self, event: BrowserTouchEvent) -> Result<CommandId, ViewError> {
        self.check_live()?;
        let event = event.validate_and_normalize().map_err(ViewError::Input)?;
        self.attachment
            .handle()
            .submit(BrowserCommand::TouchEvent(event))
            .map_err(ViewError::Input)
    }

    fn cancel_outbound_drags(&mut self) {
        self.outbound_drag_task.take();
        for finish in self.outbound_drag.cancel_all() {
            let _ = self
                .attachment
                .handle()
                .submit(BrowserCommand::FinishOutboundDrag(finish));
        }
    }

    fn pointer_pinch(&mut self, event: &PinchEvent, window: &mut Window) {
        if !self.pinch_zoom_enabled
            || self.check_window(window).is_err()
            || self.check_live().is_err()
        {
            return;
        }
        if let Some(next) = compute_pinch_zoom(self.zoom_level(), event.delta)
            && let Ok(milli) = zoom_level_to_milli(next)
            && self
                .attachment
                .handle()
                .submit(BrowserCommand::SetZoomLevel(next))
                .is_ok()
        {
            self.ux.zoom_level_milli = milli;
        }
    }

    fn sync_ux(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let nav = self.attachment.handle().snapshot().navigation;
        self.ux.apply_snapshot(&nav);
        let events = self.attachment.handle().take_ux_events();
        let mut focus_taken = Vec::new();
        let mut shortcuts = Vec::new();
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
                }
                BrowserEvent::AccessibilityTreeUpdated { .. } => {
                    a11y_updated = true;
                }
                other => {
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
                    let _ = self
                        .attachment
                        .handle()
                        .submit(BrowserCommand::FinishOutboundDrag(finish));
                }
            } else {
                let hwnd_raw = raw_window_handle::HasWindowHandle::window_handle(window)
                    .ok()
                    .and_then(|h| match h.as_raw() {
                        raw_window_handle::RawWindowHandle::Win32(w) => Some(w.hwnd.get() as usize),
                        _ => None,
                    });
                let handle = self.attachment.handle().clone();
                let pump = self.outbound_drag_pump.clone();
                self.outbound_drag_task = Some(cx.spawn_in(window, async move |weak, cx| {
                    let hwnd = hwnd_raw.map(|ptr| ptr as *mut std::ffi::c_void);
                    let finish = crate::ole_drag::run_ole_drag(&start, hwnd, pump);
                    let _ = handle.submit(BrowserCommand::FinishOutboundDrag(finish));
                    let _ = weak.update(cx, |view, _| {
                        let _ = view.outbound_drag.finish_in_flight(
                            finish.drag_id,
                            finish.x,
                            finish.y,
                            finish.operation,
                        );
                    });
                }));
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

    /// One host-selected retry; never pumps CEF or changes frame retries.
    pub fn retry_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, ViewError> {
        self.check_window(window)?;
        self.check_live()?;
        self.sync_focus(window, cx);
        if !self.focus.is_focused(window) || !window.is_window_active() {
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
        if !window.is_window_active() {
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
        let composition = self.composition.as_ref()?;
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
        let has_composition = self.composition.is_some() || self.input.borrow().pending_mark();
        if has_composition
            && self
                .attachment
                .handle()
                .input_snapshot()
                .is_ok_and(|s| !s.accepts_text)
        {
            // A non-frame keyboard-mode wake may revoke text while the GPUI
            // view still owns focus. Use the existing cleanup lane, not a
            // direct native call or focus restoration during rendering.
            self.reset_input(InputResetReason::FocusLost);
        }
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
        if !window.is_window_active() {
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
        if !snapshot.accepts_text {
            return Err(InputCallbackGate::TextNotAccepted);
        }
        Ok(())
    }

    fn sync_focus(&mut self, window: &Window, cx: &App) {
        self.project_menu_focus(window, cx);
        let focused = window.is_window_active() && self.focus.is_focused(window);
        if self.was_focused && !focused {
            self.reset_input(InputResetReason::FocusLost);
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
                if let Some(b) = crate::menu::project(&self.menu) {
                    b.geometry_changed();
                }
                self.reset_input(InputResetReason::FocusLost);
            }
            if changed {
                self.popup_visible = false;
                self.popup_invalid = true;
                // Revoke closures before attempting a fallible atlas removal.
                self.advance_binding()?;
                let clear = self.clear_popup(window);
                cx.notify();
                self.needs_resync = true;
                clear?;
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

            let candidate = match image_slot_from_frame(frame) {
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

            match commit_image_change(&mut self.image, Some(candidate), |old| {
                drop_atlas_image(window, old)
            }) {
                Ok(removed) => {
                    if removed {
                        self.released_images = self.released_images.saturating_add(1);
                    }
                    self.created_images = self.created_images.saturating_add(1);
                    self.needs_resync = false;
                    self.advance_binding()?;
                    window.refresh();
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
        self.cancel_outbound_drags();
        self.ax_snapshot = None;
        self.attachment.ax_tree().clear();
        self.reset_input(InputResetReason::Closing);
        self.token.retire();
        self.needs_resync = false;
        let view = self.image.take();
        let popup = self.popup_image.take();
        if view.is_none() && popup.is_none() {
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
                    if let Err(message) = drop_atlas_image(window, slot.image) {
                        error.get_or_insert(message);
                    }
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
                if !expected_epoch.is_some_and(|epoch| {
                    self.attachment
                        .handle()
                        .input_snapshot()
                        .is_ok_and(|s| s.epoch != epoch)
                }) {
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
        translate_point(
            self.paint.borrow().surface_bounds,
            (f32::from(position.x), f32::from(position.y)),
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
        self.presses_received = self.presses_received.saturating_add(1);
        if self
            .check_window(window)
            .and_then(|_| self.check_live())
            .is_err()
            || !window.is_window_active()
            || self.attachment.handle().snapshot().state != SessionState::Alive
        {
            return;
        }
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
        if self.ensure_cef_focus(window).is_err() {
            return;
        }
        if button == InputButton::Left {
            let bounds = self.paint.borrow().surface_bounds;
            if let Err(error) = self.click.borrow_mut().begin(
                window_identity(window),
                bounds,
                (f32::from(event.position.x), f32::from(event.position.y)),
                mouse_modifiers(event.modifiers),
                count,
            ) {
                self.record_message(error.to_string());
            }
        } else {
            self.click.borrow_mut().cancel();
        }
        let _ = self.send_input(
            InputKind::MouseButton {
                x,
                y,
                button,
                up: false,
                click_count: count,
                modifiers: mouse_modifiers(event.modifiers),
            },
            false,
            window,
        );
        cx.notify();
    }

    fn pointer_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(button) = input_button(event.button) else {
            return;
        };
        if !self.input.borrow().owns_button(button) && !self.input.borrow().pending() {
            return;
        }
        if self
            .check_window(window)
            .and_then(|_| self.check_live())
            .is_err()
            || !window.is_window_active()
        {
            self.reset_input(InputResetReason::FocusLost);
            return;
        }
        let Some((x, y)) = self.point(event.position, true) else {
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
        let held = self.input.borrow().held_button();
        if held != owned_only || !window.is_window_active() {
            return;
        }
        if held && event.pressed_button.is_none() {
            // A mouse move no longer reporting a pressed button cannot extend
            // an owned drag; revoke it instead of leaving CEF pressed.
            self.reset_input(InputResetReason::FocusLost);
            return;
        }
        let Some((x, y)) = self.point(event.position, held) else {
            return;
        };
        let _ = self.send_input(
            InputKind::MouseMove {
                x,
                y,
                leaving: false,
                modifiers: mouse_modifiers(event.modifiers),
            },
            false,
            window,
        );
    }

    fn pointer_leave(&mut self, window: &Window) {
        if !window.is_window_active() || self.input.borrow().held_button() {
            return;
        }
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

    fn pointer_scroll(&mut self, event: &ScrollEvent, window: &Window) {
        if !window.is_window_active() {
            return;
        }
        let Some((x, y)) = self.point(event.position, false) else {
            return;
        };
        let (delta_x, delta_y) = match event.delta {
            ScrollDelta::Lines(delta) => keymap::wheel_units((delta.x, delta.y)),
            ScrollDelta::Pixels(delta) => {
                keymap::wheel_units_from_pixels((f32::from(delta.x), f32::from(delta.y)))
            }
        };
        let _ = self.send_input(
            InputKind::MouseWheel {
                x,
                y,
                delta_x,
                delta_y,
                modifiers: mouse_modifiers(event.modifiers),
            },
            false,
            window,
        );
    }

    pub(crate) fn native_key_down(
        &mut self,
        key: NativeKey,
        window: &Window,
    ) -> NativeDownAdmission {
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

    fn key_down(&mut self, event: &KeyDownEvent, window: &Window) {
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
        self.cancel_outbound_drags();
        self.ax_snapshot = None;
        self.attachment.ax_tree().clear();
        self.reset_input(InputResetReason::Closing);
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
        let Some(stamp) = self.token.stamp() else {
            self.note_rejected_paint(ViewError::RetiredBinding.to_string());
            return div().size_full().into_any_element();
        };
        if !self.token.permits(stamp, actual) {
            self.note_rejected_paint(ViewError::RetiredBinding.to_string());
            return div().size_full().into_any_element();
        }

        let paint_frame = self.image.as_ref().map(|slot| PaintFrame {
            image: slot.image.clone(),
            sequence: slot.sequence,
            generation: slot.generation,
            stamp,
        });
        let popup_frame = if self.popup_visible && !self.popup_invalid {
            self.popup_image.as_ref().map(|slot| PaintFrame {
                image: slot.image.clone(),
                sequence: slot.sequence,
                generation: slot.generation,
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
        let handle = self.attachment.handle().clone();
        let sent_viewport = self.sent_viewport.clone();

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
            .size_full()
            .role(Role::WebView)
            .cursor(cursor_style_for(self.ux.cursor))
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
                if !cx.has_active_drag() {
                    view.pointer_move(event, window, false);
                }
            }))
            .on_hover(cx.listener(|view, hovered: &bool, window, cx| {
                if !*hovered && !cx.has_active_drag() {
                    view.pointer_leave(window);
                }
            }))
            .on_scroll(cx.listener(|view, event: &ScrollEvent, window, _| {
                view.pointer_scroll(event, window);
            }))
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, _| {
                view.key_down(event, window);
            }))
            .on_key_up(cx.listener(|view, event: &KeyUpEvent, window, _| {
                view.key_up(event, window);
            }))
            .on_pinch(cx.listener(|view, event: &PinchEvent, window, _| {
                view.pointer_pinch(event, window);
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
                    move |bounds, window, app| {
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
                            return;
                        }
                        let scale = window.scale_factor();
                        let next = match viewport_for(bounds, scale) {
                            Ok(next) => next,
                            Err(error) => {
                                diag_prepaint.borrow_mut().last_error = Some(error.to_string());
                                return;
                            }
                        };
                        let Some(entity) = weak_context.upgrade() else {
                            return;
                        };
                        let stamp = match entity.update(app, |view, cx| {
                            view.reconcile_popup_context(next, bounds, window, cx)
                        }) {
                            Ok(stamp) => stamp,
                            Err(error) => {
                                diag_prepaint.borrow_mut().last_error = Some(error.to_string());
                                return;
                            }
                        };
                        {
                            let mut diag = diag_prepaint.borrow_mut();
                            diag.surface_bounds = bounds_dip(bounds);
                            diag.scale_factor = scale;
                        }
                        entity.update(app, |view, cx| view.refresh_ime(window, cx));
                        if handle.snapshot().state != SessionState::Alive {
                            return;
                        }
                        // Public GPUI hitbox belongs to this actual prepaint frame.
                        // Capture drag_move bounds alone do not exclude host overlays.
                        if token_prepaint.permits(stamp, actual) {
                            let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
                            *file_hitbox.borrow_mut() = Some((stamp, hitbox));
                        }
                        if sent_viewport.get() == Some(next) {
                            return;
                        }
                        // Re-check immediately before handing the command to the
                        // Runtime: a retirement/replacement between render and
                        // prepaint must not post a stale viewport.
                        if !token_prepaint.permits(stamp, actual) {
                            let mut diag = diag_prepaint.borrow_mut();
                            diag.rejected_paints = diag.rejected_paints.saturating_add(1);
                            diag.last_error =
                                Some("SetViewport skipped: paint stamp was invalidated".to_owned());
                            return;
                        }
                        // Owned command to the Runtime; no CEF object is touched here
                        // or in paint. Accepted values are remembered so a stable
                        // layout does not resubmit the same viewport every frame.
                        match handle.submit(BrowserCommand::SetViewport(next)) {
                            Ok(_) => sent_viewport.set(Some(next)),
                            Err(error) => {
                                diag_prepaint.borrow_mut().last_error =
                                    Some(format!("SetViewport rejected: {error:?}"));
                            }
                        }
                    },
                    move |bounds, _prepaint: (), window, app| {
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
                                    entity.update(app, |view, _| {
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
                                        if !window.is_window_active() {
                                            view.reset_input(InputResetReason::FocusLost);
                                        }
                                        let _ = event;
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
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, app| {
                            if phase == DispatchPhase::Capture && !app.has_active_drag() {
                                if let Some(entity) = moving.upgrade() {
                                    entity.update(app, |view, _| {
                                        view.pointer_move(event, window, true)
                                    });
                                }
                            }
                        });
                        for (frame, is_popup) in [(paint_frame, false), (popup_frame, true)] {
                            let Some(frame) = frame else {
                                continue;
                            };
                            // Re-check before reading the image or importing it into the
                            // atlas. An already-alive image Arc is not proof of liveness.
                            let actual = window_identity(window);
                            if actual != expected_window
                                || !token_paint.permits(frame.stamp, actual)
                            {
                                diag_paint.borrow_mut().reject_stamp(
                                    &token_paint,
                                    frame.stamp,
                                    actual,
                                    expected_window,
                                    "paint_image",
                                );
                                return;
                            }
                            let scale = window.scale_factor();
                            let image_size = frame.image.size(0);
                            let origin = if is_popup {
                                popup_origin(bounds.origin, popup_bounds)
                            } else {
                                bounds.origin
                            };
                            let target = match frame_target(
                                origin,
                                u32::from(image_size.width),
                                u32::from(image_size.height),
                                scale,
                            ) {
                                Ok(target) => target,
                                Err(error) => {
                                    diag_paint.borrow_mut().last_error = Some(error.to_string());
                                    return;
                                }
                            };
                            let visible = bounds.intersect(&target);
                            if f32::from(visible.size.width) <= 0.0
                                || f32::from(visible.size.height) <= 0.0
                            {
                                continue;
                            }
                            if !token_paint.permits(frame.stamp, window_identity(window)) {
                                diag_paint.borrow_mut().reject_stamp(
                                    &token_paint,
                                    frame.stamp,
                                    window_identity(window),
                                    expected_window,
                                    "paint_image",
                                );
                                continue;
                            }
                            // `bounds` clips; `target` keeps the frame at its captured
                            // device-pixel size divided by the window scale, anchored at
                            // the surface origin. No stretching of a stale image.
                            match window.paint_image(
                                bounds,
                                target,
                                Corners::default(),
                                frame.image.clone(),
                                0,
                                false,
                            ) {
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
                                                width: u32::from(image_size.width),
                                                height: u32::from(image_size.height),
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
                    },
                )
                .size_full(),
            )
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
    keymap::to_view_point(position, (x, y), (width, height))
}

fn input_button(button: MouseButton) -> Option<InputButton> {
    match button {
        MouseButton::Left => Some(InputButton::Left),
        MouseButton::Middle => Some(InputButton::Middle),
        MouseButton::Right => Some(InputButton::Right),
        _ => None,
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
    let Some(old) = slot.take() else {
        *slot = candidate;
        return Ok(false);
    };
    match drop_old(old.image.clone()) {
        Ok(()) => {
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
    if frame.pixels.len() != expected {
        return Err(FrameError::InvalidDimensions);
    }
    let (width, height) = (info.width, info.height);
    frame.pixels.copy_buffer()?.try_into_owner(|pixels| {
        let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(width, height, pixels)
            .ok_or(FrameError::InvalidDimensions)?;
        Ok::<_, FrameError>(Arc::new(RenderImage::new(smallvec![Frame::new(buffer)])))
    })
}

fn image_slot_from_frame(frame: &OwnedFrame) -> Result<ImageSlot, FrameError> {
    image_slot_with_image(frame, render_image_from_frame(frame)?)
}

fn image_slot_from_surface(frame: &OwnedFrame, kind: SurfaceKind) -> Result<ImageSlot, FrameError> {
    image_slot_with_image(frame, render_image_from_surface(frame, kind)?)
}

fn image_slot_with_image(
    frame: &OwnedFrame,
    image: Arc<RenderImage>,
) -> Result<ImageSlot, FrameError> {
    Ok(ImageSlot {
        image,
        sequence: frame.info.sequence,
        generation: frame.info.surface_generation,
        width: frame.info.width,
        height: frame.info.height,
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
        let frame = OwnedFrame {
            info: identity,
            pixels: pixels.freeze(),
        };
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
        let frame = OwnedFrame {
            info: info(browser(3), 4, 2, 2),
            pixels: pixels.freeze(),
        };
        let image = render_image_from_frame(&frame).unwrap();
        assert_eq!(budget.stats().used, 32);
        // A second consumer copy must be refused; it cannot escape this ledger.
        assert!(matches!(
            render_image_from_frame(&frame),
            Err(FrameError::BudgetExceeded)
        ));
        assert_eq!(frame.pixels.as_slice(), &[123; 16]);
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
            image: old_image.clone(),
            sequence: 9,
            generation: 4,
            width: 2,
            height: 2,
        });
        let candidate = ImageSlot {
            image: candidate_image.clone(),
            sequence: 10,
            generation: 4,
            width: 2,
            height: 2,
        };

        let error = commit_image_change(&mut slot, Some(candidate), |_| {
            Err("drop_image failed".to_owned())
        })
        .expect_err("a failed old atlas removal is an error");
        assert_eq!(error, "drop_image failed");

        let kept = slot.as_ref().expect("old image is restored");
        assert_eq!((kept.sequence, kept.generation), (9, 4));
        assert_eq!((kept.width, kept.height), (2, 2));
        assert!(Arc::ptr_eq(&kept.image, &old_image));
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
        assert!(Arc::ptr_eq(&kept.image, &old_image));
    }

    #[test]
    fn failed_conversion_keeps_the_previous_slot_and_owner() {
        // 8 bytes total: the published frame holds 4 and the first presentation
        // copy takes the remaining 4, so a second copy is a real budget failure.
        let budget = FrameBudget::new(8);
        let mut pixels = budget.allocate(4).unwrap();
        pixels.as_mut_slice().fill(7);
        let frame = OwnedFrame {
            info: info(browser(3), 4, 1, 1),
            pixels: pixels.freeze(),
        };
        let old_image = test_image(1, 1);
        let mut slot = Some(ImageSlot {
            image: old_image.clone(),
            sequence: 3,
            generation: 2,
            width: 1,
            height: 1,
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
        assert!(Arc::ptr_eq(&kept.image, &old_image));
        // A later successful transaction can still replace it.
        let replacement = commit_image_change(&mut slot, Some(first), |_| Ok(()))
            .expect("old atlas entry is removed");
        assert!(replacement);
        assert_eq!(slot.as_ref().map(|slot| slot.sequence), Some(9));
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
}
