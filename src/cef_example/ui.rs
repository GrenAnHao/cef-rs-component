//! GPUI desktop browser example UI (menu bar, multi-tab strip, address bar,
//! rendering & JS sidebar, and modal dialogs).

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    ops::Range,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
};

use gpui::{
    AnyElement, App, Bounds, Context, EntityInputHandler, FocusHandle, InteractiveElement as _,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement as _,
    Pixels, Point, Radians, Render, ScaledPixels, StatefulInteractiveElement as _, Styled as _,
    Subscription, TransformationMatrix, UTF16Selection, Window, canvas, div, hsla, point,
    prelude::FluentBuilder as _, px, rgb, size,
};
use rust_cef_gpui::{
    DefaultMenuPresenter, contracts::zoom_level_to_percent, dispatcher::UiWakeSender,
    host_capabilities::JsDialogKind, ports::UiDispatcher as _,
};

use super::{ExampleAction, ExampleView};
use super::{
    assets::{
        BASIC_SCHEME_TEST_URL, BEFORE_UNLOAD_TEST_URL, BENCHMARK_SUITE_URL,
        BINDING_TEST_SINGLE_URL, BINDING_TEST_URL, BINDING_TESTS_ASYNC_TASK_URL,
        CDM_SUPPORT_TEST_URL, CHROME_INTERNAL_URLS, CHROME_NET_INTERNAL_URLS,
        CHROME_PROCESS_INTERNAL_URLS, DEFAULT_HOME_URL, DESKTOP_UX_FIXTURES_URL,
        DRAG_DROP_CURSORS_TEST_URL, DRAGGABLE_REGION_TEST_URL, LEGACY_BINDING_TEST_URL,
        POPUP_PARENT_URL, POPUP_TEST_URL, POST_MESSAGE_TEST_URL, RESPONSE_FILTER_TEST_URL,
        TOOLTIP_TEST_URL,
    },
    dialogs::{
        render_auth_modal, render_before_unload_modal, render_certificate_modal,
        render_permission_modal,
    },
    find_bar::render_find_bar,
    model::{
        ActiveInput, ExampleAppModel, ExampleHostMode, ExampleTabModel, MenuCategory, TextBuffer,
        is_point_draggable,
    },
    text_input::{
        KeyEditOutcome, TextFieldLayoutCache, TextFieldRenderSpec, TextFieldTheme,
        apply_editor_key_down_masked, child_hwnd_owns_win32_focus,
        ensure_parent_window_win32_focus, mask_display_text, masked_to_plain_offset,
        plain_to_masked_offset, render_text_field_box,
    },
};

const WATERMARK_SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 540 88" width="540" height="88"><text x="12" y="60" font-family="Segoe UI, Arial, sans-serif" font-size="52" font-weight="700" fill="#000000">cef-rs-gpui Rocks!</text></svg>"##;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SliderKind {
    Angle,
    Opacity,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn ReleaseCapture() -> i32;
    fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
}

fn initiate_win32_window_drag(hwnd: isize) {
    const WM_NCLBUTTONDOWN: u32 = 0x00A1;
    const HTCAPTION: usize = 2;
    if hwnd != 0 {
        unsafe {
            let _ = ReleaseCapture();
            let _ = PostMessageW(hwnd, WM_NCLBUTTONDOWN, HTCAPTION, 0);
        }
    }
}

fn slider_ratio_from_x(bounds: Bounds<Pixels>, cursor_x: Pixels) -> f32 {
    let w = f32::from(bounds.size.width);
    if !w.is_finite() || w <= 1.0 {
        return 0.5;
    }
    ((f32::from(cursor_x) - f32::from(bounds.origin.x)) / w).clamp(0.0, 1.0)
}

pub(super) struct CefExampleRoot {
    pub(super) model: ExampleAppModel,
    pub(super) host_mode: ExampleHostMode,
    pub(super) backend_label: &'static str,
    pub(super) views: BTreeMap<u64, ExampleView>,
    pub(super) menu_presenters: BTreeMap<u64, Rc<RefCell<DefaultMenuPresenter>>>,
    pub(super) actions: Rc<RefCell<Vec<ExampleAction>>>,
    pub(super) wake: Arc<UiWakeSender>,
    pub(super) focus: FocusHandle,
    pub(super) input_focus: FocusHandle,
    pub(super) input_caches: [Rc<RefCell<TextFieldLayoutCache>>; 7],
    pub(super) dragging_input: Option<(ActiveInput, Point<Pixels>, bool)>,
    #[allow(dead_code)]
    pub(super) focus_out_sub: Option<Subscription>,
    pub(super) smoke_eval_ok: bool,
    pub(super) smoke_response_filter_ok: bool,
    pub(super) smoke_benchmark_ok: bool,
    pub(super) smoke_custom_post_ok: bool,
    pub(super) active_slider: Option<SliderKind>,
    pub(super) angle_track_bounds: Rc<Cell<Bounds<Pixels>>>,
    pub(super) opacity_track_bounds: Rc<Cell<Bounds<Pixels>>>,
    pub(super) selected_cert_index: Option<usize>,
}

impl CefExampleRoot {
    pub(super) fn emit(&self, action: ExampleAction) {
        self.actions.borrow_mut().push(action);
        let _ = self.wake.wake();
    }

    pub(super) fn sync_native_tab_visibility(&self, cx: &mut App) {
        if self.host_mode != ExampleHostMode::NativeHwnd {
            return;
        }
        let active = self.model.active_tab;
        let overlay_blocks_hwnd = self.model.has_blocking_modal();
        for (id, view) in &self.views {
            view.set_visible(!overlay_blocks_hwnd && active == Some(*id), cx);
        }
    }

    fn input_cache_index(kind: ActiveInput) -> Option<usize> {
        match kind {
            ActiveInput::Address => Some(0),
            ActiveInput::ExecuteJs => Some(1),
            ActiveInput::EvaluateJs => Some(2),
            ActiveInput::JsPrompt => Some(3),
            ActiveInput::FindQuery => Some(4),
            ActiveInput::AuthUser => Some(5),
            ActiveInput::AuthPass => Some(6),
            ActiveInput::None => None,
        }
    }

    fn is_multiline_input(kind: ActiveInput) -> bool {
        matches!(kind, ActiveInput::ExecuteJs | ActiveInput::EvaluateJs)
    }

    fn buffer_for(&self, kind: ActiveInput) -> Option<&TextBuffer> {
        match kind {
            ActiveInput::None => None,
            ActiveInput::Address => self.model.active_tab().map(|t| &t.address_input),
            ActiveInput::ExecuteJs => self.model.active_tab().map(|t| &t.execute_js_input),
            ActiveInput::EvaluateJs => self.model.active_tab().map(|t| &t.evaluate_js_input),
            ActiveInput::JsPrompt => self
                .model
                .pending_js_dialog
                .as_ref()
                .map(|d| &d.prompt_input),
            ActiveInput::FindQuery => self.model.active_tab().map(|t| &t.find.query),
            ActiveInput::AuthUser => self.model.pending_auth.front().map(|d| &d.username_input),
            ActiveInput::AuthPass => self.model.pending_auth.front().map(|d| &d.password_input),
        }
    }

    fn buffer_for_mut(&mut self, kind: ActiveInput) -> Option<&mut TextBuffer> {
        match kind {
            ActiveInput::None => None,
            ActiveInput::Address => self.model.active_tab_mut().map(|t| &mut t.address_input),
            ActiveInput::ExecuteJs => self.model.active_tab_mut().map(|t| &mut t.execute_js_input),
            ActiveInput::EvaluateJs => self
                .model
                .active_tab_mut()
                .map(|t| &mut t.evaluate_js_input),
            ActiveInput::JsPrompt => self
                .model
                .pending_js_dialog
                .as_mut()
                .map(|d| &mut d.prompt_input),
            ActiveInput::FindQuery => self.model.active_tab_mut().map(|t| &mut t.find.query),
            ActiveInput::AuthUser => self
                .model
                .pending_auth
                .front_mut()
                .map(|d| &mut d.username_input),
            ActiveInput::AuthPass => self
                .model
                .pending_auth
                .front_mut()
                .map(|d| &mut d.password_input),
        }
    }

    fn active_buffer(&self) -> Option<&TextBuffer> {
        self.buffer_for(self.model.active_input)
    }

    fn active_buffer_mut(&mut self) -> Option<&mut TextBuffer> {
        self.buffer_for_mut(self.model.active_input)
    }

    pub(super) fn blur_active_input(&mut self, cx: &mut Context<Self>) {
        self.dragging_input = None;
        let kind = self.model.active_input;
        if kind != ActiveInput::None {
            if kind == ActiveInput::Address {
                self.input_caches[0].borrow_mut().reset_scroll();
            }
            if let Some(buf) = self.active_buffer_mut() {
                buf.unmark();
                if kind == ActiveInput::Address {
                    buf.scroll_x = 0.0;
                }
            }
            self.model.active_input = ActiveInput::None;
            cx.notify();
        }
    }

