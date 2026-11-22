use crate::browser::{BrowserChannel, CursorShape, FrameStore, ViewportChange, keymap};
use cef::{
    BrowserHost, CefString, ImplBrowserHost, KeyEvent, KeyEventType, MouseButtonType, MouseEvent,
    Range,
};
use gpui::{
    Anchor, Bounds, Context, Corners, CursorStyle, ElementInputHandler, EntityInputHandler,
    FocusHandle, Focusable, KeyDownEvent, KeyUpEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Render, RenderImage, ScrollDelta, ScrollEvent, SharedString,
    UTF16Selection, Window, canvas, div, prelude::*, px, rgb, size,
};
use gpui_component::ElementExt as _;
use image::{Frame, ImageBuffer, Rgba};
use smallvec::smallvec;
use std::{cell::RefCell, ops::Range as StdRange, rc::Rc, sync::Arc};

pub struct BrowserView {
    frames: Arc<FrameStore>,
    channel: Arc<BrowserChannel>,
    address: SharedString,
    displayed_sequence: u64,
    image: Option<Arc<RenderImage>>,
    focus: FocusHandle,
    /// Content bounds of the browser surface in window coordinates, refreshed
    /// every prepaint so pointer events can be translated into CEF view space.
    content_bounds: Rc<RefCell<Bounds<Pixels>>>,
    /// Drag payload of an in-progress file drag; CEF takes a reference to it for
    /// the whole gesture, so it has to outlive the individual calls.
    drag_data: std::cell::RefCell<Option<cef::DragData>>,
    /// Additional browser instances rendered beside the primary one. Each entry
    /// is a full view plus its channel, because CEF keeps per-browser state.
    tiles: Vec<(gpui::Entity<BrowserView>, Arc<BrowserChannel>)>,
    /// Actual painted menu bounds in window coordinates, including edge snapping.
    menu_bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
    /// One entity and subscription per CEF opening, retained across frames.
    context_menu: Option<ContextMenuView>,
    /// Optional buffered writer for per-paint diagnostics (`POC_PAINT_LOG`).
    paint_log: Option<Rc<std::cell::RefCell<std::io::BufWriter<std::fs::File>>>>,
    /// Text of the active IME composition, if any. Chromium owns the document,
    /// so this is the only text we can honestly report back to the IME.
    composition: Option<String>,
}

struct ContextMenuView {
    id: u64,
    popup: gpui::Entity<gpui_component::menu::PopupMenu>,
    _subscription: gpui::Subscription,
}

impl BrowserView {
    pub fn new(
        frames: Arc<FrameStore>,
        channel: Arc<BrowserChannel>,
        address: String,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            frames,
            channel,
            address: address.into(),
            displayed_sequence: 0,
            image: None,
            focus: cx.focus_handle(),
            content_bounds: Rc::new(RefCell::new(Bounds::default())),
            composition: None,
            drag_data: std::cell::RefCell::new(None),
            tiles: Vec::new(),
            context_menu: None,
            menu_bounds: Rc::new(RefCell::new(None)),
            paint_log: std::env::var("POC_PAINT_LOG").ok().and_then(|path| {
                std::fs::File::create(path)
                    .ok()
                    .map(|file| Rc::new(std::cell::RefCell::new(std::io::BufWriter::new(file))))
            }),
        }
    }

    /// Attaches extra browser instances (see `POC_BROWSERS`).
    pub fn with_tiles(
        mut self,
        tiles: Vec<(gpui::Entity<BrowserView>, Arc<BrowserChannel>)>,
    ) -> Self {
        self.tiles = tiles;
        self
    }

    /// Per-window wiring that needs a `Window`: focus and activation mirroring.
    pub fn install(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        self.sync_activation(window);
        cx.observe_window_activation(window, |view, window, _cx| {
            view.sync_activation(window);
        })
        .detach();
    }

    /// Keeps CEF's *focus* in step with the GPUI window.
    ///
    /// Deliberately does not touch `WasHidden`: losing focus is not the same as
    /// being hidden. Calling `WasHidden(1)` on deactivation makes Chromium stop
    /// producing frames, so the last painted frame freezes until the window is
    /// activated again. Hiding is handled in `sync_viewport`.
    fn sync_activation(&self, window: &Window) {
        if let Some(host) = self.channel.host() {
            host.set_focus(window.is_window_active() as _);
        }
        let was_open = self.channel.has_context_menu();
        // CEF keeps the browser inside `RunContextMenu` until the callback is
        // answered. Losing focus must dismiss the menu, or the page stays
        // blocked with a popup nobody can click.
        if !window.is_window_active() && was_open {
            self.close_context_menu();
        }
    }

    /// Dismisses the menu without choosing anything.
    pub fn close_context_menu(&self) -> bool {
        self.channel.resolve_context_menu(None)
    }

    fn refresh_image(&mut self, window: &mut Window) {
        let Some(frame) = self.frames.latest() else {
            return;
        };
        if frame.sequence == self.displayed_sequence {
            return;
        }
        // Rgba is the storage type; RenderImage consumes premultiplied BGRA.
        // Its exclusive CPU copy must reserve from the same Browser budget.
        // The ledger keeps a strong owner until it can atomically unwrap and
        // actually destroy the last RenderImage, then refunds the allocation;
        // drop_image alone (or a Weak count of zero) is not that proof.
        let Ok(copy) = frame.bgra.copy_buffer() else {
            return; // Observable budget rejection; retain the previous good image.
        };
        let Ok(next) = copy.try_into_owner(|pixels| {
            let buffer = ImageBuffer::<Rgba<u8>, _>::from_raw(frame.width, frame.height, pixels)
                .ok_or(rust_cef_core::frame_budget::FrameError::InvalidDimensions)?;
            Ok::<_, rust_cef_core::frame_budget::FrameError>(Arc::new(RenderImage::new(smallvec![
                Frame::new(buffer)
            ])))
        }) else {
            return;
        };
        if let Some(previous) = self.image.replace(next) {
            // Release the old atlas entry rather than retaining every video frame.
            let _ = window.drop_image(previous);
        }
        self.displayed_sequence = frame.sequence;
    }

    /// Pointer position in CEF view (DIP) coordinates.
    fn view_point(&self, position: Point<Pixels>) -> Option<(i32, i32)> {
        let bounds = *self.content_bounds.borrow();
        keymap::to_view_point(
            (f32::from(position.x), f32::from(position.y)),
            (f32::from(bounds.origin.x), f32::from(bounds.origin.y)),
            (f32::from(bounds.size.width), f32::from(bounds.size.height)),
        )
    }

    fn mouse_event(
        &self,
        position: Point<Pixels>,
        modifiers: gpui::Modifiers,
    ) -> Option<MouseEvent> {
        let (x, y) = self.view_point(position)?;
        Some(MouseEvent {
            x,
            y,
            modifiers: keymap::modifier_flags(
                modifiers.shift,
                modifiers.control,
                modifiers.alt,
                modifiers.platform,
            ),
        })
    }

    fn host(&self) -> Option<BrowserHost> {
        self.channel.host()
    }
}

