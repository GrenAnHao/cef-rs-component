//! Thin GPUI layout-slot adapter for one opt-in Win32 Alloy child `HWND` browser.
//!
//! Unlike [`crate::CefBrowser`] (Offscreen compositor surface), [`NativeCefBrowser`]
//! does not allocate OSR frames or GPUI `RenderImage`s: CEF renders directly into a
//! Win32 child `HWND` parented to the host GPUI window via `SetAsChild`.
//!
//! Because Win32 child `HWND`s participate in native GDI/DirectComposition window
//! clipping ("Win32 airspace"), GPUI elements cannot overlay on top of the child
//! `HWND` area; hosts must dock toolbars/menus/bars adjacent to the slot and call
//! [`NativeCefBrowser::set_visible`] when switching tabs.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::{
    cell::{Cell, RefCell},
    num::NonZeroUsize,
    rc::Rc,
};

use gpui::{
    AnyWindowHandle, App, AppContext as _, Context, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, Styled as _, Subscription,
    Window, canvas, div,
};
use raw_window_handle::RawWindowHandle;
use rust_cef_core::{
    contracts::{
        BrowserCursor, BrowserId, BrowserSurfaceMode, NavigationSnapshot, PixelRect, ShortcutId,
    },
    js_bridge::{EvalOutcome, EvalRequestId, JsFrameScope},
    ports::{BrowserCommand, BrowserEvent, BrowserHandle, CloseMode, CommandId},
};
use rust_cef_ui::native_slot::{NativeSlotController, NativeSlotError};

use crate::{
    BrowserAttachment,
    view::{ViewError, ViewUxState, bounds_dip, checked_scale, window_identity},
};

#[allow(clashing_extern_declarations)]
#[link(name = "user32")]
unsafe extern "system" {
    fn IsIconic(hwnd: isize) -> i32;
}

/// Extract the Win32 `HWND` for a GPUI `Window`, if backed by Win32.
pub fn window_hwnd(window: &Window) -> Option<NonZeroUsize> {
    let raw = raw_window_handle::HasWindowHandle::window_handle(window).ok()?;
    match raw.as_raw() {
        RawWindowHandle::Win32(handle) => NonZeroUsize::new(handle.hwnd.get() as usize),
        _ => None,
    }
}

/// Compute physical-pixel `PixelRect` for a GPUI layout slot (`origin * scale_factor`).
pub fn compute_slot_pixel_rect(
    origin_x: f32,
    origin_y: f32,
    width: f32,
    height: f32,
    scale_factor: f32,
) -> Result<PixelRect, ViewError> {
    let scale = checked_scale(scale_factor)?;
    if ![origin_x, origin_y, width, height]
        .into_iter()
        .all(f32::is_finite)
    {
        return Err(ViewError::InvalidGeometry);
    }
    let px_x = (origin_x * scale).round();
    let px_y = (origin_y * scale).round();
    let px_w = (width.max(0.0) * scale).round();
    let px_h = (height.max(0.0) * scale).round();
    if !px_x.is_finite()
        || !px_y.is_finite()
        || !px_w.is_finite()
        || !px_h.is_finite()
        || px_x < i32::MIN as f32
        || px_x > i32::MAX as f32
        || px_y < i32::MIN as f32
        || px_y > i32::MAX as f32
        || px_w < 0.0
        || px_w > u32::MAX as f32
        || px_h < 0.0
        || px_h > u32::MAX as f32
    {
        return Err(ViewError::InvalidGeometry);
    }
    let rect = PixelRect {
        x: px_x as i32,
        y: px_y as i32,
        width: px_w as u32,
        height: px_h as u32,
    };
    rect.validate().map_err(|_| ViewError::InvalidGeometry)?;
    Ok(rect)
}

fn is_window_minimized(hwnd: NonZeroUsize) -> bool {
    // SAFETY: Win32 `IsIconic` safely probes any handle value without mutating window state.
    unsafe { IsIconic(hwnd.get() as isize) != 0 }
}