    fn focus_browser_or_root(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self
            .model
            .active_tab
            .and_then(|id| self.views.get(&id))
            .cloned()
        {
            let handle = view.focus_handle(cx);
            window.focus(&handle, cx);
        } else {
            window.focus(&self.focus, cx);
        }
    }

    fn hit_test_buffer_index(
        kind: ActiveInput,
        cache: &TextFieldLayoutCache,
        plain_text: &str,
        position: Point<Pixels>,
        multiline: bool,
    ) -> usize {
        if kind == ActiveInput::AuthPass {
            let masked = mask_display_text(plain_text);
            let raw = cache.index_for_point(&masked, position, multiline);
            masked_to_plain_offset(plain_text, raw)
        } else {
            cache.index_for_point(plain_text, position, multiline)
        }
    }

    fn focus_modal_input(
        &mut self,
        kind: ActiveInput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dragging_input = None;
        if self.model.active_input != kind
            && let Some(prev) = self.active_buffer_mut()
        {
            prev.unmark();
        }
        self.model.active_input = kind;
        let handle = self.input_focus.clone();
        window.focus(&handle, cx);
        window.request_virtual_keyboard();
        if let Some(buf) = self.buffer_for_mut(kind) {
            buf.focus_select_all();
        }
        window.notify_text_selection_changed(&handle);
        cx.notify();
    }

    fn focus_input_on_mouse_down(
        &mut self,
        kind: ActiveInput,
        select_all_on_first: bool,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        ensure_parent_window_win32_focus(window);
        if self.model.open_menu.is_some() {
            self.model.open_menu = None;
            self.sync_native_tab_visibility(cx);
        }
        let was_active = self.model.active_input == kind && self.input_focus.is_focused(window);
        if self.model.active_input != kind
            && let Some(prev_buf) = self.active_buffer_mut()
        {
            prev_buf.unmark();
        }
        self.model.active_input = kind;
        let handle = self.input_focus.clone();
        window.focus(&handle, cx);
        window.request_virtual_keyboard();

        let multiline = Self::is_multiline_input(kind);
        let Some(cache) = Self::input_cache_index(kind).map(|i| self.input_caches[i].clone())
        else {
            cx.notify();
            return;
        };
        if let Some(buf) = self.buffer_for_mut(kind) {
            let cache_ref = cache.borrow();
            buf.scroll_x = cache_ref.scroll_x;
            buf.scroll_y = cache_ref.scroll_y;
            let idx =
                Self::hit_test_buffer_index(kind, &cache_ref, &buf.text, event.position, multiline);
            drop(cache_ref);

            if event.click_count >= 3 {
                buf.select_line_at(idx, multiline);
                self.dragging_input = None;
            } else if event.click_count == 2 {
                buf.select_word_at(idx);
                self.dragging_input = None;
            } else if !was_active && select_all_on_first && !buf.text.is_empty() {
                buf.focus_select_all();
                self.dragging_input = Some((kind, event.position, true));
            } else if event.modifiers.shift {
                buf.select_to(idx);
                self.dragging_input = Some((kind, event.position, false));
            } else {
                buf.move_to(idx);
                self.dragging_input = Some((kind, event.position, false));
            }
        }
        window.notify_text_selection_changed(&handle);
        cx.notify();
    }