/// Raw `HWND` of a gpui window, for Win32 calls gpui does not wrap.
fn window_hwnd(window: &Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // Fully qualified: `Window` also has an inherent `window_handle()` that
    // returns gpui's own handle type, which would shadow the trait method.
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

/// Places the IME candidate window at `caret`, in window coordinates.
///
/// gpui sets the candidate position only once, on `WM_IME_STARTCOMPOSITION`,
/// and at that moment CEF has not yet reported the composition rectangle: the
/// caret still reads as `(0, 0)` and the candidate list opens at the window
/// origin. CEF then reports the real rectangle through
/// `on_ime_composition_range_changed`, which is too late for gpui's one-shot
/// placement. CefSharp avoids this because its IME handler runs on every
/// Places the IME candidate window at `caret`.
///
/// This follows CefSharp's `WpfIMEKeyboardHandler.MoveImeWindow` step by step,
/// because each part exists for a reason found the hard way there:
///
/// * `ImmSetCandidateWindow` alone is not enough for Chinese IMEs. With TSF
///   (Text Services Framework) disabled they ignore it and position the
///   candidate list from the **system caret** instead, so a temporary 1x1 caret
///   has to exist and be moved with `SetCaretPos`.
/// * Chinese IMEs additionally want the composition window moved, or the
///   in-line composition text stays where it was first shown.
/// * The anchor is the caret's bottom-right corner: `x + width`, `y + height`.
///
/// `caret` is `(x, y, height)` in CEF view DIP, `scale` converts to client px.
fn place_ime_candidate_window(
    hwnd: isize,
    caret: (i32, i32, i32, i32),
    scale: f32,
    surface_origin: (f32, f32),
) {
    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::UI::Input::Ime::{
        CANDIDATEFORM, CFS_CANDIDATEPOS, CFS_POINT, COMPOSITIONFORM, ImmGetContext,
        ImmReleaseContext, ImmSetCandidateWindow, ImmSetCompositionWindow,
    };
    use windows::Win32::UI::WindowsAndMessaging::{CreateCaret, SetCaretPos};

    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    // CefSharp anchors at the bottom-right of the character box. That puts the
    // candidate list a full line-height below the text, which reads as a gap;
    // the vertical anchor is therefore the vertical centre of the box, halving
    // that distance while still clearing the glyphs.
    let (left, top, width, height) = caret;
    // Caret is relative to the CEF view; add the surface inset so the result is
    // in client pixels, which is what the IME APIs expect.
    let x = ((left + width) as f32 * scale) + surface_origin.0;
    let y = ((top as f32 + height as f32 / 2.0) * scale) + surface_origin.1;
    let (x, y) = (x as i32, y as i32);

    // SAFETY: `hwnd` is the window this browser is painted into, and every call
    // here is a plain Win32 call on the UI thread (the pump applies this).
    unsafe {
        // A 1x1 bitmap-less caret is enough for Chinese IMEs to read a position
        // via GetCaretPos. Created on every placement: CreateCaret is cheap and
        // tracking ownership across focus changes is error-prone.
        let _ = CreateCaret(hwnd, None, 1, 1);
        let _ = SetCaretPos(x, y);

        let ctx = ImmGetContext(hwnd);
        if ctx.is_invalid() {
            return;
        }
        let _ = ImmSetCandidateWindow(
            ctx,
            &CANDIDATEFORM {
                dwIndex: 0,
                dwStyle: CFS_CANDIDATEPOS,
                ptCurrentPos: POINT { x, y },
                rcArea: Default::default(),
            },
        );
        // Chinese IMEs keep the in-line composition text at the composition
        // window's position, which otherwise stays where it was first set.
        let _ = ImmSetCompositionWindow(
            ctx,
            &COMPOSITIONFORM {
                dwStyle: CFS_POINT,
                ptCurrentPos: POINT { x, y },
                rcArea: Default::default(),
            },
        );
        let _ = ImmReleaseContext(hwnd, ctx);
    }
}

/// Applies any pending IME candidate-window placement.
///
/// Called from the UI thread after CEF's dispatch, because the Win32 IME calls
/// must not run on CEF's callback thread.
pub fn apply_pending_ime_reposition(channel: &crate::browser::BrowserChannel, scale: f32) {
    let origin = channel.surface_origin();
    while let Some((hwnd, caret)) = channel.take_ime_reposition() {
        place_ime_candidate_window(hwnd, caret, scale, origin);
    }
}

/// Maps a CEF cursor type onto the closest GPUI cursor style.
fn cursor_style(shape: CursorShape) -> CursorStyle {
    match shape {
        CursorShape::Pointer => CursorStyle::PointingHand,
        CursorShape::IBeam => CursorStyle::IBeam,
        CursorShape::Crosshair => CursorStyle::Crosshair,
        CursorShape::Grab => CursorStyle::OpenHand,
        CursorShape::Grabbing => CursorStyle::ClosedHand,
        // `Pointer` is CEF's CT_HAND (a link); OpenHand is the "can grab" state,
        // which Chromium reports as CT_GRAB instead.
        CursorShape::NotAllowed => CursorStyle::OperationNotAllowed,
        CursorShape::ContextMenu => CursorStyle::ContextualMenu,
        CursorShape::ResizeEastWest => CursorStyle::ResizeLeftRight,
        CursorShape::ResizeNorthSouth => CursorStyle::ResizeUpDown,
        CursorShape::ResizeNortheastSouthwest => CursorStyle::ResizeUpLeftDownRight,
        CursorShape::ResizeNorthwestSoutheast => CursorStyle::ResizeUpRightDownLeft,
        CursorShape::ResizeColumn => CursorStyle::ResizeColumn,
        CursorShape::ResizeRow => CursorStyle::ResizeRow,
        // GPUI has no four-arrow "move" cursor; Arrow is the closest neutral.
        CursorShape::Move => CursorStyle::Arrow,
        CursorShape::VerticalText => CursorStyle::IBeamCursorForVerticalLayout,
        CursorShape::Cell => CursorStyle::Crosshair,
        // gpui-pre dropped the "hidden cursor" style; hiding is now a platform
        // hint (`hide_cursor_until_mouse_moves`), not a style. Chromium asks for
        // CT_NONE mainly while content is dragged and it draws its own cursor,
        // so a plain arrow is the closest available behaviour.
        CursorShape::None => CursorStyle::Arrow,
        CursorShape::Arrow => CursorStyle::Arrow,
    }
}

/// `CefRange::InvalidRange()` is (-1, -1); the binding's fields are `u32`, so
/// the same bit pattern has to be written explicitly.
fn invalid_range() -> Range {
    Range {
        from: u32::MAX,
        to: u32::MAX,
    }
}

fn cef_button(button: MouseButton) -> Option<MouseButtonType> {
    match button {
        MouseButton::Left => Some(MouseButtonType::LEFT),
        MouseButton::Right => Some(MouseButtonType::RIGHT),
        MouseButton::Middle => Some(MouseButtonType::MIDDLE),
        _ => None,
    }
}

/// Feeds the surface size and DPI to CEF. A changed scale also invalidates
/// Chromium's cached screen info, otherwise the compositor keeps painting at
/// the old device size and the image is upscaled (visibly blurry).
fn sync_viewport(channel: &BrowserChannel, bounds: Bounds<Pixels>, window: &Window) {
    let width = f32::from(bounds.size.width).round().max(1.0) as i32;
    let height = f32::from(bounds.size.height).round().max(1.0) as i32;
    let change = channel.set_viewport(width, height, window.scale_factor());
    let Some(host) = channel.host() else { return };
    match change {
        ViewportChange::Size => host.was_resized(),
        ViewportChange::Scale => {
            host.notify_screen_info_changed();
            host.was_resized();
        }
        ViewportChange::None => {}
    }
    // A minimized window collapses to a 1px client area; pausing paints there is
    // correct, while every other state must keep rendering.
    let drawable = width > 1 && height > 1;
    host.was_hidden(!drawable as _);
}

/// Placement of the painted frame inside `bounds`.
///
/// Frames arrive in device pixels; dividing by the window scale gives the DIP box
/// the frame was painted for. That box is drawn **unscaled**, anchored at the
/// surface origin.
///
/// CEF is asynchronous, so mid-drag the newest frame is one step behind the
/// surface. Scaling it to fill (what this used to do) resamples every pixel and,
/// because width and height were fitted separately, squashes text: that was the
/// jelly and the distortion. Drawing it 1:1 instead means the not-yet-painted
/// strip simply shows the surface background for a frame or two, and the page
/// background is the same colour by construction, so nothing flickers.
fn frame_bounds(bounds: Bounds<Pixels>, image: &RenderImage, scale: f32) -> Bounds<Pixels> {
    let image_size = image.size(0);
    let width = u32::from(image_size.width) as f32 / scale;
    let height = u32::from(image_size.height) as f32 / scale;
    if width <= 0.0 || height <= 0.0 {
        return bounds;
    }
    Bounds {
        origin: bounds.origin,
        size: size(px(width), px(height)),
    }
}

impl Render for BrowserView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_image(window);
        let image = self.image.clone();
        let entity = cx.entity();
        let focus = self.focus.clone();
        let channel = self.channel.clone();
        let content_bounds = self.content_bounds.clone();
        let cursor = cursor_style(self.channel.cursor());
        let self_channel = self.channel.clone();
        let paint_log = self.paint_log.clone();

        let surface = canvas(
            move |bounds, window, _cx| {
                *content_bounds.borrow_mut() = bounds;
                sync_viewport(&channel, bounds, window);
            },
            move |bounds, _, window, cx| {
                window.set_window_cursor_style(cursor);
                if let Some(hwnd) = window_hwnd(window) {
                    self_channel.set_window_hwnd(hwnd);
                }
                // CEF's caret is relative to the surface; the IME needs client
                // pixels, so remember the surface inset (in device pixels).
                let scale = window.scale_factor();
                self_channel.set_surface_origin(
                    f32::from(bounds.origin.x) * scale,
                    f32::from(bounds.origin.y) * scale,
                );
                if let Some(image) = &image {
                    let target = frame_bounds(bounds, image, window.scale_factor());
                    // Diagnostics are off by default and deliberately cheap: the
                    // file is opened once and lines are buffered, because doing
                    // open/append/close per frame collapsed the frame rate when
                    // the window was large.
                    if let Some(log) = paint_log.as_ref() {
                        let image_size = image.size(0);
                        let line = format!(
                            "paint surface={:.1}x{:.1} frame={:.1}x{:.1} image={}x{}",
                            f32::from(bounds.size.width),
                            f32::from(bounds.size.height),
                            f32::from(target.size.width),
                            f32::from(target.size.height),
                            u32::from(image_size.width),
                            u32::from(image_size.height),
                        );
                        if let Ok(mut file) = log.try_borrow_mut() {
                            use std::io::Write as _;
                            let _ = writeln!(file, "{line}");
                        }
                    }
                    // gpui-pre split the clip rect from the image rect. The frame
                    // is clipped to the surface and drawn at the size it was
                    // painted for, so both are `target`.
                    let _ = window.paint_image(
                        target,
                        target,
                        Corners::default(),
                        image.clone(),
                        0,
                        false,
                    );
                }
                // Routes WM_CHAR / WM_IME_COMPOSITION into this view while it
                // holds focus, which is what our EntityInputHandler forwards to CEF.
                window.handle_input(&focus, ElementInputHandler::new(bounds, entity.clone()), cx);
            },
        )
        // The canvas takes the row that is left after the status bar. Sizing it
        // `size_full()` inside the same flex container made it compete with the
        // status line for height, so its bounds kept oscillating during a resize
        // and CEF was asked for a different viewport every frame.
        .flex_1()
        .w_full()
        .min_h_0();

        let mut content = div()
            .id("cef-browser-surface")
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .m_3()
            .overflow_hidden()
            // Matches CefBrowserSettings::background_color and the demo page,
            // so the strip CEF has not painted yet is invisible during resize.
            .bg(rgb(0xf3f6fb))
            // No `.cursor_default()` here: it is a hitbox-level style on an
            // ancestor, and it wins over the paint-phase
            // `set_window_cursor_style` used for the CEF cursor. That is why the
            // pointer stayed an arrow over text inputs.
            .track_focus(&self.focus)
            .on_any_mouse_down(cx.listener(|view, event: &MouseDownEvent, window, cx| {
                // A click on the menu belongs to the menu, not to the page. The
                // menu is a child of this element, so this bubble-phase listener
                // sees the event too; without the rect check it would forward the
                // click to Chromium and close the menu underneath the cursor.
                if view.menu_contains(event.position) {
                    return;
                }
                // Complete the OLD callback before forwarding any outside
                // press, including right-click. CEF can then open a fresh menu
                // at this position. Never cancel after sending the new event.
                view.close_context_menu();
                view.focus.focus(window, cx);
                view.handle_mouse_down(event);
                cx.notify();
            }))
            .capture_any_mouse_up(cx.listener(|view, event: &MouseUpEvent, _window, _cx| {
                view.handle_mouse_up(event);
            }))
            .on_mouse_move(cx.listener(|view, event: &MouseMoveEvent, _window, _cx| {
                view.handle_mouse_move(event.position, event.pressed_button, event.modifiers);
            }))
            .on_hover(cx.listener(|view, hovered: &bool, _window, _cx| {
                if !*hovered {
                    view.handle_mouse_leave();
                }
            }))
            .on_drag_move::<gpui::ExternalPaths>(cx.listener(
                |view, _event: &gpui::DragMoveEvent<gpui::ExternalPaths>, _window, _cx| {
                    // Keeps the in-page "ghost" in step while a file is dragged
                    // over the browser, so the drop target highlight follows.
                    let _ = view.host();
                },
            ))
            .on_scroll(cx.listener(|view, event: &ScrollEvent, _window, _cx| {
                view.handle_scroll(event.position, event.modifiers, event.delta);
            }))
            .can_drop(|value, _, _| value.is::<gpui::ExternalPaths>())
            .on_drop(
                cx.listener(|view, paths: &gpui::ExternalPaths, _window, _cx| {
                    view.handle_file_drop(paths.paths());
                }),
            )
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, _window, _cx| {
                // Deliberately lets the event bubble: gpui's Windows backend
                // then runs TranslateMessage, and the resulting WM_CHAR /
                // WM_IME_COMPOSITION reach our EntityInputHandler, which is
                // what inserts text into Chromium.
                view.forward_key_down(event);
            }))
            .on_key_up(cx.listener(|view, event: &KeyUpEvent, _window, _cx| {
                view.forward_key_up(event);
            }))
            .child(surface);

        let status = div()
            .px_4()
            .pb_3()
            .text_sm()
            .text_color(rgb(0x9eabba))
            .child(format!(
                "Frame {} · {} × {} @ {:.2}x · interactive",
                self.displayed_sequence,
                self.channel.viewport().width,
                self.channel.viewport().height,
                self.channel.viewport().scale,
            ));

        // Context menu, drawn as a gpui overlay. CEF is blocked inside
        // RunContextMenu until we answer, so this must be dismissed on every
        // path (item click, outside click, window blur).
        match self
            .channel
            .with_context_menu(|open| (open.id, open.menu.clone()))
        {
            Some((id, menu)) => {
                content = content.child(self.render_context_menu(id, &menu, window, cx));
            }
            None => {
                if let Some(menu) = self.context_menu.take() {
                    if menu.popup.focus_handle(cx).is_focused(window) {
                        self.focus.focus(window, cx);
                    }
                }
                *self.menu_bounds.borrow_mut() = None;
            }
        }

        if self.image.is_none() {
            // Keep the placeholder visible on top of the (empty) surface.
            content = content.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(0x9eabba))
                    .child("Waiting for Chromium's first frame..."),
            );
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x101318))
            .child(
                div()
                    .h(px(48.))
                    .px_4()
                    .flex()
                    .items_center()
                    .bg(rgb(0x1a2029))
                    .child(div().text_sm().text_color(rgb(0xd7e0ec)).child(
                        if self.channel.is_accelerated() {
                            "GPUI + CEF · D3D11 shared texture"
                        } else {
                            "GPUI + CEF · software OSR"
                        },
                    ))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x9eabba))
                            .child(self.address.clone()),
                    ),
            )
            .child(if self.tiles.is_empty() {
                content.into_any_element()
            } else {
                let mut row = div().flex().flex_1().min_h_0().child(content);
                for (entity, _) in &self.tiles {
                    row = row.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .m_3()
                            .ml_0()
                            .overflow_hidden()
                            .child(entity.clone()),
                    );
                }
                row.into_any_element()
            })
            .child(status)
    }
}