fn map_slot_error(err: NativeSlotError) -> ViewError {
    match err {
        NativeSlotError::BrowserMismatch { expected, actual } => {
            ViewError::BrowserMismatch { expected, actual }
        }
        NativeSlotError::NotNativeChildHwnd => ViewError::SurfaceModeMismatch,
        NativeSlotError::ParentHwndMismatch { expected, actual } => {
            ViewError::NativeWindowMismatch {
                expected,
                actual: Some(actual),
            }
        }
        NativeSlotError::Retired => ViewError::RetiredBinding,
        NativeSlotError::InvalidRect => ViewError::InvalidGeometry,
    }
}

pub type NativeTakeFocusCallback =
    Box<dyn FnMut(bool, &mut NativeCefBrowser, &mut Window, &mut Context<NativeCefBrowser>)>;
pub type NativeShortcutCallback =
    Box<dyn FnMut(ShortcutId, &mut NativeCefBrowser, &mut Window, &mut Context<NativeCefBrowser>)>;
pub type NativeJsMessageCallback = Box<
    dyn FnMut(String, String, &mut NativeCefBrowser, &mut Window, &mut Context<NativeCefBrowser>),
>;
pub type NativeEvalCompletedCallback = Box<
    dyn FnMut(
        EvalRequestId,
        EvalOutcome,
        &mut NativeCefBrowser,
        &mut Window,
        &mut Context<NativeCefBrowser>,
    ),
>;

#[derive(Clone, Debug, PartialEq)]
pub struct NativeViewDiagnostics {
    pub parent_hwnd: NonZeroUsize,
    pub last_bounds: Option<PixelRect>,
    pub visible: bool,
    pub window_minimized: bool,
    pub bounds_commands_sent: u64,
    pub visibility_commands_sent: u64,
    pub move_notifications_sent: u64,
    pub focus_commands_sent: u64,
    pub retired: bool,
    pub last_error: Option<String>,
    pub scale_factor: f32,
}

/// Thin GPUI layout-slot entity for a `BrowserSurfaceMode::NativeChildHwnd` Browser.
pub struct NativeCefBrowser {
    browser: BrowserId,
    handle: BrowserHandle,
    parent_hwnd: NonZeroUsize,
    surface_mode: BrowserSurfaceMode,
    window_handle: AnyWindowHandle,
    focus: FocusHandle,
    slot: Rc<RefCell<NativeSlotController>>,
    host_visible: Rc<Cell<bool>>,
    minimized: Rc<Cell<bool>>,
    scale_factor: Rc<Cell<f32>>,
    last_error: Rc<RefCell<Option<String>>>,
    ux: ViewUxState,
    subscriptions: [Option<Subscription>; 4],
    on_take_focus: Option<NativeTakeFocusCallback>,
    on_shortcut: Option<NativeShortcutCallback>,
    on_js_message: Option<NativeJsMessageCallback>,
    on_eval_completed: Option<NativeEvalCompletedCallback>,
}

impl NativeCefBrowser {
    /// Bind a `NativeChildHwnd` attachment to the owning GPUI window.
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
        let surface_mode = attachment.surface_mode();
        let BrowserSurfaceMode::NativeChildHwnd(cfg) = surface_mode else {
            return Err(ViewError::SurfaceModeMismatch);
        };
        let actual_hwnd = window_hwnd(window);
        if actual_hwnd != Some(cfg.parent_hwnd) {
            return Err(ViewError::NativeWindowMismatch {
                expected: cfg.parent_hwnd,
                actual: actual_hwnd,
            });
        }
        let scale_factor = checked_scale(window.scale_factor())?;
        let window_handle = window.window_handle();
        let window_id = window_handle.window_id().as_u64();
        let handle = attachment.handle().clone();
        let slot_controller =
            NativeSlotController::from_attachment(attachment, cfg.parent_hwnd, window_id)
                .map_err(map_slot_error)?;
        let close_token = slot_controller.token().clone();
        let close_handle = handle.clone();
        let slot = Rc::new(RefCell::new(slot_controller));
        let host_visible = Rc::new(Cell::new(true));
        let minimized = Rc::new(Cell::new(is_window_minimized(cfg.parent_hwnd)));
        let scale_cell = Rc::new(Cell::new(scale_factor));
        let last_error = Rc::new(RefCell::new(None));