    fn handle_input_drag_move(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((kind, down_pos, pending_select_all)) = self.dragging_input else {
            return;
        };
        let Some(cache) = Self::input_cache_index(kind).map(|i| self.input_caches[i].clone())
        else {
            return;
        };
        let multiline = Self::is_multiline_input(kind);
        if pending_select_all {
            let moved_far = (f32::from(position.x) - f32::from(down_pos.x)).abs() > 3.0
                || (f32::from(position.y) - f32::from(down_pos.y)).abs() > 3.0;
            if !moved_far {
                return;
            }
            if let Some(buf) = self.buffer_for_mut(kind) {
                let start_idx = Self::hit_test_buffer_index(
                    kind,
                    &cache.borrow(),
                    &buf.text,
                    down_pos,
                    multiline,
                );
                buf.move_to(start_idx);
            }
            self.dragging_input = Some((kind, down_pos, false));
        }
        if let Some(buf) = self.buffer_for_mut(kind) {
            let idx =
                Self::hit_test_buffer_index(kind, &cache.borrow(), &buf.text, position, multiline);
            buf.select_to(idx);
            buf.scroll_x = cache.borrow().scroll_x;
            buf.scroll_y = cache.borrow().scroll_y;
        }
        window.notify_text_selection_changed(&self.input_focus);
        cx.notify();
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mods = &event.keystroke.modifiers;
        let key = event.keystroke.key.as_str();
        let has_blocking_modal = self.model.has_blocking_modal();

        if mods.control && !mods.alt && !mods.platform && !has_blocking_modal {
            if key.eq_ignore_ascii_case("t") {
                self.emit(ExampleAction::NewTab(None));
                cx.stop_propagation();
                return;
            }
            if key.eq_ignore_ascii_case("w") {
                if let Some(id) = self.model.active_tab {
                    self.emit(ExampleAction::CloseTab(id));
                }
                cx.stop_propagation();
                return;
            }
            if key.eq_ignore_ascii_case("f") {
                self.emit(ExampleAction::OpenFindBar);
                cx.stop_propagation();
                return;
            }
            if key.eq_ignore_ascii_case("u") {
                self.emit(ExampleAction::ViewSource);
                cx.stop_propagation();
                return;
            }
        }
        if !mods.control
            && !mods.alt
            && !mods.platform
            && !has_blocking_modal
            && key.eq_ignore_ascii_case("f3")
        {
            self.emit(ExampleAction::FindDispatch {
                forward: !mods.shift,
                force_find_next: true,
            });
            cx.stop_propagation();
            return;
        }

        if self.model.active_input != ActiveInput::None && !self.input_focus.is_focused(window) {
            self.blur_active_input(cx);
        }

        if self.model.active_input == ActiveInput::None {
            if self.model.notice_dialog.is_some() && (key == "escape" || key == "enter") {
                self.emit(ExampleAction::DismissNoticeDialog);
                cx.stop_propagation();
                return;
            }
            if key == "escape" {
                if self.model.pending_before_unload.front().is_some() {
                    self.emit(ExampleAction::ResolveBeforeUnload(false));
                    cx.stop_propagation();
                    return;
                }
                if self.model.pending_permission.front().is_some() {
                    self.emit(ExampleAction::ResolvePermission { allow: false });
                    cx.stop_propagation();
                    return;
                }
                if self.model.pending_auth.front().is_some() {
                    self.emit(ExampleAction::ResolveAuth { submit: false });
                    cx.stop_propagation();
                    return;
                }
                if self.model.pending_cert.front().is_some() {
                    self.emit(ExampleAction::ResolveCertificate { accept: false });
                    cx.stop_propagation();
                    return;
                }
                if self.model.open_menu.is_some() {
                    self.model.open_menu = None;
                    self.sync_native_tab_visibility(cx);
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                if !has_blocking_modal && self.model.active_tab().is_some_and(|t| t.find.open) {
                    self.emit(ExampleAction::CloseFindBar);
                    cx.stop_propagation();
                }
            }
            return;
        }

        let input_kind = self.model.active_input;
        match key {
            "escape" => {
                self.dragging_input = None;
                self.model.active_input = ActiveInput::None;
                if input_kind == ActiveInput::Address {
                    self.input_caches[0].borrow_mut().reset_scroll();
                    if let Some(tab) = self.model.active_tab_mut() {
                        let addr = tab.address.clone();
                        tab.address_input.set_from_external(&addr);
                    }
                    self.focus_browser_or_root(window, cx);
                } else if input_kind == ActiveInput::FindQuery {
                    if let Some(idx) = Self::input_cache_index(ActiveInput::FindQuery) {
                        self.input_caches[idx].borrow_mut().reset_scroll();
                    }
                    self.focus_browser_or_root(window, cx);
                    self.emit(ExampleAction::CloseFindBar);
                } else if matches!(input_kind, ActiveInput::AuthUser | ActiveInput::AuthPass) {
                    self.focus_browser_or_root(window, cx);
                    self.emit(ExampleAction::ResolveAuth { submit: false });
                } else {
                    if let Some(buf) = self.buffer_for_mut(input_kind) {
                        buf.unmark();
                    }
                    self.focus_browser_or_root(window, cx);
                }
                cx.notify();
                cx.stop_propagation();
            }
            "tab" if matches!(input_kind, ActiveInput::AuthUser | ActiveInput::AuthPass) => {
                let next_kind = if input_kind == ActiveInput::AuthUser {
                    ActiveInput::AuthPass
                } else {
                    ActiveInput::AuthUser
                };
                self.focus_modal_input(next_kind, window, cx);
                cx.stop_propagation();
            }
            "enter" => {
                match input_kind {
                    ActiveInput::Address => {
                        if let Some(tab) = self.model.active_tab() {
                            let url = tab.address_input.text.clone();
                            self.dragging_input = None;
                            self.model.active_input = ActiveInput::None;
                            self.focus_browser_or_root(window, cx);
                            self.emit(ExampleAction::NavigateActive(url));
                            cx.notify();
                        }
                    }
                    ActiveInput::ExecuteJs => {
                        if mods.control {
                            if let Some(tab) = self.model.active_tab() {
                                self.emit(ExampleAction::ExecuteJs(
                                    tab.execute_js_input.text.clone(),
                                ));
                            }
                        } else {
                            let handle = self.input_focus.clone();
                            if let Some(buf) = self.active_buffer_mut() {
                                buf.insert_str_filtered("\n", true);
                            }
                            window.notify_text_input_changed(&handle);
                            cx.notify();
                        }
                    }
                    ActiveInput::EvaluateJs => {
                        if mods.shift {
                            let handle = self.input_focus.clone();
                            if let Some(buf) = self.active_buffer_mut() {
                                buf.insert_str_filtered("\n", true);
                            }
                            window.notify_text_input_changed(&handle);
                            cx.notify();
                        } else if let Some(tab) = self.model.active_tab() {
                            self.emit(ExampleAction::EvaluateJs(
                                tab.evaluate_js_input.text.clone(),
                            ));
                        }
                    }
                    ActiveInput::JsPrompt => {
                        self.dragging_input = None;
                        self.model.active_input = ActiveInput::None;
                        self.focus_browser_or_root(window, cx);
                        self.emit(ExampleAction::ResolveJsDialog { accept: true });
                        cx.notify();
                    }
                    ActiveInput::FindQuery => {
                        self.emit(ExampleAction::FindDispatch {
                            forward: !mods.shift,
                            force_find_next: true,
                        });
                    }
                    ActiveInput::AuthUser => {
                        self.focus_modal_input(ActiveInput::AuthPass, window, cx);
                    }
                    ActiveInput::AuthPass => {
                        self.dragging_input = None;
                        self.model.active_input = ActiveInput::None;
                        self.focus_browser_or_root(window, cx);
                        self.emit(ExampleAction::ResolveAuth { submit: true });
                        cx.notify();
                    }
                    ActiveInput::None => {}
                }
                cx.stop_propagation();
            }
            _ => {
                let Some(cache) =
                    Self::input_cache_index(input_kind).map(|i| self.input_caches[i].clone())
                else {
                    return;
                };
                let multiline = Self::is_multiline_input(input_kind);
                let masked = matches!(input_kind, ActiveInput::AuthPass);
                let cache_ref = cache.borrow();
                let outcome =
                    self.buffer_for_mut(input_kind)
                        .map_or(KeyEditOutcome::NotHandled, |buf| {
                            buf.scroll_x = cache_ref.scroll_x;
                            buf.scroll_y = cache_ref.scroll_y;
                            apply_editor_key_down_masked(
                                buf, &cache_ref, event, multiline, masked, cx,
                            )
                        });
                drop(cache_ref);
                match outcome {
                    KeyEditOutcome::HandledChanged => {
                        window.notify_text_input_changed(&self.input_focus);
                        if input_kind == ActiveInput::FindQuery {
                            self.emit(ExampleAction::FindDispatch {
                                forward: true,
                                force_find_next: false,
                            });
                        }
                        cx.notify();
                        cx.stop_propagation();
                    }
                    KeyEditOutcome::HandledNoChange => {
                        window.notify_text_selection_changed(&self.input_focus);
                        cx.notify();
                        cx.stop_propagation();
                    }
                    KeyEditOutcome::NotHandled => {
                        if mods.control || mods.alt || mods.platform {
                            cx.stop_propagation();
                        }
                    }
                }
            }
        }
    }

    fn purple_btn(
        id: impl Into<gpui::ElementId>,
        label: impl Into<gpui::SharedString>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(id)
            .px(px(12.0))
            .py(px(5.0))
            .bg(rgb(0x5e35b1))
            .hover(|s| s.bg(rgb(0x512da8)))
            .active(|s| s.bg(rgb(0x4527a0)))
            .rounded(px(3.0))
            .text_color(rgb(0xffffff))
            .text_size(px(12.0))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    on_click(this, window, cx);
                }),
            )
            .child(label.into())
    }

    fn tool_btn(
        id: impl Into<gpui::ElementId>,
        label: &'static str,
        enabled: bool,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(id)
            .w(px(28.0))
            .h(px(28.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.0))
            .when(enabled, |d| {
                d.cursor_pointer()
                    .text_color(rgb(0x37474f))
                    .hover(|s| s.bg(rgb(0xe0e0e0)))
                    .active(|s| s.bg(rgb(0xd5d5d5)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            on_click(this, window, cx);
                        }),
                    )
            })
            .when(!enabled, |d| d.text_color(rgb(0xbdbdbd)))
            .text_size(px(15.0))
            .child(label)
    }

    fn menu_trigger(
        &self,
        id: &'static str,
        label: &'static str,
        cat: MenuCategory,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let open = self.model.open_menu == Some(cat);
        div()
            .id(id)
            .px(px(10.0))
            .py(px(3.0))
            .bg(rgb(if open { 0xd1c4e9 } else { 0xf5f5f5 }))
            .hover(|s| s.bg(rgb(0xe1bee7)))
            .text_color(rgb(0x212121))
            .text_size(px(12.0))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.model.open_menu = if this.model.open_menu == Some(cat) {
                        None
                    } else {
                        Some(cat)
                    };
                    this.sync_native_tab_visibility(cx);
                    cx.notify();
                }),
            )
            .child(label)
    }

    fn menu_row(
        id: &'static str,
        label: &'static str,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(id)
            .w_full()
            .px(px(12.0))
            .py(px(4.0))
            .text_size(px(12.0))
            .text_color(rgb(0x212121))
            .hover(|s| s.bg(rgb(0xede7f6)))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.model.open_menu = None;
                    this.sync_native_tab_visibility(cx);
                    on_click(this, window, cx);
                    cx.notify();
                }),
            )
            .child(label)
    }

    fn render_menu_dropdown(&self, cat: MenuCategory, cx: &mut Context<Self>) -> AnyElement {
        let left_offset = match cat {
            MenuCategory::File => px(4.0),
            MenuCategory::View => px(46.0),
            MenuCategory::Tests => px(92.0),
            MenuCategory::ChromeUrls => px(142.0),
        };
        let mut card = div()
            .id("menu-dropdown-card")
            .absolute()
            .top(px(26.0))
            .left(left_offset)
            .w(px(270.0))
            .max_h(px(620.0))
            .overflow_y_scroll()
            .py(px(4.0))
            .bg(rgb(0xffffff))
            .border_1()
            .border_color(rgb(0xbdbdbd))
            .shadow_md()
            .flex()
            .flex_col();

        match cat {
            MenuCategory::File => {
                card = card
                    .child(Self::menu_row(
                        "m-new-tab",
                        "New Tab  (Ctrl+T)",
                        |this, _, _| this.emit(ExampleAction::NewTab(None)),
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-close-tab",
                        "Close Tab  (Ctrl+W)",
                        |this, _, _| {
                            if let Some(id) = this.model.active_tab {
                                this.emit(ExampleAction::CloseTab(id));
                            }
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-devtools",
                        "Open / Close DevTools",
                        |this, _, _| this.emit(ExampleAction::ToggleDevTools),
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-zoom-in",
                        "Zoom In",
                        |this, _, _| this.emit(ExampleAction::ZoomSteps(1)),
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-zoom-out",
                        "Zoom Out",
                        |this, _, _| this.emit(ExampleAction::ZoomSteps(-1)),
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-zoom-reset",
                        "Zoom Reset",
                        |this, _, _| this.emit(ExampleAction::ZoomReset),
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-mute",
                        "Toggle Audio Mute",
                        |this, _, _| this.emit(ExampleAction::ToggleAudioMute),
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-clear-auth",
                        "ClearHttpAuthCredentials",
                        |this, _, _| this.emit(ExampleAction::ClearHttpAuthCredentials),
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-exit",
                        "Exit",
                        |this, _, _| this.emit(ExampleAction::ExitApp),
                        cx,
                    ));
            }
            MenuCategory::View => {
                let save_as_label: &'static str = if self.model.ask_save_as {
                    "[x] Ask Save As (Downloads)"
                } else {
                    "[ ] Ask Save As (Downloads)"
                };
                card = card
                    .child(Self::menu_row(
                        "m-toggle-sidebar",
                        "Toggle Sidebar",
                        |this, _, _| this.emit(ExampleAction::ToggleSidebar),
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-toggle-dl",
                        "Toggle Download Info",
                        |this, _, _| this.emit(ExampleAction::ToggleDownloadInfo),
                        cx,
                    ))
                    .child(div().h(px(1.0)).mx(px(8.0)).my(px(2.0)).bg(rgb(0xe0e0e0)))
                    .child(Self::menu_row(
                        "m-find-bar",
                        "Find in Page  (Ctrl+F)",
                        |this, _, _| this.emit(ExampleAction::OpenFindBar),
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-view-source",
                        "View Source  (Ctrl+U)",
                        |this, _, _| this.emit(ExampleAction::ViewSource),
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-ask-save-as",
                        save_as_label,
                        |this, _, _| this.emit(ExampleAction::ToggleAskSaveAs),
                        cx,
                    ));
            }
            MenuCategory::Tests => {
                card = card
                    .child(Self::menu_row(
                        "m-t-binding",
                        "Binding Test",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(BINDING_TEST_URL.into())))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-binding-single",
                        "Binding Test Single",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(BINDING_TEST_SINGLE_URL.into())))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-legacy-binding",
                        "LegacyBinding Test",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTabLegacy(LEGACY_BINDING_TEST_URL.into()))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-post",
                        "Post Message Test",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(POST_MESSAGE_TEST_URL.into())))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-tooltip",
                        "Tooltip Test",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(TOOLTIP_TEST_URL.into())))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-dnd",
                        "Drag&Drop Cursors Test",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(
                                DRAG_DROP_CURSORS_TEST_URL.into(),
                            )))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-popup",
                        "Popup Test",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(POPUP_PARENT_URL.into())))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-popup-scheme",
                        "Popup Test Custom Scheme",
                        |this, _, _| this.emit(ExampleAction::NewTab(Some(POPUP_TEST_URL.into()))),
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-scheme",
                        "Basic Scheme Test",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(BASIC_SCHEME_TEST_URL.into())))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-resp-filter",
                        "Response Filter Test",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(RESPONSE_FILTER_TEST_URL.into())))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-drag-region",
                        "Draggable Region Test",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(
                                DRAGGABLE_REGION_TEST_URL.into(),
                            )))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-fish-gl",
                        "Fish Gl",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some("http://www.fishgl.com/".into())))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-ms-test-drive",
                        "Microsoft Test Drive",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(
                                "http://dev.modern.ie/testdrive/".into(),
                            )))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-drag-demo",
                        "Drag Demo",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(
                                "http://html5demos.com/drag".into(),
                            )))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-pdf",
                        "Print Current tab to PDF",
                        |this, _, _| this.emit(ExampleAction::ExportPdfPrompt),
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-custom-req",
                        "Load Custom Request",
                        |this, _, _| this.emit(ExampleAction::LoadCustomPost),
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-cdm",
                        "CDM/DRM Support Test",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(CDM_SUPPORT_TEST_URL.into())))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-async-jsb-task",
                        "Async JSB Task Tests",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(
                                BINDING_TESTS_ASYNC_TASK_URL.into(),
                            )))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-resize-hack",
                        "Resize Hack Tests",
                        |this, _, _| this.emit(ExampleAction::ResizeHackTests),
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-sw-demo",
                        "Google Service Worker Demo",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(
                                "https://googlechrome.github.io/samples/service-worker/basic/"
                                    .into(),
                            )))
                        },
                        cx,
                    ))
                    .child(div().h(px(1.0)).mx(px(8.0)).my(px(2.0)).bg(rgb(0xe0e0e0)))
                    .child(Self::menu_row(
                        "m-t-bench-local",
                        "Benchmark Self-Test Suite (Local)",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(BENCHMARK_SUITE_URL.into())))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-bench-sp3",
                        "Speedometer 3.0 (browserbench.org)",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(
                                "https://browserbench.org/Speedometer3.0/".into(),
                            )))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-bench-mm",
                        "MotionMark 1.3 (browserbench.org)",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(
                                "https://browserbench.org/MotionMark1.3/".into(),
                            )))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-bench-js2",
                        "JetStream 2.2 (browserbench.org)",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(
                                "https://browserbench.org/JetStream2.2/".into(),
                            )))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-bench-h5",
                        "HTML5test (html5test.co)",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some("https://html5test.co/".into())))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-bench-webgl",
                        "WebGL Aquarium Sample",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(
                                "https://webglsamples.org/aquarium/aquarium.html".into(),
                            )))
                        },
                        cx,
                    ))
                    .child(div().h(px(1.0)).mx(px(8.0)).my(px(2.0)).bg(rgb(0xe0e0e0)))
                    .child(Self::menu_row(
                        "m-t-ux-fixtures",
                        "Desktop UX Fixtures",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(DESKTOP_UX_FIXTURES_URL.into())))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-t-before-unload",
                        "BeforeUnload Test",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(BEFORE_UNLOAD_TEST_URL.into())))
                        },
                        cx,
                    ));
            }
            MenuCategory::ChromeUrls => {
                card = card
                    .child(Self::menu_row(
                        "m-c-urls",
                        "List Internal Urls",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(CHROME_INTERNAL_URLS.into())))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-c-net",
                        "Net Internal Urls",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(CHROME_NET_INTERNAL_URLS.into())))
                        },
                        cx,
                    ))
                    .child(Self::menu_row(
                        "m-c-proc",
                        "Process Internal Urls",
                        |this, _, _| {
                            this.emit(ExampleAction::NewTab(Some(
                                CHROME_PROCESS_INTERNAL_URLS.into(),
                            )))
                        },
                        cx,
                    ));
            }
        }
        card.into_any_element()
    }

    fn apply_slider_position(
        &mut self,
        kind: SliderKind,
        cursor_x: Pixels,
        cx: &mut Context<Self>,
    ) {
        match kind {
            SliderKind::Angle => {
                let ratio = slider_ratio_from_x(self.angle_track_bounds.get(), cursor_x);
                let deg = ((ratio - 0.5) * 360.0).clamp(-180.0, 180.0);
                if let Some(t) = self.model.active_tab_mut() {
                    t.render_angle_deg = deg;
                }
                self.emit(ExampleAction::SetRenderAngle(deg));
                cx.notify();
            }
            SliderKind::Opacity => {
                let ratio = slider_ratio_from_x(self.opacity_track_bounds.get(), cursor_x);
                let opacity = ratio.clamp(0.0, 1.0);
                if let Some(t) = self.model.active_tab_mut() {
                    t.watermark_opacity = opacity;
                }
                self.emit(ExampleAction::SetWatermarkOpacity(opacity));
                cx.notify();
            }
        }
    }

    fn render_sidebar(
        &self,
        tab: &ExampleTabModel,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let zoom_pct =
            zoom_level_to_percent(f64::from(tab.zoom_level_milli) / 1000.0).round() as i32;
        let angle_ratio = ((tab.render_angle_deg + 180.0) / 360.0).clamp(0.0, 1.0);
        let opacity_ratio = tab.watermark_opacity.clamp(0.0, 1.0);
        let scaling_next = tab.bitmap_scaling.toggled();
        let exec_focused = self.model.active_input == ActiveInput::ExecuteJs
            && self.input_focus.is_focused(window);
        let eval_focused = self.model.active_input == ActiveInput::EvaluateJs
            && self.input_focus.is_focused(window);

        let angle_bounds_cell = self.angle_track_bounds.clone();
        let opacity_bounds_cell = self.opacity_track_bounds.clone();

        let card = |title: &'static str, body: AnyElement| {
            div()
                .flex()
                .flex_col()
                .bg(rgb(0xffffff))
                .border_1()
                .border_color(rgb(0xd1c4e9))
                .rounded(px(2.0))
                .overflow_hidden()
                .child(
                    div()
                        .px(px(10.0))
                        .py(px(5.0))
                        .bg(rgb(0x5e35b1))
                        .text_color(rgb(0xffffff))
                        .text_size(px(12.0))
                        .child(title),
                )
                .child(div().p(px(8.0)).child(body))
        };

        let render_slider = |id: &'static str,
                             ratio: f32,
                             bounds_cell: Rc<Cell<Bounds<Pixels>>>,
                             kind: SliderKind| {
            let pct = (ratio.clamp(0.0, 1.0) * 100.0).clamp(0.0, 100.0);
            let thumb_offset = (ratio.clamp(0.0, 1.0) * 108.0).clamp(0.0, 108.0);
            div()
                .id(id)
                .w(px(120.0))
                .h(px(20.0))
                .relative()
                .flex()
                .items_center()
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.active_slider = Some(kind);
                        this.apply_slider_position(kind, event.position.x, cx);
                    }),
                )
                .child(
                    canvas(
                        move |bounds, _, _| {
                            bounds_cell.set(bounds);
                        },
                        |_, _, _, _| {},
                    )
                    .size_full()
                    .absolute()
                    .top_0()
                    .left_0(),
                )
                .child(
                    div()
                        .w_full()
                        .h(px(4.0))
                        .rounded(px(2.0))
                        .bg(rgb(0xd1c4e9))
                        .child(
                            div()
                                .h_full()
                                .w(gpui::relative(pct / 100.0))
                                .rounded(px(2.0))
                                .bg(rgb(0x5e35b1)),
                        ),
                )
                .child(
                    div()
                        .absolute()
                        .left(px(thumb_offset))
                        .w(px(12.0))
                        .h(px(12.0))
                        .rounded(px(6.0))
                        .bg(rgb(0x5e35b1))
                        .border_1()
                        .border_color(rgb(0xffffff)),
                )
        };

        let render_card_body = div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .text_size(px(12.0))
            .text_color(rgb(0x333333))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(6.0))
                    .child(
                        div()
                            .id("sb-angle-reset")
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    cx.stop_propagation();
                                    if let Some(t) = this.model.active_tab_mut() {
                                        t.render_angle_deg = 0.0;
                                    }
                                    this.emit(ExampleAction::SetRenderAngle(0.0));
                                    cx.notify();
                                }),
                            )
                            .child(format!("Angle: {:.0}°", tab.render_angle_deg)),
                    )
                    .child(render_slider(
                        "sb-angle-slider",
                        angle_ratio,
                        angle_bounds_cell,
                        SliderKind::Angle,
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(6.0))
                    .child(format!("Watermark Opacity: {:.0}%", opacity_ratio * 100.0))
                    .child(render_slider(
                        "sb-opacity-slider",
                        opacity_ratio,
                        opacity_bounds_cell,
                        SliderKind::Opacity,
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child("Bitmap Scaling Mode:")
                    .child(
                        div()
                            .id("sb-scaling-mode")
                            .px(px(8.0))
                            .py(px(3.0))
                            .bg(rgb(0xf5f5f5))
                            .hover(|s| s.bg(rgb(0xede7f6)))
                            .border_1()
                            .border_color(rgb(0xbdbdbd))
                            .rounded(px(2.0))
                            .cursor_pointer()
                            .text_size(px(11.0))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    if let Some(t) = this.model.active_tab_mut() {
                                        t.bitmap_scaling = scaling_next;
                                    }
                                    this.emit(ExampleAction::SetBitmapScaling(scaling_next));
                                    cx.notify();
                                }),
                            )
                            .child(format!("{} ▾", tab.bitmap_scaling.label())),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(format!("Zoom: {zoom_pct}%"))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(4.0))
                            .child(Self::tool_btn(
                                "sb-zoom-out",
                                "−",
                                true,
                                |this, _, _| this.emit(ExampleAction::ZoomSteps(-1)),
                                cx,
                            ))
                            .child(Self::tool_btn(
                                "sb-zoom-reset",
                                "1:1",
                                true,
                                |this, _, _| this.emit(ExampleAction::ZoomReset),
                                cx,
                            ))
                            .child(Self::tool_btn(
                                "sb-zoom-in",
                                "+",
                                true,
                                |this, _, _| this.emit(ExampleAction::ZoomSteps(1)),
                                cx,
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .text_size(px(11.0))
                    .text_color(rgb(0x555555))
                    .child(format!(
                        "{} | Reg:{} V8:{}",
                        self.backend_label, tab.draggable_region_count, tab.v8_context_count
                    ))
                    .child(
                        div()
                            .id("sb-devtools")
                            .px(px(7.0))
                            .py(px(2.0))
                            .bg(rgb(if tab.devtools_open {
                                0x4527a0
                            } else {
                                0xede7f6
                            }))
                            .text_color(rgb(if tab.devtools_open {
                                0xffffff
                            } else {
                                0x4527a0
                            }))
                            .rounded(px(2.0))
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.emit(ExampleAction::ToggleDevTools);
                                }),
                            )
                            .child(if tab.devtools_open {
                                "DevTools*"
                            } else {
                                "DevTools"
                            }),
                    ),
            )
            .into_any_element();

        let exec_card_body = div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(render_text_field_box(
                TextFieldRenderSpec {
                    id: "sb-exec-input",
                    host: cx.entity(),
                    focus_handle: self.input_focus.clone(),
                    buffer: &tab.execute_js_input,
                    cache: self.input_caches[1].clone(),
                    placeholder: "<click to type JS>",
                    focused: exec_focused,
                    multiline: true,
                    font_size_px: 12.0,
                    pad_x_px: 7.0,
                    pad_y_px: 6.0,
                    height_px: Some(62.0),
                    rounded_px: 3.0,
                    flex_1: false,
                    theme: TextFieldTheme::example_light(),
                },
                |this, event, window, cx| {
                    this.focus_input_on_mouse_down(
                        ActiveInput::ExecuteJs,
                        false,
                        event,
                        window,
                        cx,
                    );
                },
                cx,
            ))
            .child(div().flex().justify_center().child(Self::purple_btn(
                "sb-exec-btn",
                "Execute",
                |this, _, _| {
                    if let Some(t) = this.model.active_tab() {
                        this.emit(ExampleAction::ExecuteJs(t.execute_js_input.text.clone()));
                    }
                },
                cx,
            )))
            .into_any_element();

        let eval_card_body = div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(render_text_field_box(
                TextFieldRenderSpec {
                    id: "sb-eval-input",
                    host: cx.entity(),
                    focus_handle: self.input_focus.clone(),
                    buffer: &tab.evaluate_js_input,
                    cache: self.input_caches[2].clone(),
                    placeholder: "<click to type JS expression>",
                    focused: eval_focused,
                    multiline: true,
                    font_size_px: 12.0,
                    pad_x_px: 7.0,
                    pad_y_px: 6.0,
                    height_px: Some(46.0),
                    rounded_px: 3.0,
                    flex_1: false,
                    theme: TextFieldTheme::example_light(),
                },
                |this, event, window, cx| {
                    this.focus_input_on_mouse_down(
                        ActiveInput::EvaluateJs,
                        false,
                        event,
                        window,
                        cx,
                    );
                },
                cx,
            ))
            .child(div().flex().justify_center().child(Self::purple_btn(
                "sb-eval-btn",
                "Evaluate",
                |this, _, _| {
                    if let Some(t) = this.model.active_tab() {
                        this.emit(ExampleAction::EvaluateJs(t.evaluate_js_input.text.clone()));
                    }
                },
                cx,
            )))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(rgb(0x424242))
                    .child("Result:"),
            )
            .child(
                div()
                    .min_h(px(22.0))
                    .px(px(4.0))
                    .py(px(2.0))
                    .border_b_1()
                    .border_color(rgb(0x9e9e9e))
                    .text_size(px(12.0))
                    .text_color(rgb(0x212121))
                    .child(tab.evaluate_js_result.clone()),
            )
            .into_any_element();

        let tests_card_body = div()
            .flex()
            .justify_center()
            .child(Self::purple_btn(
                "sb-stress-btn",
                "Javascript Binding Stress Test",
                |this, _, _| this.emit(ExampleAction::RunBindingStressTest),
                cx,
            ))
            .into_any_element();

        div()
            .w(px(304.0))
            .flex_shrink_0()
            .h_full()
            .p(px(6.0))
            .gap(px(6.0))
            .flex()
            .flex_col()
            .bg(rgb(0xfafafa))
            .border_l_1()
            .border_color(rgb(0xd6d6d6))
            .overflow_hidden()
            .child(card("Tweak Rendering", render_card_body))
            .child(card("Execute Javascript (asynchronously)", exec_card_body))
            .child(card("Evaluate Javascript (Async)", eval_card_body))
            .child(card("Tests", tests_card_body))
            .into_any_element()
    }

    fn render_download_panel(&self, tab: &ExampleTabModel, cx: &mut Context<Self>) -> AnyElement {
        let dl = &tab.download;
        let tab_id = tab.id;
        let can_cancel = dl.download_id.is_some() && !dl.is_complete && !dl.is_cancelled;
        let folder_path = (!dl.full_path.is_empty()).then(|| PathBuf::from(&dl.full_path));
        let row = |label: &'static str, val: String| {
            div()
                .flex()
                .flex_row()
                .justify_between()
                .gap(px(6.0))
                .text_size(px(11.0))
                .child(div().text_color(rgb(0x616161)).child(label))
                .child(div().text_color(rgb(0x212121)).overflow_hidden().child(val))
        };
        div()
            .w(px(260.0))
            .flex_shrink_0()
            .h_full()
            .p(px(6.0))
            .bg(rgb(0xfafafa))
            .border_l_1()
            .border_color(rgb(0xd6d6d6))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .bg(rgb(0xffffff))
                    .border_1()
                    .border_color(rgb(0xd1c4e9))
                    .rounded(px(2.0))
                    .child(
                        div()
                            .px(px(10.0))
                            .py(px(5.0))
                            .bg(rgb(0x5e35b1))
                            .text_color(rgb(0xffffff))
                            .text_size(px(12.0))
                            .child("Download info"),
                    )
                    .child(
                        div()
                            .p(px(8.0))
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .child(row("Last action:", dl.last_action.clone()))
                            .child(row("IsInProgress:", dl.is_in_progress.to_string()))
                            .child(row("IsComplete:", dl.is_complete.to_string()))
                            .child(row("IsCancelled:", dl.is_cancelled.to_string()))
                            .when_some(dl.cancelled_reason.clone(), |col, reason| {
                                col.child(row("CancelReason:", reason))
                            })
                            .child(row(
                                "PercentComplete:",
                                dl.percent_complete.map_or("-".into(), |p| format!("{p}%")),
                            ))
                            .child(row("ReceivedBytes:", dl.received_bytes.to_string()))
                            .child(row(
                                "TotalBytes:",
                                dl.total_bytes.map_or("-".into(), |b| b.to_string()),
                            ))
                            .child(row("SuggestedName:", dl.suggested_file_name.clone()))
                            .child(row("FullPath:", dl.full_path.clone()))
                            .child(row("Url:", dl.url.clone()))
                            .when(can_cancel || folder_path.is_some(), |col| {
                                let mut actions_col =
                                    div().pt(px(4.0)).flex().flex_col().gap(px(4.0));
                                if can_cancel {
                                    actions_col = actions_col.child(Self::purple_btn(
                                        "dl-cancel-btn",
                                        "Cancel Download",
                                        move |this, _, _| {
                                            this.emit(ExampleAction::CancelDownload(tab_id));
                                        },
                                        cx,
                                    ));
                                }
                                if let Some(p) = folder_path.clone() {
                                    actions_col = actions_col.child(Self::purple_btn(
                                        "dl-open-folder-btn",
                                        "Open Containing Folder",
                                        move |this, _, _| {
                                            this.emit(ExampleAction::OpenContainingFolder(
                                                p.clone(),
                                            ));
                                        },
                                        cx,
                                    ));
                                }
                                col.child(actions_col)
                            }),
                    ),
            )
            .into_any_element()
    }
}