impl BrowserView {
    /// Runs one scripted input event through the same handlers the element
    /// listeners call, so the self-test covers the translation layer.
    pub fn apply_probe(&mut self, input: &gpui::PlatformInput) {
        use gpui::PlatformInput;
        match input {
            PlatformInput::MouseDown(event) => self.handle_mouse_down(event),
            PlatformInput::MouseUp(event) => self.handle_mouse_up(event),
            PlatformInput::MouseMove(event) => {
                self.handle_mouse_move(event.position, event.pressed_button, event.modifiers)
            }
            PlatformInput::Scroll(event) => {
                self.handle_scroll(event.position, event.modifiers, event.delta)
            }
            PlatformInput::KeyDown(event) => {
                self.forward_key_down(event);
            }
            PlatformInput::KeyUp(event) => {
                self.forward_key_up(event);
            }
            PlatformInput::MouseExited(_) => self.handle_mouse_leave(),
            _ => {}
        }
    }

    /// Draws the open context menu positioned at the click point.
    ///
    /// Chromium reports the position in view coordinates, so it is offset by the
    /// surface origin. The menu is a normal gpui element: it participates in the
    /// scene like anything else, which is the whole reason for doing OSR.
    fn render_context_menu(
        &mut self,
        menu_id: u64,
        menu: &crate::browser::context_menu::ContextMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use crate::browser::context_menu::MenuEntry;
        use gpui_component::menu::{PopupMenu, PopupMenuItem};

        // Like gpui-component's ContextMenu, build once per opening. Rebuilding
        // per frame detached DismissEvent from the visible popup and reset its
        // focus/selection/click state.
        if self
            .context_menu
            .as_ref()
            .is_none_or(|menu| menu.id != menu_id)
        {
            let entries = menu.entries.clone();
            let channel = self.channel.clone();
            let focus = self.focus.clone();
            let popup = PopupMenu::build(window, cx, move |this, _window, _cx| {
                let mut menu = this.action_context(focus);
                for entry in &entries {
                    match entry {
                        MenuEntry::Separator => menu = menu.item(PopupMenuItem::separator()),
                        MenuEntry::Command {
                            command_id,
                            label,
                            enabled,
                            checked,
                        } => {
                            let id = *command_id;
                            let channel = channel.clone();
                            let mut item = PopupMenuItem::new(label.clone()).disabled(!enabled);
                            if *checked {
                                item = item.checked(true);
                            }
                            item = item.on_click(move |_event, _window, _cx| {
                                channel.resolve_context_menu_for(Some(menu_id), Some(id));
                            });
                            menu = menu.item(item);
                        }
                    }
                }
                menu
            });
            let subscription = cx.subscribe(
                &popup,
                move |view, _menu, _event: &gpui::DismissEvent, cx| {
                    if view.channel.resolve_context_menu_for(Some(menu_id), None) {
                        cx.notify();
                    }
                },
            );
            popup.focus_handle(cx).focus(window, cx);
            self.context_menu = Some(ContextMenuView {
                id: menu_id,
                popup,
                _subscription: subscription,
            });
        }
        let popup = self.context_menu.as_ref().unwrap().popup.clone();
        let menu_bounds = self.menu_bounds.clone();
        let surface = *self.content_bounds.borrow();

        // Anchored to the click point. `deferred` lifts the menu above the
        // browser image, which paints over a normal child.
        //
        // CEF gives view coordinates; Anchored expects window coordinates.
        // Let Anchored snap using the real menu size, not estimated row heights.
        let window_origin = Point {
            x: surface.origin.x + px(menu.x as f32),
            y: surface.origin.y + px(menu.y as f32),
        };
        gpui::deferred(
            gpui::anchored()
                .anchor(Anchor::TopLeft)
                .position(window_origin)
                .child(div().child(popup).on_prepaint(move |bounds, _, _| {
                    *menu_bounds.borrow_mut() = Some(bounds);
                }))
                .snap_to_window_with_margin(px(4.)),
        )
        .into_any_element()
    }