        let view = cx.new(|cx| Self {
            browser,
            handle,
            parent_hwnd: cfg.parent_hwnd,
            surface_mode,
            window_handle,
            focus: cx.focus_handle(),
            slot,
            host_visible,
            minimized,
            scale_factor: scale_cell,
            last_error,
            ux: ViewUxState::default(),
            subscriptions: [None, None, None, None],
            on_take_focus: None,
            on_shortcut: None,
            on_js_message: None,
            on_eval_completed: None,
        });

        let release_subscription = cx.observe_release(&view, |view, _app| {
            view.retire();
        });
        let close_view = view.downgrade();
        let close_subscription = cx.on_window_closed(move |app, closed_window| {
            if closed_window.as_u64() != window_id {
                return;
            }
            if close_view.update(app, |view, _| view.retire()).is_err() {
                close_token.retire();
                let _ = close_handle.submit(BrowserCommand::SetVisible(false));
                close_handle.request_close(CloseMode::Graceful);
            }
        });

        view.update(cx, move |view, cx| {
            let activation = cx.observe_window_activation(window, |view, _window, cx| {
                let minimized = is_window_minimized(view.parent_hwnd);
                view.minimized.set(minimized);
                view.reconcile_visibility();
                cx.notify();
            });
            let bounds = cx.observe_window_bounds(window, |view, window, cx| {
                if let Ok(scale) = checked_scale(window.scale_factor()) {
                    view.scale_factor.set(scale);
                }
                let minimized = is_window_minimized(view.parent_hwnd);
                view.minimized.set(minimized);
                view.reconcile_visibility();
                view.notify_parent_moved();
                cx.notify();
            });
            view.subscriptions = [
                Some(release_subscription),
                Some(close_subscription),
                Some(activation),
                Some(bounds),
            ];
        });