impl EntityInputHandler for CefExampleRoot {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let masked = matches!(self.model.active_input, ActiveInput::AuthPass);
        let buf = self.active_buffer_mut()?;
        let range = buf.range_from_utf16(&range_utf16);
        adjusted_range.replace(buf.range_to_utf16(&range));
        let slice = &buf.text[range];
        if masked {
            Some(super::text_input::mask_display_text(slice))
        } else {
            Some(slice.to_owned())
        }
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let buf = self.active_buffer_mut()?;
        Some(UTF16Selection {
            range: buf.range_to_utf16(&buf.selected_range),
            reversed: buf.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let buf = self.active_buffer()?;
        buf.marked_range.as_ref().map(|r| buf.range_to_utf16(r))
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.input_focus.clone();
        if let Some(buf) = self.active_buffer_mut() {
            buf.unmark();
        }
        window.notify_text_input_changed(&focus);
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let kind = self.model.active_input;
        let multiline = Self::is_multiline_input(kind);
        let focus = self.input_focus.clone();
        if let Some(buf) = self.active_buffer_mut() {
            buf.replace_in_utf16_range(range, text, multiline);
        }
        window.notify_text_input_changed(&focus);
        if kind == ActiveInput::FindQuery {
            self.emit(ExampleAction::FindDispatch {
                forward: true,
                force_find_next: false,
            });
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let kind = self.model.active_input;
        let multiline = Self::is_multiline_input(kind);
        let focus = self.input_focus.clone();
        let mut has_marked = false;
        if let Some(buf) = self.active_buffer_mut() {
            buf.replace_and_mark_in_utf16_range(range, new_text, new_selected_range, multiline);
            has_marked = buf.marked_range.is_some();
        }
        window.notify_text_input_changed(&focus);
        if kind == ActiveInput::FindQuery && !has_marked {
            self.emit(ExampleAction::FindDispatch {
                forward: true,
                force_find_next: false,
            });
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let kind = self.model.active_input;
        let idx = Self::input_cache_index(kind)?;
        let cache = self.input_caches[idx].clone();
        let buf = self.active_buffer_mut()?;
        let utf8 = buf.range_from_utf16(&range_utf16);
        let cache_range = if kind == ActiveInput::AuthPass {
            plain_to_masked_offset(&buf.text, utf8.start)
                ..plain_to_masked_offset(&buf.text, utf8.end)
        } else {
            utf8
        };
        cache.borrow().bounds_for_utf8_range(cache_range)
    }

    fn selection_bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Bounds<Pixels>> {
        self.bounds_for_range(range_utf16, bounds, window, cx)
            .into_iter()
            .collect()
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let kind = self.model.active_input;
        let idx = Self::input_cache_index(kind)?;
        let multiline = Self::is_multiline_input(kind);
        let cache = self.input_caches[idx].clone();
        let buf = self.active_buffer_mut()?;
        let utf8 = Self::hit_test_buffer_index(kind, &cache.borrow(), &buf.text, point, multiline);
        Some(buf.offset_to_utf16(utf8))
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = self.input_focus.clone();
        if let Some(buf) = self.active_buffer_mut() {
            let utf8 = buf.range_from_utf16(&range_utf16);
            buf.selected_range = utf8;
            buf.selection_reversed = false;
            buf.touch();
        }
        window.notify_text_selection_changed(&focus);
        cx.notify();
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        self.active_buffer_mut().map(|b| b.text_len_utf16())
    }
}

impl Render for CefExampleRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.host_mode == ExampleHostMode::NativeHwnd
            && self.model.active_input != ActiveInput::None
            && child_hwnd_owns_win32_focus(window)
        {
            self.dragging_input = None;
            if self.model.active_input == ActiveInput::Address {
                self.input_caches[0].borrow_mut().reset_scroll();
            }
            self.model.active_input = ActiveInput::None;
        }
        let active_id = self.model.active_tab;
        let active_tab = self.model.active_tab().cloned();
        let active_view = active_id.and_then(|id| self.views.get(&id).cloned());
        let menu = active_id
            .and_then(|id| self.menu_presenters.get(&id))
            .and_then(|presenter| presenter.borrow().element(window, cx));

        let menu_bar = div()
            .h(px(26.0))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .bg(rgb(0xf5f5f5))
            .border_b_1()
            .border_color(rgb(0xe0e0e0))
            .child(self.menu_trigger("mb-file", "File", MenuCategory::File, cx))
            .child(self.menu_trigger("mb-view", "View", MenuCategory::View, cx))
            .child(self.menu_trigger("mb-tests", "Tests", MenuCategory::Tests, cx))
            .child(self.menu_trigger("mb-chrome", "Chrome Urls", MenuCategory::ChromeUrls, cx));

        let mut tab_strip = div()
            .h(px(30.0))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_end()
            .gap(px(2.0))
            .px(px(4.0))
            .bg(rgb(0xeceff1))
            .border_b_1()
            .border_color(rgb(0xcfd8dc));

        for tab in &self.model.tabs {
            let tab_id = tab.id;
            let is_active = Some(tab_id) == active_id;
            let mut label = if tab.title.is_empty() {
                tab.address.clone()
            } else {
                tab.title.clone()
            };
            if label.chars().count() > 26 {
                label = label.chars().take(25).collect::<String>() + "…";
            }
            tab_strip = tab_strip.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .h(px(26.0))
                    .px(px(10.0))
                    .bg(rgb(if is_active { 0x81d4fa } else { 0xcfd8dc }))
                    .rounded_t(px(3.0))
                    .text_color(rgb(0x212121))
                    .text_size(px(12.0))
                    .child(
                        div()
                            .id(("ex-tab-sel", tab_id as usize))
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.emit(ExampleAction::SwitchTab(tab_id));
                                }),
                            )
                            .child(label),
                    )
                    .child(
                        div()
                            .id(("ex-tab-close", tab_id as usize))
                            .px(px(4.0))
                            .rounded(px(2.0))
                            .hover(|s| s.bg(rgb(0x4fc3f7)))
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.emit(ExampleAction::CloseTab(tab_id));
                                }),
                            )
                            .child("×"),
                    ),
            );
        }
        tab_strip = tab_strip.child(
            div()
                .id("ex-new-tab-btn")
                .px(px(8.0))
                .py(px(3.0))
                .text_size(px(13.0))
                .text_color(rgb(0x455a64))
                .hover(|s| s.bg(rgb(0xcfd8dc)))
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.emit(ExampleAction::NewTab(None));
                    }),
                )
                .child("+"),
        );

        let can_back = active_tab.as_ref().is_some_and(|t| t.can_go_back);
        let can_fwd = active_tab.as_ref().is_some_and(|t| t.can_go_forward);
        let is_loading = active_tab.as_ref().is_some_and(|t| t.is_loading);
        let addr_focused =
            self.model.active_input == ActiveInput::Address && self.input_focus.is_focused(window);
        let fallback_addr = TextBuffer::new(DEFAULT_HOME_URL);
        let addr_buf = active_tab
            .as_ref()
            .map_or(&fallback_addr, |t| &t.address_input);

        let nav_bar = div()
            .h(px(42.0))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(10.0))
            .bg(rgb(0xfafafa))
            .border_b_1()
            .border_color(rgb(0xe0e0e0))
            .child(Self::tool_btn(
                "nav-home",
                "⌂",
                true,
                |this, _, _| this.emit(ExampleAction::NavigateHome),
                cx,
            ))
            .child(Self::tool_btn(
                "nav-back",
                "‹",
                can_back,
                |this, _, _| this.emit(ExampleAction::Back),
                cx,
            ))
            .child(Self::tool_btn(
                "nav-fwd",
                "›",
                can_fwd,
                |this, _, _| this.emit(ExampleAction::Forward),
                cx,
            ))
            .child(Self::tool_btn(
                "nav-reload",
                if is_loading { "✕" } else { "↻" },
                true,
                move |this, _, _| {
                    if is_loading {
                        this.emit(ExampleAction::StopLoading);
                    } else {
                        this.emit(ExampleAction::Reload);
                    }
                },
                cx,
            ))
            .child(render_text_field_box(
                TextFieldRenderSpec {
                    id: "nav-address-box",
                    host: cx.entity(),
                    focus_handle: self.input_focus.clone(),
                    buffer: addr_buf,
                    cache: self.input_caches[0].clone(),
                    placeholder: "Enter URL...",
                    focused: addr_focused,
                    multiline: false,
                    font_size_px: 13.0,
                    pad_x_px: 10.0,
                    pad_y_px: 4.0,
                    height_px: Some(30.0),
                    rounded_px: 3.0,
                    flex_1: true,
                    theme: TextFieldTheme::example_light(),
                },
                |this, event, window, cx| {
                    this.focus_input_on_mouse_down(ActiveInput::Address, true, event, window, cx);
                },
                cx,
            ))
            .child(Self::tool_btn(
                "nav-go",
                "⇥",
                true,
                |this, window, cx| {
                    if let Some(t) = this.model.active_tab() {
                        let url = t.address_input.text.clone();
                        this.blur_active_input(cx);
                        this.focus_browser_or_root(window, cx);
                        this.emit(ExampleAction::NavigateActive(url));
                        cx.notify();
                    }
                },
                cx,
            ))
            .child(Self::tool_btn(
                "nav-sidebar",
                "☷",
                true,
                |this, _, _| this.emit(ExampleAction::ToggleSidebar),
                cx,
            ));

        let drag_root = cx.entity().downgrade();
        let browser_host = div()
            .flex_1()
            .h_full()
            .relative()
            .overflow_hidden()
            .bg(rgb(0xf0f0f0))
            .child(
                canvas(
                    |_, _, _| {},
                    |bounds, _, window, app| {
                        let scale = window.scale_factor();
                        let tile_w = 340.0_f32;
                        let tile_h = 84.0_f32;
                        let start_x = f32::from(bounds.origin.x) - tile_w * 0.5;
                        let start_y = f32::from(bounds.origin.y) - tile_h * 0.5;
                        let end_x = f32::from(bounds.origin.x)
                            + f32::from(bounds.size.width)
                            + tile_w * 0.5;
                        let end_y = f32::from(bounds.origin.y)
                            + f32::from(bounds.size.height)
                            + tile_h * 0.5;
                        let angle_rad = Radians(-22.5_f32.to_radians());
                        let color = hsla(0.0, 0.0, 0.0, 0.08);
                        let mut row = 0_i32;
                        let mut y = start_y;
                        while y < end_y {
                            let row_shift = if row % 2 == 0 { 0.0 } else { tile_w * 0.45 };
                            let mut x = start_x - row_shift;
                            while x < end_x {
                                let tile_bounds = Bounds {
                                    origin: point(px(x), px(y)),
                                    size: size(px(tile_w), px(tile_h)),
                                };
                                let cx_sp = (x + tile_w * 0.5) * scale;
                                let cy_sp = (y + tile_h * 0.5) * scale;
                                let c = point(ScaledPixels(cx_sp), ScaledPixels(cy_sp));
                                let xform = TransformationMatrix::unit()
                                    .translate(c)
                                    .rotate(angle_rad)
                                    .translate(point(ScaledPixels(-cx_sp), ScaledPixels(-cy_sp)));
                                let _ = window.paint_svg(
                                    tile_bounds,
                                    "cef-rs-gpui-rocks-watermark.svg".into(),
                                    Some(WATERMARK_SVG),
                                    xform,
                                    color,
                                    app,
                                );
                                x += tile_w;
                            }
                            row += 1;
                            y += tile_h;
                        }
                    },
                )
                .size_full()
                .absolute()
                .top_0()
                .left_0(),
            )
            .children(active_view.map(|view| {
                div()
                    .id("ex-browser-view-host")
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .capture_any_mouse_down(move |event: &MouseDownEvent, window, app| {
                        if event.button != MouseButton::Left {
                            return;
                        }
                        let drag_hit = drag_root
                            .update(app, |this, cx| {
                                let id = this.model.active_tab?;
                                let tab = this.model.tabs.iter().find(|t| t.id == id)?;
                                if tab.draggable_regions.is_empty() {
                                    return None;
                                }
                                let ExampleView::Osr(v) = this.views.get(&id)? else {
                                    return None;
                                };
                                let (vx, vy) = v.read(cx).hit_test_surface_point((
                                    f32::from(event.position.x),
                                    f32::from(event.position.y),
                                ))?;
                                if is_point_draggable(&tab.draggable_regions, vx, vy) {
                                    rust_cef_gpui::window_hwnd(window).map(|h| h.get() as isize)
                                } else {
                                    None
                                }
                            })
                            .ok()
                            .flatten();
                        if let Some(hwnd) = drag_hit {
                            app.stop_propagation();
                            initiate_win32_window_drag(hwnd);
                        }
                    })
                    .child(view.into_any_element())
            }));

        let js_dialog_bar = self.model.pending_js_dialog.as_ref().map(|dlg| {
            let is_prompt = dlg.kind == JsDialogKind::Prompt;
            let show_cancel = dlg.kind != JsDialogKind::Alert;
            let prompt_focused = self.model.active_input == ActiveInput::JsPrompt
                && self.input_focus.is_focused(window);
            div()
                .flex_shrink_0()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .px(px(12.0))
                .py(px(6.0))
                .bg(rgb(0xede7f6))
                .border_b_1()
                .border_color(rgb(0x5e35b1))
                .text_color(rgb(0x212121))
                .text_size(px(12.0))
                .child(format!(
                    "[{:?}] {}: {}",
                    dlg.kind, dlg.origin_url, dlg.message
                ))
                .when(is_prompt, |bar| {
                    bar.child(div().w(px(180.0)).child(render_text_field_box(
                        TextFieldRenderSpec {
                            id: "dlg-prompt-input",
                            host: cx.entity(),
                            focus_handle: self.input_focus.clone(),
                            buffer: &dlg.prompt_input,
                            cache: self.input_caches[3].clone(),
                            placeholder: "",
                            focused: prompt_focused,
                            multiline: false,
                            font_size_px: 12.0,
                            pad_x_px: 6.0,
                            pad_y_px: 3.0,
                            height_px: Some(26.0),
                            rounded_px: 3.0,
                            flex_1: false,
                            theme: TextFieldTheme::example_light(),
                        },
                        |this, event, window, cx| {
                            this.focus_input_on_mouse_down(
                                ActiveInput::JsPrompt,
                                true,
                                event,
                                window,
                                cx,
                            );
                        },
                        cx,
                    )))
                })
                .child(Self::purple_btn(
                    "dlg-ok",
                    "OK",
                    |this, _, _| this.emit(ExampleAction::ResolveJsDialog { accept: true }),
                    cx,
                ))
                .when(show_cancel, |bar| {
                    bar.child(Self::purple_btn(
                        "dlg-cancel",
                        "Cancel",
                        |this, _, _| this.emit(ExampleAction::ResolveJsDialog { accept: false }),
                        cx,
                    ))
                })
        });

        let mut center_col = div().flex_1().h_full().flex().flex_col().overflow_hidden();
        if let Some(tab) = active_tab.as_ref()
            && tab.find.open
        {
            let find_focused = self.model.active_input == ActiveInput::FindQuery
                && self.input_focus.is_focused(window);
            let query_cache = self.input_caches
                [Self::input_cache_index(ActiveInput::FindQuery).expect("find query cache")]
            .clone();
            center_col = center_col.child(render_find_bar(
                tab,
                find_focused,
                cx.entity(),
                self.input_focus.clone(),
                query_cache,
                |this, event, window, cx| {
                    this.focus_input_on_mouse_down(
                        ActiveInput::FindQuery,
                        false,
                        event,
                        window,
                        cx,
                    );
                },
                |this, _, _| this.emit(ExampleAction::FindToggleCase),
                |this, _, _| {
                    this.emit(ExampleAction::FindDispatch {
                        forward: false,
                        force_find_next: true,
                    })
                },
                |this, _, _| {
                    this.emit(ExampleAction::FindDispatch {
                        forward: true,
                        force_find_next: true,
                    })
                },
                |this, window, cx| {
                    if this.model.active_input == ActiveInput::FindQuery {
                        this.dragging_input = None;
                        this.model.active_input = ActiveInput::None;
                        this.focus_browser_or_root(window, cx);
                    }
                    this.emit(ExampleAction::CloseFindBar);
                },
                cx,
            ));
        }
        if let Some(dlg_bar) = js_dialog_bar {
            center_col = center_col.child(dlg_bar);
        }
        center_col = center_col.child(browser_host);

        let mut middle_row = div()
            .flex_1()
            .w_full()
            .flex()
            .flex_row()
            .overflow_hidden()
            .child(center_col);
        if let Some(tab) = active_tab.as_ref() {
            if tab.show_download_info {
                middle_row = middle_row.child(self.render_download_panel(tab, cx));
            }
            if tab.show_sidebar {
                middle_row = middle_row.child(self.render_sidebar(tab, window, cx));
            }
        }

        let prog_px = active_tab.as_ref().map_or(100.0, |t| {
            if t.is_loading {
                (f32::from(t.loading_progress_per_mille) / 1000.0 * 100.0).max(8.0)
            } else {
                100.0
            }
        });
        let status_msg = active_tab
            .as_ref()
            .map(|t| t.status_message.clone())
            .unwrap_or_default();
        let output_msg = active_tab
            .as_ref()
            .map(|t| t.output_message.clone())
            .unwrap_or_else(|| "Initializing...".into());

        let status_bar = div()
            .h(px(24.0))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(6.0))
            .bg(rgb(0xeeeeee))
            .border_t_1()
            .border_color(rgb(0xd6d6d6))
            .text_size(px(12.0))
            .text_color(rgb(0x333333))
            .child(
                div()
                    .w(px(100.0))
                    .h(px(14.0))
                    .bg(rgb(0xd1c4e9))
                    .overflow_hidden()
                    .child(div().h_full().w(px(prog_px)).bg(rgb(0x7e57c2))),
            )
            .when(!status_msg.is_empty(), |bar| {
                bar.child(div().text_color(rgb(0x4527a0)).child(status_msg))
                    .child(div().w(px(1.0)).h(px(14.0)).bg(rgb(0xbdbdbd)))
            })
            .child(div().flex_1().overflow_hidden().child(output_msg));

        let mut root = div()
            .id("cef-example-root")
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    let mut changed = false;
                    if this.model.open_menu.is_some() {
                        this.model.open_menu = None;
                        this.sync_native_tab_visibility(cx);
                        changed = true;
                    }
                    if this.model.active_input != ActiveInput::None {
                        let root_focus = this.focus.clone();
                        window.focus(&root_focus, cx);
                        this.blur_active_input(cx);
                        changed = true;
                    }
                    if changed {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                if let Some(kind) = this.active_slider {
                    this.apply_slider_position(kind, event.position.x, cx);
                }
                if this.dragging_input.is_some() {
                    this.handle_input_drag_move(event.position, window, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    let mut changed = this.active_slider.take().is_some();
                    if this.dragging_input.take().is_some() {
                        changed = true;
                    }
                    if changed {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    let mut changed = this.active_slider.take().is_some();
                    if this.dragging_input.take().is_some() {
                        changed = true;
                    }
                    if changed {
                        cx.notify();
                    }
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key_down(event, window, cx);
            }))
            .flex()
            .flex_col()
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(rgb(0xf5f5f5))
            .child(menu_bar)
            .child(tab_strip)
            .child(nav_bar)
            .child(middle_row)
            .child(status_bar);

        if let Some(cat) = self.model.open_menu {
            root = root.child(self.render_menu_dropdown(cat, cx));
        }

        if let Some(ref perm) = self.model.pending_permission.front().cloned() {
            root = root.child(render_permission_modal(
                perm,
                |this, _, _| this.emit(ExampleAction::ResolvePermission { allow: true }),
                |this, _, _| this.emit(ExampleAction::ResolvePermission { allow: false }),
                cx,
            ));
        } else if let Some(ref auth) = self.model.pending_auth.front().cloned() {
            let user_focused = self.model.active_input == ActiveInput::AuthUser
                && self.input_focus.is_focused(window);
            let pass_focused = self.model.active_input == ActiveInput::AuthPass
                && self.input_focus.is_focused(window);
            let user_cache = self.input_caches
                [Self::input_cache_index(ActiveInput::AuthUser).expect("auth user cache")]
            .clone();
            let pass_cache = self.input_caches
                [Self::input_cache_index(ActiveInput::AuthPass).expect("auth pass cache")]
            .clone();
            root = root.child(render_auth_modal(
                auth,
                user_focused,
                pass_focused,
                cx.entity(),
                self.input_focus.clone(),
                user_cache,
                pass_cache,
                |this, event, window, cx| {
                    this.focus_input_on_mouse_down(ActiveInput::AuthUser, false, event, window, cx);
                },
                |this, event, window, cx| {
                    this.focus_input_on_mouse_down(ActiveInput::AuthPass, false, event, window, cx);
                },
                |this, _, _| this.emit(ExampleAction::ResolveAuth { submit: true }),
                |this, _, _| this.emit(ExampleAction::ResolveAuth { submit: false }),
                cx,
            ));
        } else if let Some(ref cert) = self.model.pending_cert.front().cloned() {
            root = root.child(render_certificate_modal(
                cert,
                self.selected_cert_index,
                |this, idx, _, cx| {
                    this.selected_cert_index = Some(idx);
                    cx.notify();
                },
                |this, _, _| this.emit(ExampleAction::ResolveCertificate { accept: true }),
                |this, _, _| this.emit(ExampleAction::ResolveCertificate { accept: false }),
                cx,
            ));
        } else if let Some(ref bu) = self.model.pending_before_unload.front().cloned() {
            root = root.child(render_before_unload_modal(
                bu,
                |this, _, _| this.emit(ExampleAction::ResolveBeforeUnload(false)),
                |this, _, _| this.emit(ExampleAction::ResolveBeforeUnload(true)),
                cx,
            ));
        }

        if let Some(ref notice) = self.model.notice_dialog {
            root = root.child(
                div()
                    .id("notice-modal-backdrop")
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .bg(hsla(0.0, 0.0, 0.0, 0.35))
                    .flex()
                    .items_center()
                    .justify_center()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                    )
                    .child(
                        div()
                            .w(px(460.0))
                            .bg(rgb(0xffffff))
                            .border_1()
                            .border_color(rgb(0xbdbdbd))
                            .rounded(px(4.0))
                            .shadow_lg()
                            .overflow_hidden()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .px(px(14.0))
                                    .py(px(8.0))
                                    .bg(rgb(0x5e35b1))
                                    .text_color(rgb(0xffffff))
                                    .text_size(px(13.0))
                                    .child(notice.title.clone()),
                            )
                            .child(
                                div()
                                    .px(px(16.0))
                                    .py(px(14.0))
                                    .text_size(px(13.0))
                                    .text_color(rgb(0x212121))
                                    .child(notice.message.clone()),
                            )
                            .child(div().px(px(14.0)).pb(px(12.0)).flex().justify_end().child(
                                Self::purple_btn(
                                    "notice-modal-ok",
                                    "OK",
                                    |this, _, _| this.emit(ExampleAction::DismissNoticeDialog),
                                    cx,
                                ),
                            )),
                    ),
            );
        }

        match menu {
            Some(m) => root.child(m),
            None => root,
        }
    }
}