    /// Whether `position` falls inside the open menu.
    fn menu_contains(&self, position: Point<Pixels>) -> bool {
        match *self.menu_bounds.borrow() {
            Some(bounds) => bounds.contains(&position),
            None => false,
        }
    }

    /// Pointer press forwarded to CEF.
    fn handle_mouse_down(&self, event: &MouseDownEvent) {
        let Some(button) = cef_button(event.button) else {
            return;
        };
        let Some(mouse) = self.mouse_event(event.position, event.modifiers) else {
            return;
        };
        if let Some(host) = self.host() {
            host.send_mouse_click_event(Some(&mouse), button, 0, event.click_count as i32);
        }
    }

    /// Pointer release. Captured at the window level so a drag that ends
    /// outside the surface still releases the button inside Chromium.
    fn handle_mouse_up(&self, event: &MouseUpEvent) {
        let Some(button) = cef_button(event.button) else {
            return;
        };
        let Some(mouse) = self.mouse_event(event.position, event.modifiers) else {
            return;
        };
        if let Some(host) = self.host() {
            host.send_mouse_click_event(Some(&mouse), button, 1, event.click_count as i32);
        }
    }

    fn handle_mouse_move(
        &self,
        position: Point<Pixels>,
        pressed: Option<MouseButton>,
        modifiers: gpui::Modifiers,
    ) {
        let Some((x, y)) = self.view_point(position) else {
            return;
        };
        let flags = keymap::modifier_flags(
            modifiers.shift,
            modifiers.control,
            modifiers.alt,
            modifiers.platform,
        );
        // Chromium learns the button state only from the modifier flags; a move
        // without them is a hover, so dragging inside the page would do nothing.
        let mouse =
            crate::browser::dnd::drag_move_event(x, y, pressed == Some(MouseButton::Left), flags);
        self.channel.set_last_cursor(x, y);
        if let Some(host) = self.host() {
            host.send_mouse_move_event(Some(&mouse), 0);
        }
    }