        Ok(view)
    }

    pub fn browser(&self) -> BrowserId {
        self.browser
    }

    pub fn handle(&self) -> &BrowserHandle {
        &self.handle
    }

    pub fn parent_hwnd(&self) -> NonZeroUsize {
        self.parent_hwnd
    }

    pub fn surface_mode(&self) -> BrowserSurfaceMode {
        self.surface_mode
    }

    fn check_window(&self, window: &Window) -> Result<(), ViewError> {
        let actual = window_identity(window);
        let expected = self.window_handle.window_id().as_u64();
        if actual != expected {
            return Err(ViewError::WrongWindow { expected, actual });
        }
        Ok(())
    }

    fn check_live(&self) -> Result<(), ViewError> {
        if self.slot.borrow().is_live() {
            Ok(())
        } else {
            Err(ViewError::RetiredBinding)
        }
    }

    fn reconcile_visibility(&self) {
        let visible = self.host_visible.get() && !self.minimized.get();
        let mut slot = self.slot.borrow_mut();
        if !slot.is_live() {
            return;
        }
        if let Err(err) = slot.set_visible(visible) {
            *self.last_error.borrow_mut() = Some(err.to_string());
        }
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        self.host_visible.set(visible);
        let minimized = is_window_minimized(self.parent_hwnd);
        self.minimized.set(minimized);
        self.reconcile_visibility();
        cx.notify();
    }

    pub fn set_focus(&mut self, focused: bool) {
        let mut slot = self.slot.borrow_mut();
        if !slot.is_live() {
            return;
        }
        if let Err(err) = slot.set_focus(focused) {
            *self.last_error.borrow_mut() = Some(err.to_string());
        }
    }

    pub fn notify_parent_moved(&mut self) {
        let mut slot = self.slot.borrow_mut();
        if !slot.is_live() {
            return;
        }
        if let Err(err) = slot.notify_parent_moved() {
            *self.last_error.borrow_mut() = Some(err.to_string());
        }
    }

    /// Synchronize slot geometry/visibility and drain owned UX/JS events for the bound window.
    pub fn consume(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), ViewError> {
        self.check_window(window)?;
        self.check_live()?;
        if let Ok(scale) = checked_scale(window.scale_factor()) {
            self.scale_factor.set(scale);
        }
        let minimized = is_window_minimized(self.parent_hwnd);
        self.minimized.set(minimized);
        {
            let mut slot = self.slot.borrow_mut();
            if let Err(err) = slot.set_visible(self.host_visible.get() && !minimized) {
                *self.last_error.borrow_mut() = Some(err.to_string());
            } else if let Err(err) = slot.sync() {
                *self.last_error.borrow_mut() = Some(err.to_string());
            }
        }
        self.sync_ux(window, cx);
        self.sync_js(window, cx);
        cx.notify();
        Ok(())
    }

    fn sync_ux(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let nav = self.handle.snapshot().navigation;
        self.ux.apply_snapshot(&nav);
        let events = self.handle.take_ux_events();
        let mut focus_taken = Vec::new();
        let mut shortcuts = Vec::new();
        for event in events {
            let (focus, shortcut) = self.ux.apply_event(event);
            if let Some(next) = focus {
                focus_taken.push(next);
            }
            if let Some(id) = shortcut {
                shortcuts.push(id);
            }
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
    }

    fn sync_js(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let events = self.handle.take_js_events();
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

    pub fn retire(&mut self) {
        self.slot.borrow_mut().retire();
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

    pub fn zoom_level(&self) -> f64 {
        f64::from(self.ux.zoom_level_milli) / 1000.0
    }

    pub fn audio_muted(&self) -> bool {
        self.ux.audio_muted
    }

    pub fn navigation_snapshot(&self) -> NavigationSnapshot {
        self.handle.snapshot().navigation
    }

    pub fn set_on_take_focus(&mut self, handler: Option<NativeTakeFocusCallback>) {
        self.on_take_focus = handler;
    }

    pub fn set_on_shortcut(&mut self, handler: Option<NativeShortcutCallback>) {
        self.on_shortcut = handler;
    }

    pub fn set_on_js_message(&mut self, handler: Option<NativeJsMessageCallback>) {
        self.on_js_message = handler;
    }

    pub fn set_on_eval_completed(&mut self, handler: Option<NativeEvalCompletedCallback>) {
        self.on_eval_completed = handler;
    }

    pub fn set_zoom_level(&mut self, level: f64) -> Result<CommandId, ViewError> {
        self.check_live()?;
        self.handle
            .submit(BrowserCommand::SetZoomLevel(level))
            .map_err(ViewError::Input)
    }

    pub fn reset_zoom(&mut self) -> Result<CommandId, ViewError> {
        self.set_zoom_level(0.0)
    }

    pub fn set_audio_muted(&mut self, muted: bool) -> Result<CommandId, ViewError> {
        self.check_live()?;
        self.handle
            .submit(BrowserCommand::SetAudioMuted(muted))
            .map_err(ViewError::Input)
    }

    pub fn execute_javascript(
        &self,
        script: impl Into<String>,
        frame_scope: JsFrameScope,
    ) -> Result<CommandId, ViewError> {
        self.check_live()?;
        self.handle
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
        self.check_live()?;
        self.handle
            .evaluate_script(script.into(), frame_scope, timeout_ms)
            .map_err(ViewError::Input)
    }

    pub fn post_js_message(&self, payload_json: impl Into<String>) -> Result<CommandId, ViewError> {
        self.check_live()?;
        self.handle
            .submit(BrowserCommand::PostJsMessage {
                payload_json: payload_json.into(),
            })
            .map_err(ViewError::Input)
    }

    pub fn diagnostics(&self) -> NativeViewDiagnostics {
        let slot = self.slot.borrow().diagnostics();
        let error = self
            .last_error
            .borrow()
            .clone()
            .or_else(|| slot.last_error.map(|e| format!("{e:?}")));
        NativeViewDiagnostics {
            parent_hwnd: slot.parent_hwnd,
            last_bounds: slot.last_bounds,
            visible: slot.visible,
            window_minimized: self.minimized.get(),
            bounds_commands_sent: slot.bounds_commands_sent,
            visibility_commands_sent: slot.visibility_commands_sent,
            move_notifications_sent: slot.move_notifications_sent,
            focus_commands_sent: slot.focus_commands_sent,
            retired: slot.retired,
            last_error: error,
            scale_factor: self.scale_factor.get(),
        }
    }
}

impl Drop for NativeCefBrowser {
    fn drop(&mut self) {
        self.slot.borrow_mut().retire();
    }
}

impl Focusable for NativeCefBrowser {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for NativeCefBrowser {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let slot = self.slot.clone();
        let host_visible = self.host_visible.clone();
        let minimized_cell = self.minimized.clone();
        let scale_cell = self.scale_factor.clone();
        let last_error = self.last_error.clone();
        let parent_hwnd = self.parent_hwnd;
        let expected_window = self.window_handle.window_id().as_u64();

        div().size_full().track_focus(&self.focus).child(
            canvas(
                move |bounds, window, _app| {
                    let actual_window = window_identity(window);
                    if actual_window != expected_window {
                        return;
                    }
                    let scale = window.scale_factor();
                    if let Ok(validated) = checked_scale(scale) {
                        scale_cell.set(validated);
                    }
                    let (x, y, w, h) = bounds_dip(bounds);
                    let rect = match compute_slot_pixel_rect(x, y, w, h, scale) {
                        Ok(rect) => rect,
                        Err(err) => {
                            *last_error.borrow_mut() = Some(err.to_string());
                            return;
                        }
                    };
                    let minimized = is_window_minimized(parent_hwnd);
                    minimized_cell.set(minimized);
                    let mut slot = slot.borrow_mut();
                    if !slot.is_live() {
                        return;
                    }
                    let want_visible = host_visible.get() && !minimized;
                    if let Err(err) = slot.set_visible(want_visible) {
                        *last_error.borrow_mut() = Some(err.to_string());
                        return;
                    }
                    if let Err(err) = slot.set_bounds(rect) {
                        *last_error.borrow_mut() = Some(err.to_string());
                    }
                },
                |_bounds, _prepaint: (), _window, _app| {},
            )
            .size_full(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::contracts::MAX_NATIVE_BOUND;

    #[test]
    fn compute_slot_pixel_rect_handles_dpi_scales_and_empty_bounds() {
        assert_eq!(
            compute_slot_pixel_rect(10.0, 20.0, 800.0, 600.0, 1.0),
            Ok(PixelRect {
                x: 10,
                y: 20,
                width: 800,
                height: 600,
            })
        );
        assert_eq!(
            compute_slot_pixel_rect(10.0, 86.0, 1000.0, 600.0, 1.25),
            Ok(PixelRect {
                x: 13,
                y: 108,
                width: 1250,
                height: 750,
            })
        );
        assert_eq!(
            compute_slot_pixel_rect(0.0, 86.0, 800.0, 500.0, 1.5),
            Ok(PixelRect {
                x: 0,
                y: 129,
                width: 1200,
                height: 750,
            })
        );
        assert_eq!(
            compute_slot_pixel_rect(4.0, 8.0, 320.0, 240.0, 2.0),
            Ok(PixelRect {
                x: 8,
                y: 16,
                width: 640,
                height: 480,
            })
        );
        assert_eq!(
            compute_slot_pixel_rect(0.0, 0.0, 0.0, 0.0, 1.5),
            Ok(PixelRect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            })
        );
    }

    #[test]
    fn compute_slot_pixel_rect_rejects_invalid_and_oversized_inputs() {
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                compute_slot_pixel_rect(0.0, 0.0, 100.0, 100.0, bad),
                Err(ViewError::InvalidGeometry)
            );
        }
        assert_eq!(
            compute_slot_pixel_rect(f32::NAN, 0.0, 100.0, 100.0, 1.0),
            Err(ViewError::InvalidGeometry)
        );
        assert_eq!(
            compute_slot_pixel_rect(0.0, 0.0, (MAX_NATIVE_BOUND + 1) as f32, 100.0, 1.0),
            Err(ViewError::InvalidGeometry)
        );
    }

    #[test]
    fn view_error_formats_native_hwnd_variants() {
        assert!(
            ViewError::SurfaceModeMismatch
                .to_string()
                .contains("surface mode")
        );
        let err = ViewError::NativeWindowMismatch {
            expected: NonZeroUsize::new(0x1234).unwrap(),
            actual: Some(NonZeroUsize::new(0x5678).unwrap()),
        };
        let rendered = err.to_string();
        assert!(rendered.contains("0x1234"));
        assert!(rendered.contains("0x5678"));
    }
}