    /// CEF only clears hover state (link underlines, tooltips) when told the
    /// pointer left the view, and that call still needs a position.
    fn handle_mouse_leave(&self) {
        let Some(host) = self.host() else {
            return;
        };
        let bounds = *self.content_bounds.borrow();
        let mouse = MouseEvent {
            x: (f32::from(bounds.size.width) / 2.0).round() as i32,
            y: (f32::from(bounds.size.height) / 2.0).round() as i32,
            modifiers: 0,
        };
        host.send_mouse_move_event(Some(&mouse), 1);
    }

    /// Drives a press -> move-with-button-held -> release sequence, the same
    /// CEF calls a real mouse drag produces. Used by `POC_SELF_TEST=3`.
    pub fn run_drag_probe(&self, index: &mut usize) {
        let Some(host) = self.host() else { return };
        let (start, end) = ((80, 60), (260, 180));
        match *index {
            0 => host.send_mouse_move_event(
                Some(&crate::browser::dnd::drag_position(start.0, start.1)),
                0,
            ),
            1 => host.send_mouse_click_event(
                Some(&crate::browser::dnd::drag_position(start.0, start.1)),
                MouseButtonType::LEFT,
                0,
                1,
            ),
            2 | 3 => {
                // Two move events while held: Chromium only starts a drag after
                // the pointer leaves the click slop area. The button bit is what
                // makes it a drag rather than a hover.
                let t = if *index == 2 { 0.5 } else { 1.0 };
                let x = start.0 as f32 + (end.0 - start.0) as f32 * t;
                let y = start.1 as f32 + (end.1 - start.1) as f32 * t;
                host.send_mouse_move_event(
                    Some(&crate::browser::dnd::drag_move_event(
                        x as i32, y as i32, true, 0,
                    )),
                    0,
                );
            }
            4 => host.send_mouse_click_event(
                Some(&crate::browser::dnd::drag_position(end.0, end.1)),
                MouseButtonType::LEFT,
                1,
                1,
            ),
            _ => {}
        }
        *index += 1;
    }

    /// Drags the page's own `#dragme` element onto its `#dropzone`.
    ///
    /// Coordinates are in CEF view DIP: the page places the handles at
    /// x=820..1000, y=20..240, so the gesture starts at (900, 60) and ends at
    /// (900, 190). This is the HTML5 drag path, which CEF only starts if the
    /// renderer asked for `StartDragging`.
    pub fn run_html5_drag_probe(&self, index: &mut usize) {
        let Some(host) = self.host() else { return };
        let (start, end) = ((900, 60), (900, 190));
        match *index {
            0 => host.send_mouse_move_event(
                Some(&crate::browser::dnd::drag_position(start.0, start.1)),
                0,
            ),
            1 => host.send_mouse_click_event(
                Some(&crate::browser::dnd::drag_position(start.0, start.1)),
                MouseButtonType::LEFT,
                0,
                1,
            ),
            2..=5 => {
                let t = (*index - 1) as f32 / 5.0;
                let x = start.0 as f32 + (end.0 - start.0) as f32 * t;
                let y = start.1 as f32 + (end.1 - start.1) as f32 * t;
                host.send_mouse_move_event(
                    Some(&crate::browser::dnd::drag_move_event(
                        x as i32, y as i32, true, 0,
                    )),
                    0,
                );
            }
            6 => host.send_mouse_click_event(
                Some(&crate::browser::dnd::drag_position(end.0, end.1)),
                MouseButtonType::LEFT,
                1,
                1,
            ),
            _ => {}
        }
        *index += 1;
    }

    /// Starts a CEF drag targeted at this browser.
    ///
    /// Real files arrive as a sequence: enter, then repeated over events while
    /// the pointer moves, then drop. Chromium needs the over events because the
    /// page's `dragover` handler is what calls `preventDefault()` and makes the
    /// element a valid drop target; a drop with no accepted over falls through
    /// to the browser's default action, which is to navigate to the file.
    pub fn begin_file_drag(&self, paths: &[std::path::PathBuf], position: (i32, i32)) -> bool {
        let Some(host) = self.host() else {
            return false;
        };
        let Some(data) = crate::browser::dnd::drag_data_for_paths(paths) else {
            return false;
        };
        let event = crate::browser::dnd::drag_position(position.0, position.1);
        let ops = crate::browser::dnd::mask(crate::browser::dnd::ACCEPTED_OPERATIONS);
        let mut slot = self.drag_data.borrow_mut();
        *slot = Some(data);
        let Some(data) = slot.as_mut() else {
            return false;
        };
        host.drag_target_drag_enter(Some(data), Some(&event), ops);
        host.drag_target_drag_over(Some(&event), ops);
        true
    }

    /// Moves an in-progress drag. This is what lets the page keep its drop
    /// target armed between the enter and the drop.
    pub fn move_file_drag(&self, position: (i32, i32)) {
        let Some(host) = self.host() else { return };
        if self.drag_data.borrow().is_none() {
            return;
        }
        let event = crate::browser::dnd::drag_position(position.0, position.1);
        let ops = crate::browser::dnd::mask(crate::browser::dnd::ACCEPTED_OPERATIONS);
        host.drag_target_drag_over(Some(&event), ops);
    }

    /// Finishes the drag. `drop = false` cancels it instead.
    pub fn end_file_drag(&self, position: (i32, i32), drop: bool) {
        let Some(host) = self.host() else { return };
        let had_data = self.drag_data.borrow_mut().take().is_some();
        if !had_data {
            return;
        }
        let event = crate::browser::dnd::drag_position(position.0, position.1);
        if drop {
            host.drag_target_drop(Some(&event));
        } else {
            host.drag_target_drag_leave();
        }
    }

    /// Centre of the browser surface in view coordinates.
    pub fn surface_center(&self) -> (i32, i32) {
        let bounds = *self.content_bounds.borrow();
        (
            (f32::from(bounds.size.width) / 2.0).round() as i32,
            (f32::from(bounds.size.height) / 2.0).round() as i32,
        )
    }

    /// One-shot convenience used by the self-test: begin the drag now, and let
    /// the caller finish it on a later frame.
    ///
    /// The enter and drop cannot be in the same turn: the page's `dragover`
    /// handler has to run in the renderer before the drop arrives, otherwise
    /// Chromium falls back to navigating to the dropped file.
    pub fn handle_file_drop(&self, paths: &[std::path::PathBuf]) -> bool {
        self.begin_file_drag(paths, self.surface_center())
    }

    /// Advances the self-test file drag by one frame: over, over, drop.
    pub fn step_file_drop(&self, step: usize) -> bool {
        let centre = self.surface_center();
        match step {
            0 | 1 => {
                self.move_file_drag((centre.0 + 2 + step as i32 * 2, centre.1 + 2));
                true
            }
            2 => {
                self.end_file_drag((centre.0 + 6, centre.1 + 6), true);
                false
            }
            _ => false,
        }
    }

    fn handle_scroll(
        &self,
        position: Point<Pixels>,
        modifiers: gpui::Modifiers,
        delta: ScrollDelta,
    ) {
        let Some(mouse) = self.mouse_event(position, modifiers) else {
            return;
        };
        // CEF expects Windows wheel units: 120 per notch.
        let (dx, dy) = match delta {
            ScrollDelta::Lines(delta) => keymap::wheel_units((delta.x, delta.y)),
            ScrollDelta::Pixels(delta) => {
                keymap::wheel_units_from_pixels((f32::from(delta.x), f32::from(delta.y)))
            }
        };
        if let Some(host) = self.host() {
            host.send_mouse_wheel_event(Some(&mouse), dx, dy);
        }
    }

    fn key_event(
        &self,
        key: &str,
        key_char: Option<&str>,
        type_: KeyEventType,
        modifiers: gpui::Modifiers,
        repeat: bool,
    ) -> Option<KeyEvent> {
        let mut event = KeyEvent::default();
        event.type_ = type_;
        event.modifiers = keymap::modifier_flags(
            modifiers.shift,
            modifiers.control,
            modifiers.alt,
            modifiers.platform,
        );
        if repeat {
            event.modifiers |= keymap::EVENTFLAG_IS_REPEAT;
        }

        if type_ == KeyEventType::CHAR {
            let ch = key_char
                .and_then(|text| text.encode_utf16().next())
                .or_else(|| keymap::char_for_key(key))?;
            event.character = ch;
            event.unmodified_character = ch;
            event.windows_key_code = ch as i32;
            event.native_key_code = ch as i32;
            return Some(event);
        }

        event.windows_key_code = keymap::windows_key_code(key)?;
        event.native_key_code = event.windows_key_code;
        Some(event)
    }

    /// Sends only the key-down part. Text is inserted separately from
    /// `replace_text_in_range`, which is fed by GPUI's `WM_CHAR` and
    /// `WM_IME_COMPOSITION` handling.
    ///
    /// Emitting a `KEYEVENT_CHAR` here as well would double-insert every
    /// character whenever an IME is active: the IME path already commits the
    /// text. It is also wrong for IME composition, where the keystroke starts a
    /// composition instead of producing a character.
    fn forward_key_down(&self, event: &KeyDownEvent) -> bool {
        let Some(host) = self.host() else {
            return false;
        };
        let keystroke = &event.keystroke;
        let Some(raw) = self.key_event(
            &keystroke.key,
            keystroke.key_char.as_deref(),
            KeyEventType::RAWKEYDOWN,
            keystroke.modifiers,
            event.is_held,
        ) else {
            return false;
        };
        host.send_key_event(Some(&raw));
        true
    }

    fn forward_key_up(&self, event: &KeyUpEvent) -> bool {
        let Some(host) = self.host() else {
            return false;
        };
        let keystroke = &event.keystroke;
        let Some(up) = self.key_event(
            &keystroke.key,
            keystroke.key_char.as_deref(),
            KeyEventType::KEYUP,
            keystroke.modifiers,
            false,
        ) else {
            return false;
        };
        host.send_key_event(Some(&up));
        true
    }
}

impl EntityInputHandler for BrowserView {
    /// Only report text acceptance while Chromium has a text field focused.
    ///
    /// Returning `true` unconditionally (the trait default) let the Windows IME
    /// attach to the whole view, so a Chinese candidate window opened over plain
    /// text and its keystrokes were swallowed instead of reaching the page.
    /// `OnVirtualKeyboardRequested` is CEF's own signal for this.
    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        // Taken straight from CEF's focus state: the platform polls this when it
        // decides whether to offer an IME context, and a stale `true` is what let
        // the Chinese candidate window appear over plain text.
        self.channel.accepts_text()
    }

    fn text_for_range(
        &mut self,
        range: StdRange<usize>,
        _adjusted_range: &mut Option<StdRange<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        // Only the composing text is mirrored here; everything else lives in
        // Chromium. Answering with the composition slice keeps IMEs that probe
        // the range from receiving text they would re-insert.
        let Some(text) = self.composition.as_ref() else {
            return Some(String::new());
        };
        let units: Vec<u16> = text.encode_utf16().collect();
        let start = range.start.min(units.len());
        let end = range.end.min(units.len()).max(start);
        Some(String::from_utf16_lossy(&units[start..end]))
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        // gpui's Windows IME code asks for this first, and only then calls
        // `bounds_for_range` to place the candidate window. Returning `None`
        // here would leave the candidate window at the screen origin.
        let len = self
            .composition
            .as_ref()
            .map(|text| text.encode_utf16().count())
            .unwrap_or(0);
        Some(UTF16Selection {
            range: 0..len,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<StdRange<usize>> {
        self.composition
            .as_ref()
            .map(|text| 0..text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        // gpui calls this on every focus/selection change, including when no
        // composition exists. `ImeFinishComposingText` with nothing composing is
        // a no-op inside Chromium but still enters its IME pipeline, and doing
        // that from a keydown handler (e.g. Backspace) trips a Chromium
        // assertion. Only notify CEF when we were actually composing.
        let was_composing = self.composition.take().is_some();
        if !was_composing {
            return;
        }
        if let Some(host) = self.host() {
            // Keep the committed text: `keep_selection = 1`.
            host.ime_finish_composing_text(1);
        }
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<StdRange<usize>>,
        text: &str,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        let was_composing = self.composition.take().is_some();
        let Some(host) = self.host() else { return };
        if text.is_empty() {
            // Only an active composition can be cancelled; calling
            // ImeCancelComposition otherwise reaches Chromium with no
            // composition node, which is where Backspace crashed.
            if was_composing {
                host.ime_cancel_composition();
            }
            return;
        }
        let cef_text = CefString::from(text);
        // cefclient passes an invalid replacement range; the u32 fields carry
        // the -1 bit pattern of CefRange::InvalidRange().
        host.ime_commit_text(Some(&cef_text), Some(&invalid_range()), 0);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<StdRange<usize>>,
        new_text: &str,
        _new_selected_range: Option<StdRange<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        let Some(host) = self.host() else { return };
        let utf16_len = new_text.encode_utf16().count();
        self.composition = if utf16_len == 0 {
            None
        } else {
            Some(new_text.to_string())
        };
        // The candidate window is positioned from `on_ime_composition_range_changed`
        // instead of here: at this point CEF has not produced the rectangle yet,
        // so anything placed now would use a stale anchor.
        let cef_text = CefString::from(new_text);
        // Selection range within the composition, in UTF-16 units.
        let selection = Range {
            from: 0,
            to: utf16_len as u32,
        };
        let mut underline = cef::CompositionUnderline::default();
        underline.range = selection.clone();
        underline.color = 0xff00_0000;
        underline.thick = 1;
        let underlines = [underline];
        // Without an explicit invalid replacement range Chromium never reports
        // compositionstart, so keystrokes would be typed but not composed.
        host.ime_set_composition(
            Some(&cef_text),
            Some(&underlines),
            Some(&invalid_range()),
            Some(&selection),
        );
    }

    fn bounds_for_range(
        &mut self,
        _range: StdRange<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // Chromium owns the caret position inside the page; it reports the
        // composition rectangle, which we translate into window coordinates.
        // Falling back to the surface origin is still better than None, which
        // would drop the IME candidate window in a screen corner.
        let bounds = *self.content_bounds.borrow();
        let (left, top, width, height) = self.channel.ime_caret().unwrap_or((0, 0, 1, 24));
        let origin = Point {
            x: bounds.origin.x + px((left + width) as f32),
            y: bounds.origin.y + px(top as f32),
        };
        Some(Bounds {
            origin,
            size: size(px(1.), px(height.max(1) as f32)),
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
