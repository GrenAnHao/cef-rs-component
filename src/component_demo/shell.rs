//! Opt-in multi-tab desktop browser shell mode for `gpui-cef-component-demo` (`--browser-shell true`).
//!
//! Uses only public `rust-cef-gpui` and `rust-cef-core` APIs (`Runtime`, `CefBrowser`,
//! `BrowserHandle`, `BrowserOptions`, `HostCapabilities`, `DefaultMenuPresenter`).
//! Preserves 100% compatibility when `--browser-shell` is omitted or `false`.

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    future::Future,
    io::Write as _,
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
    AnyElement, App, AppContext as _, AsyncApp, Bounds, Context, Entity, FocusHandle,
    Focusable as _, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton,
    ParentElement as _, Render, Styled as _, Window, WindowBounds, WindowHandle, WindowOptions,
    div, px, rgb, size,
};
use rust_cef_gpui::{
    CefBrowser, ContextMenuConfig, ContextMenuRegistration, ContextMenuServices,
    DefaultMenuPresenter, NativeCefBrowser, PdfExportStatus, PdfExportTicket, Runtime,
    RuntimeServices, ViewDiagnostics,
    contracts::{
        BrowserConfig, BrowserId, BrowserOptions, BrowserSurfaceMode, CefFeatureState,
        NativeChildHwndConfig, NavigationSnapshot, PixelRect, ProfileStorage, RuntimeConfig,
        RuntimeFiles, RuntimeSettings, RuntimeState, SessionState, ShortcutFilter, ShortcutId,
        ShortcutRule, ThreadMode, TrustedContentPolicy, Viewport, step_canonical_zoom_level,
        zoom_level_to_milli, zoom_level_to_percent,
    },
    custom_schemes::{CustomSchemeConfig, CustomSchemeOptions, MemoryAsset, SchemeAssetSource},
    dispatcher::{self, UiWakeReceiver, UiWakeSender},
    host_capabilities::{
        AuthDecision, AuthRequest, AuthService, CdpEvent, CdpMethodResult, CdpObserver, DownloadId,
        DownloadProgress, DownloadRequest, DownloadService, DownloadState, HostCapabilities,
        JsDialogDecision, JsDialogRequest, JsDialogService, PdfExportOptions, PopupRequest,
        PopupService,
    },
    js_bridge::{EvalOutcome, JsBridgeConfig, JsFrameScope, MAX_JS_PAYLOAD_BYTES},
    menu::{MenuContextLimits, MenuLimits},
    outbound_drag_contracts::OutboundDragPolicy,
    ports::{
        BrowserCommand, BrowserEvent, BrowserEventEnvelope, BrowserEventSink, BrowserHandle,
        CaptureFailureKind, CloseMode, EngineError, FrameSink, OwnedFrame, PublishResult,
        UiDispatcher as _,
    },
    runtime::{ShutdownStatus, StartError},
};

use crate::cli::{Config, HostMode};

pub(super) const MAX_SHELL_TABS: usize = 8;
const MAX_DOWNLOAD_HISTORY: usize = 8;
const MAX_INPUT_BYTES: usize = 512;
const STARTUP_BUDGET: Duration = Duration::from_secs(30);
const DRAIN_BUDGET: Duration = Duration::from_secs(15);
const PUMP_PERIOD: Duration = Duration::from_millis(4);
// Exact chrome height: tab_bar (30) + nav_bar (32) + progress_bar (2) + footer (22).
const SHELL_CHROME_HEIGHT: f32 = 86.0;

const START_URL: &str = "app://demo/index.html";
const PAGE2_URL: &str = "app://demo/page2.html";
const POPUP_URL: &str = "app://demo/popup.html";
const DOWNLOAD_URL: &str = "app://demo/sample.txt";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TabMeta {
    pub id: u64,
    pub title: String,
    pub url: String,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub is_loading: bool,
    pub loading_progress_per_mille: u16,
    pub zoom_level_milli: i32,
    pub audio_muted: bool,
    pub devtools_open: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PendingDownloadPrompt {
    pub id: DownloadId,
    pub url: String,
    pub suggested_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DownloadItem {
    pub id: DownloadId,
    pub name: String,
    pub state: DownloadState,
    pub received_bytes: u64,
    pub total_bytes: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ShellModel {
    pub tabs: Vec<TabMeta>,
    pub active_tab: Option<u64>,
    pub address_draft: String,
    pub address_focused: bool,
    pub find_open: bool,
    pub find_query: String,
    pub find_focused: bool,
    pub find_last_result: Option<String>,
    pub pending_download: Option<PendingDownloadPrompt>,
    pub active_downloads: Vec<DownloadItem>,
    pub pdf_status: Option<String>,
    pub status_banner: String,
    pub quit_requested: bool,
}

impl ShellModel {
    pub fn normalize_shell_url(input: &str) -> Option<String> {
        if input
            .bytes()
            .any(|b| b == 0 || b == b'\r' || b == b'\n' || b.is_ascii_control())
        {
            return None;
        }
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return None;
        }
        if trimmed == "about:blank" {
            return Some("about:blank".into());
        }
        if trimmed.starts_with("app://demo/") || trimmed.starts_with("data:") {
            return Some(trimmed.to_owned());
        }
        if !trimmed.contains("://")
            && !trimmed.contains(char::is_whitespace)
            && !trimmed.contains(':')
        {
            let suffix = trimmed.trim_start_matches('/').trim_start_matches("demo/");
            if suffix.is_empty() {
                return Some(START_URL.into());
            }
            return Some(format!("app://demo/{suffix}"));
        }
        None
    }

    pub fn add_tab(&mut self, id: u64, url: String) -> bool {
        if self.tabs.len() >= MAX_SHELL_TABS || self.tabs.iter().any(|t| t.id == id) {
            return false;
        }
        self.tabs.push(TabMeta {
            id,
            title: "New Tab".into(),
            url: url.clone(),
            can_go_back: false,
            can_go_forward: false,
            is_loading: true,
            loading_progress_per_mille: 0,
            zoom_level_milli: 0,
            audio_muted: false,
            devtools_open: false,
        });
        self.active_tab = Some(id);
        if !self.address_focused {
            self.address_draft = url;
        }
        true
    }

    pub fn switch_tab(&mut self, id: u64) -> bool {
        let Some(tab) = self.tabs.iter().find(|t| t.id == id) else {
            return false;
        };
        self.active_tab = Some(id);
        if !self.address_focused {
            self.address_draft = tab.url.clone();
        }
        true
    }

    pub fn close_tab(&mut self, id: u64) -> bool {
        let Some(idx) = self.tabs.iter().position(|t| t.id == id) else {
            return false;
        };
        self.tabs.remove(idx);
        if self.tabs.is_empty() {
            self.active_tab = None;
            self.quit_requested = true;
            return true;
        }
        if self.active_tab == Some(id) {
            let next_idx = idx.min(self.tabs.len() - 1);
            let next = &self.tabs[next_idx];
            self.active_tab = Some(next.id);
            if !self.address_focused {
                self.address_draft = next.url.clone();
            }
        }
        true
    }

    pub fn active_meta(&self) -> Option<&TabMeta> {
        let id = self.active_tab?;
        self.tabs.iter().find(|t| t.id == id)
    }

    pub fn active_meta_mut(&mut self) -> Option<&mut TabMeta> {
        let id = self.active_tab?;
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    pub fn can_accept_popup(&self, req: &PopupRequest) -> bool {
        req.policy_allowed && !req.target_url.is_empty() && self.tabs.len() < MAX_SHELL_TABS
    }

    pub fn apply_nav_snapshot(&mut self, id: u64, nav: &NavigationSnapshot) {
        let is_active = self.active_tab == Some(id);
        let address_focused = self.address_focused;
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) else {
            return;
        };
        if !nav.title.is_empty() {
            tab.title = nav.title.clone();
        } else if !nav.address.is_empty() {
            tab.title = nav.address.clone();
        }
        if !nav.address.is_empty() {
            tab.url = nav.address.clone();
        }
        tab.can_go_back = nav.can_go_back;
        tab.can_go_forward = nav.can_go_forward;
        tab.is_loading = nav.is_loading;
        tab.loading_progress_per_mille = nav.loading_progress_per_mille.min(1000);
        tab.zoom_level_milli = nav.zoom_level_milli;
        tab.audio_muted = nav.audio_muted;
        if is_active && !address_focused && !tab.url.is_empty() {
            self.address_draft = tab.url.clone();
        }
    }

    pub fn note_download_progress(&mut self, name: Option<&str>, progress: &DownloadProgress) {
        if let Some(existing) = self
            .active_downloads
            .iter_mut()
            .find(|item| item.id == progress.id)
        {
            existing.state = progress.state;
            existing.received_bytes = progress.received_bytes;
            existing.total_bytes = progress.total_bytes;
            if let Some(full) = progress
                .full_path
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                && !full.is_empty()
            {
                existing.name = full.to_owned();
            }
            return;
        }
        if self.active_downloads.len() >= MAX_DOWNLOAD_HISTORY {
            self.active_downloads.remove(0);
        }
        let fallback_name = name
            .filter(|n| !n.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("download-{}", progress.id.sequence));
        self.active_downloads.push(DownloadItem {
            id: progress.id,
            name: fallback_name,
            state: progress.state,
            received_bytes: progress.received_bytes,
            total_bytes: progress.total_bytes,
        });
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum ShellAction {
    NewTab(Option<String>),
    SwitchTab(u64),
    CloseTab(u64),
    NavigateActive(String),
    Back,
    Forward,
    Reload,
    StopLoading,
    ZoomSteps(i32),
    ZoomReset,
    ToggleMute,
    ToggleFind,
    FindStep { forward: bool },
    CloseFind,
    ConfirmPendingDownload { accept: bool },
    ExportPdf,
    ToggleDevTools,
}

fn push_action(
    actions: &Rc<RefCell<Vec<ShellAction>>>,
    wake: &Arc<UiWakeSender>,
    action: ShellAction,
) {
    actions.borrow_mut().push(action);
    let _ = wake.wake();
}

pub(super) fn escape_js_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn truncate_appended(buf: &mut String, text: &str, max_bytes: usize) {
    for ch in text.chars() {
        if buf.len() + ch.len_utf8() > max_bytes {
            break;
        }
        buf.push(ch);
    }
}

#[derive(Clone)]
enum ShellView {
    Osr(Entity<CefBrowser>),
    Native(Entity<NativeCefBrowser>),
}

impl ShellView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            Self::Osr(view) => view.read(cx).focus_handle(cx),
            Self::Native(view) => view.read(cx).focus_handle(cx),
        }
    }

    fn cursor_debug(&self, cx: &App) -> String {
        match self {
            Self::Osr(view) => format!("{:?}", view.read(cx).cursor()),
            Self::Native(view) => format!("{:?}", view.read(cx).cursor()),
        }
    }

    fn tooltip(&self, cx: &App) -> String {
        match self {
            Self::Osr(view) => view.read(cx).tooltip().to_owned(),
            Self::Native(view) => view.read(cx).tooltip().to_owned(),
        }
    }

    fn status_text(&self, cx: &App) -> String {
        match self {
            Self::Osr(view) => view.read(cx).status_text().to_owned(),
            Self::Native(view) => view.read(cx).status_text().to_owned(),
        }
    }

    fn zoom_level_milli(&self, cx: &App) -> Option<i32> {
        let level = match self {
            Self::Osr(view) => view.read(cx).zoom_level(),
            Self::Native(view) => view.read(cx).zoom_level(),
        };
        zoom_level_to_milli(level).ok()
    }

    fn step_zoom(&self, steps: i32, window: &mut Window, cx: &mut App) {
        match self {
            Self::Osr(view) => view.update(cx, |v, cx| v.step_zoom(steps, window, cx)),
            Self::Native(view) => view.update(cx, |v, _| {
                let next = step_canonical_zoom_level(v.zoom_level(), steps);
                let _ = v.set_zoom_level(next);
            }),
        }
    }

    fn reset_zoom(&self, window: &mut Window, cx: &mut App) {
        match self {
            Self::Osr(view) => view.update(cx, |v, cx| v.reset_zoom(window, cx)),
            Self::Native(view) => view.update(cx, |v, _| {
                let _ = v.set_zoom_level(0.0);
            }),
        }
    }

    fn toggle_audio_muted(&self, cx: &mut App) {
        match self {
            Self::Osr(view) => view.update(cx, |v, _| {
                let _ = v.set_audio_muted(!v.audio_muted());
            }),
            Self::Native(view) => view.update(cx, |v, _| {
                let _ = v.set_audio_muted(!v.audio_muted());
            }),
        }
    }

    fn evaluate_script(
        &self,
        script: impl Into<String>,
        scope: JsFrameScope,
        timeout_ms: u64,
        cx: &mut App,
    ) {
        match self {
            Self::Osr(view) => view.update(cx, |v, _| {
                let _ = v.evaluate_script(script, scope, timeout_ms);
            }),
            Self::Native(view) => view.update(cx, |v, _| {
                let _ = v.evaluate_script(script, scope, timeout_ms);
            }),
        }
    }

    fn set_visible(&self, visible: bool, cx: &mut App) {
        if let Self::Native(view) = self {
            view.update(cx, |v, cx| v.set_visible(visible, cx));
        }
    }

    fn is_native_visible(&self, cx: &App) -> bool {
        match self {
            Self::Osr(_) => false,
            Self::Native(view) => view.read(cx).diagnostics().visible,
        }
    }

    fn consume_and_sample(&self, window: &mut Window, cx: &mut App) -> Option<ViewDiagnostics> {
        match self {
            Self::Osr(view) => view.update(cx, |v, cx| {
                let _ = v.consume(window, cx);
                Some(v.diagnostics())
            }),
            Self::Native(view) => view.update(cx, |v, cx| {
                let _ = v.consume(window, cx);
                None
            }),
        }
    }

    fn release_or_retire(self, window: &mut Window, cx: &mut App) -> Option<ViewDiagnostics> {
        match self {
            Self::Osr(view) => Some(view.update(cx, |v, cx| {
                let _ = v.release_image(window, cx);
                v.diagnostics()
            })),
            Self::Native(view) => {
                view.update(cx, |v, _| v.retire());
                None
            }
        }
    }

    fn into_any_element(self) -> AnyElement {
        match self {
            Self::Osr(view) => view.into_any_element(),
            Self::Native(view) => view.into_any_element(),
        }
    }
}

struct ShellRoot {
    model: ShellModel,
    host_mode: HostMode,
    views: BTreeMap<u64, ShellView>,
    menu_presenter: Option<Rc<RefCell<DefaultMenuPresenter>>>,
    actions: Rc<RefCell<Vec<ShellAction>>>,
    wake: Arc<UiWakeSender>,
    focus: FocusHandle,
    smoke_find_ok: bool,
}

impl ShellRoot {
    fn emit(&self, action: ShellAction) {
        push_action(&self.actions, &self.wake, action);
    }

    fn sync_native_tab_visibility(&self, cx: &mut App) {
        if self.host_mode != HostMode::NativeHwnd {
            return;
        }
        let active = self.model.active_tab;
        for (id, view) in &self.views {
            view.set_visible(active == Some(*id), cx);
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.model.address_focused && !self.model.find_focused {
            return;
        }
        let editing_address = self.model.address_focused;
        let key = event.keystroke.key.as_str();
        match key {
            "backspace" => {
                if editing_address {
                    self.model.address_draft.pop();
                } else {
                    self.model.find_query.pop();
                }
                cx.notify();
            }
            "enter" => {
                if editing_address {
                    let target = self.model.address_draft.clone();
                    self.model.address_focused = false;
                    self.emit(ShellAction::NavigateActive(target));
                } else {
                    let forward = !event.keystroke.modifiers.shift;
                    self.emit(ShellAction::FindStep { forward });
                }
                cx.notify();
            }
            "escape" => {
                if editing_address {
                    self.model.address_focused = false;
                    if let Some(meta) = self.model.active_meta() {
                        self.model.address_draft = meta.url.clone();
                    }
                } else {
                    self.model.find_focused = false;
                    self.emit(ShellAction::CloseFind);
                }
                if let Some(view) = self
                    .model
                    .active_tab
                    .and_then(|id| self.views.get(&id))
                    .cloned()
                {
                    let handle = view.focus_handle(cx);
                    window.focus(&handle, cx);
                }
                cx.notify();
            }
            _ => {
                if event.keystroke.modifiers.control
                    || event.keystroke.modifiers.alt
                    || event.keystroke.modifiers.platform
                {
                    return;
                }
                let text = event
                    .keystroke
                    .key_char
                    .as_deref()
                    .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
                    .or_else(|| (key.chars().count() == 1).then_some(key));
                if let Some(ch) = text {
                    if editing_address {
                        truncate_appended(&mut self.model.address_draft, ch, MAX_INPUT_BYTES);
                    } else {
                        truncate_appended(&mut self.model.find_query, ch, MAX_INPUT_BYTES);
                    }
                    cx.notify();
                }
            }
        }
    }

    fn pill(
        id: impl Into<gpui::ElementId>,
        label: impl Into<gpui::SharedString>,
        bg_hex: u32,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(id)
            .px(px(7.0))
            .py(px(2.0))
            .bg(rgb(bg_hex))
            .text_color(rgb(0xf9fafb))
            .text_size(px(12.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    on_click(this, window, cx);
                }),
            )
            .child(label.into())
    }
}

impl Render for ShellRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let menu = self
            .menu_presenter
            .as_ref()
            .and_then(|presenter| presenter.borrow().element(window, cx));

        let active_id = self.model.active_tab;
        let active_meta = self.model.active_meta().cloned();
        let active_view = active_id.and_then(|id| self.views.get(&id).cloned());

        let (cursor_dbg, view_tooltip, view_status) = active_view
            .as_ref()
            .map(|view| {
                (
                    view.cursor_debug(cx),
                    view.tooltip(cx),
                    view.status_text(cx),
                )
            })
            .unwrap_or_else(|| ("Arrow".into(), String::new(), String::new()));

        let mut tab_bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .px(px(6.0))
            .h(px(30.0))
            .flex_shrink_0()
            .overflow_hidden()
            .bg(rgb(0x111827));

        for tab in &self.model.tabs {
            let tab_id = tab.id;
            let is_active = Some(tab_id) == active_id;
            let mut label = if tab.title.is_empty() {
                tab.url.clone()
            } else {
                tab.title.clone()
            };
            if label.chars().count() > 22 {
                label = label.chars().take(21).collect::<String>() + "…";
            }
            tab_bar = tab_bar.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(8.0))
                    .py(px(3.0))
                    .bg(rgb(if is_active { 0x2563eb } else { 0x1f2937 }))
                    .text_color(rgb(0xf9fafb))
                    .text_size(px(12.0))
                    .child(
                        div()
                            .id(("shell-tab-select", tab_id as usize))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    this.model.address_focused = false;
                                    this.model.find_focused = false;
                                    this.emit(ShellAction::SwitchTab(tab_id));
                                    cx.notify();
                                }),
                            )
                            .child(label),
                    )
                    .child(
                        div()
                            .id(("shell-tab-close", tab_id as usize))
                            .px(px(4.0))
                            .bg(rgb(0x374151))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, _| {
                                    this.emit(ShellAction::CloseTab(tab_id));
                                }),
                            )
                            .child("×"),
                    ),
            );
        }
        tab_bar = tab_bar.child(Self::pill(
            "shell-new-tab",
            "+ New Tab",
            0x047857,
            |this, _, _| {
                this.emit(ShellAction::NewTab(None));
            },
            cx,
        ));

        let can_back = active_meta.as_ref().is_some_and(|m| m.can_go_back);
        let can_fwd = active_meta.as_ref().is_some_and(|m| m.can_go_forward);
        let is_loading = active_meta.as_ref().is_some_and(|m| m.is_loading);
        let progress = active_meta
            .as_ref()
            .map_or(0, |m| m.loading_progress_per_mille);
        let zoom_pct = if let Some(ShellView::Osr(v)) = &active_view {
            zoom_level_to_percent(v.read(cx).zoom_level()).round() as i32
        } else {
            active_meta.as_ref().map_or(100, |m| {
                zoom_level_to_percent(f64::from(m.zoom_level_milli) / 1000.0).round() as i32
            })
        };
        let muted = active_meta.as_ref().is_some_and(|m| m.audio_muted);
        let devtools_open = active_meta.as_ref().is_some_and(|m| m.devtools_open);

        let address_display = if self.model.address_draft.is_empty() {
            "app://demo/index.html".to_owned()
        } else {
            self.model.address_draft.clone()
        };

        let nav_bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.0))
            .px(px(6.0))
            .h(px(32.0))
            .flex_shrink_0()
            .overflow_hidden()
            .bg(rgb(0x1f2937))
            .child(Self::pill(
                "shell-back",
                "Back",
                if can_back { 0x374151 } else { 0x111827 },
                |this, _, _| this.emit(ShellAction::Back),
                cx,
            ))
            .child(Self::pill(
                "shell-forward",
                "Fwd",
                if can_fwd { 0x374151 } else { 0x111827 },
                |this, _, _| this.emit(ShellAction::Forward),
                cx,
            ))
            .child(Self::pill(
                "shell-reload",
                "Reload",
                0x374151,
                |this, _, _| this.emit(ShellAction::Reload),
                cx,
            ))
            .child(Self::pill(
                "shell-stop",
                "Stop",
                if is_loading { 0x991b1b } else { 0x374151 },
                |this, _, _| this.emit(ShellAction::StopLoading),
                cx,
            ))
            .child(
                div()
                    .id("shell-address-input")
                    .flex_1()
                    .px(px(8.0))
                    .py(px(3.0))
                    .bg(rgb(if self.model.address_focused {
                        0x0f172a
                    } else {
                        0x111827
                    }))
                    .text_color(rgb(0xe5e7eb))
                    .text_size(px(12.0))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.model.address_focused = true;
                            this.model.find_focused = false;
                            let handle = this.focus.clone();
                            window.focus(&handle, cx);
                            cx.notify();
                        }),
                    )
                    .child(address_display),
            )
            .child(Self::pill(
                "shell-go",
                "Go",
                0x2563eb,
                |this, _, cx| {
                    this.model.address_focused = false;
                    let url = this.model.address_draft.clone();
                    this.emit(ShellAction::NavigateActive(url));
                    cx.notify();
                },
                cx,
            ))
            .child(Self::pill(
                "shell-home",
                "Home",
                0x374151,
                |this, _, _| this.emit(ShellAction::NavigateActive(START_URL.into())),
                cx,
            ))
            .child(Self::pill(
                "shell-p2",
                "P2",
                0x374151,
                |this, _, _| this.emit(ShellAction::NavigateActive(PAGE2_URL.into())),
                cx,
            ))
            .child(Self::pill(
                "shell-popup-link",
                "Popup",
                0x374151,
                |this, _, _| this.emit(ShellAction::NewTab(Some(POPUP_URL.into()))),
                cx,
            ))
            .child(Self::pill(
                "shell-dl-link",
                "DL",
                0x374151,
                |this, _, _| this.emit(ShellAction::NavigateActive(DOWNLOAD_URL.into())),
                cx,
            ))
            .child(Self::pill(
                "shell-zoom-out",
                "-",
                0x374151,
                |this, _, _| this.emit(ShellAction::ZoomSteps(-1)),
                cx,
            ))
            .child(Self::pill(
                "shell-zoom-reset",
                format!("{zoom_pct}%"),
                0x374151,
                |this, _, _| this.emit(ShellAction::ZoomReset),
                cx,
            ))
            .child(Self::pill(
                "shell-zoom-in",
                "+",
                0x374151,
                |this, _, _| this.emit(ShellAction::ZoomSteps(1)),
                cx,
            ))
            .child(Self::pill(
                "shell-mute",
                if muted { "Unmute" } else { "Mute" },
                if muted { 0xb45309 } else { 0x374151 },
                |this, _, _| this.emit(ShellAction::ToggleMute),
                cx,
            ))
            .child(Self::pill(
                "shell-find-toggle",
                "Find",
                if self.model.find_open {
                    0x2563eb
                } else {
                    0x374151
                },
                |this, window, cx| {
                    this.emit(ShellAction::ToggleFind);
                    let handle = this.focus.clone();
                    window.focus(&handle, cx);
                },
                cx,
            ))
            .child(Self::pill(
                "shell-pdf",
                "PDF",
                0x374151,
                |this, _, _| this.emit(ShellAction::ExportPdf),
                cx,
            ))
            .child(Self::pill(
                "shell-devtools",
                if devtools_open {
                    "DevTools*"
                } else {
                    "DevTools"
                },
                if devtools_open { 0x7c3aed } else { 0x374151 },
                |this, _, _| this.emit(ShellAction::ToggleDevTools),
                cx,
            ));

        let progress_fraction = (f32::from(progress) / 1000.0).clamp(0.0, 1.0);
        let progress_bar = div()
            .w_full()
            .h(px(2.0))
            .flex_shrink_0()
            .bg(rgb(0x111827))
            .child(
                div()
                    .h_full()
                    .bg(rgb(if is_loading { 0x3b82f6 } else { 0x111827 }))
                    .w(px(800.0 * progress_fraction)),
            );

        let mut root = div()
            .id("shell-root")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key_down(event, window, cx);
            }))
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(rgb(0x0f172a))
            .child(tab_bar)
            .child(nav_bar)
            .child(progress_bar);

        let viewport = div().relative().flex_1().w_full().overflow_hidden();
        let mut viewport = if let Some(view) = active_view {
            viewport.child(view.into_any_element())
        } else {
            viewport
                .text_color(rgb(0xf3f4f6))
                .child(div().p(px(16.0)).child(self.model.status_banner.clone()))
        };

        let find_bar = self.model.find_open.then(|| {
            let query_display = if self.model.find_query.is_empty() {
                "<click & type query>".to_owned()
            } else {
                self.model.find_query.clone()
            };
            let result_text = self
                .model
                .find_last_result
                .as_deref()
                .unwrap_or("")
                .to_owned();
            let bar = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .px(px(8.0))
                .h(px(28.0))
                .bg(rgb(0x1e293b))
                .text_color(rgb(0xe2e8f0))
                .text_size(px(12.0))
                .child("Find:")
                .child(
                    div()
                        .id("shell-find-input")
                        .px(px(8.0))
                        .py(px(2.0))
                        .min_w(px(160.0))
                        .bg(rgb(if self.model.find_focused {
                            0x0f172a
                        } else {
                            0x111827
                        }))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.model.find_focused = true;
                                this.model.address_focused = false;
                                let handle = this.focus.clone();
                                window.focus(&handle, cx);
                                cx.notify();
                            }),
                        )
                        .child(query_display),
                )
                .child(Self::pill(
                    "shell-find-next",
                    "Next",
                    0x2563eb,
                    |this, _, _| this.emit(ShellAction::FindStep { forward: true }),
                    cx,
                ))
                .child(Self::pill(
                    "shell-find-prev",
                    "Prev",
                    0x374151,
                    |this, _, _| this.emit(ShellAction::FindStep { forward: false }),
                    cx,
                ))
                .child(Self::pill(
                    "shell-find-close",
                    "Close",
                    0x991b1b,
                    |this, _, _| this.emit(ShellAction::CloseFind),
                    cx,
                ))
                .child(result_text);
            if self.host_mode == HostMode::Osr {
                bar.absolute().top_0().left_0().right_0()
            } else {
                bar.w_full().flex_shrink_0()
            }
        });

        let dl_bar = (self.model.pending_download.is_some()
            || !self.model.active_downloads.is_empty())
        .then(|| {
            let mut bar = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .px(px(8.0))
                .h(px(26.0))
                .bg(rgb(0x172554))
                .text_color(rgb(0xe0f2fe))
                .text_size(px(12.0));
            if let Some(pending) = &self.model.pending_download {
                bar = bar
                    .child(format!("Download request: {}", pending.suggested_name))
                    .child(Self::pill(
                        "shell-dl-save",
                        "Save",
                        0x047857,
                        |this, _, _| {
                            this.emit(ShellAction::ConfirmPendingDownload { accept: true })
                        },
                        cx,
                    ))
                    .child(Self::pill(
                        "shell-dl-cancel",
                        "Cancel",
                        0x991b1b,
                        |this, _, _| {
                            this.emit(ShellAction::ConfirmPendingDownload { accept: false })
                        },
                        cx,
                    ));
            }
            for item in self.model.active_downloads.iter().rev().take(2) {
                bar = bar.child(format!(
                    "[{} {:?} {}B]",
                    item.name, item.state, item.received_bytes
                ));
            }
            if self.host_mode == HostMode::Osr {
                bar.absolute().bottom_0().left_0().right_0()
            } else {
                bar.w_full().flex_shrink_0()
            }
        });

        if self.host_mode == HostMode::Osr {
            if let Some(bar) = find_bar {
                viewport = viewport.child(bar);
            }
            if let Some(bar) = dl_bar {
                viewport = viewport.child(bar);
            }
            root = root.child(viewport);
        } else {
            // Native HWND children occupy Win32 airspace above GPUI content, so
            // dock find/download chrome as flex siblings instead of overlays.
            if let Some(bar) = find_bar {
                root = root.child(bar);
            }
            root = root.child(viewport);
            if let Some(bar) = dl_bar {
                root = root.child(bar);
            }
        }

        let pdf_label = self
            .model
            .pdf_status
            .as_deref()
            .unwrap_or("PDF: idle")
            .to_owned();
        let status_info = if !view_tooltip.is_empty() {
            format!("Tip: {view_tooltip}")
        } else if !view_status.is_empty() {
            format!("Status: {view_status}")
        } else {
            self.model.status_banner.clone()
        };
        let footer = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(8.0))
            .h(px(22.0))
            .flex_shrink_0()
            .overflow_hidden()
            .bg(rgb(0x111827))
            .text_color(rgb(0x9ca3af))
            .text_size(px(11.0))
            .child(format!("Tabs: {}", self.model.tabs.len()))
            .child(format!("Cursor: {cursor_dbg}"))
            .child(pdf_label)
            .child(status_info);
        let root = root.child(footer);

        match menu {
            Some(menu) => root.child(menu),
            None => root,
        }
    }
}

#[derive(Default)]
struct ShellCapQueues {
    downloads: Mutex<VecDeque<DownloadRequest>>,
    progress: Mutex<VecDeque<DownloadProgress>>,
    js_dialogs: Mutex<VecDeque<JsDialogRequest>>,
    auths: Mutex<VecDeque<AuthRequest>>,
    popups: Mutex<VecDeque<PopupRequest>>,
    cdp_events: Mutex<VecDeque<CdpEvent>>,
    cdp_results: Mutex<VecDeque<CdpMethodResult>>,
}

struct ShellCapAdapter {
    queues: Arc<ShellCapQueues>,
    wake: Arc<UiWakeSender>,
}

impl DownloadService for ShellCapAdapter {
    fn on_before_download(&self, request: DownloadRequest) {
        self.queues
            .downloads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(request);
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

impl JsDialogService for ShellCapAdapter {
    fn on_js_dialog(&self, request: JsDialogRequest) {
        self.queues
            .js_dialogs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(request);
        let _ = self.wake.wake();
    }
}

impl PopupService for ShellCapAdapter {
    fn on_popup_request(&self, request: PopupRequest) {
        self.queues
            .popups
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(request);
        let _ = self.wake.wake();
    }
}

impl AuthService for ShellCapAdapter {
    fn on_auth_credentials(&self, request: AuthRequest) {
        self.queues
            .auths
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(request);
        let _ = self.wake.wake();
    }
}

impl CdpObserver for ShellCapAdapter {
    fn on_method_result(&self, result: CdpMethodResult) {
        self.queues
            .cdp_results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(result);
    }

    fn on_event(&self, event: CdpEvent) {
        self.queues
            .cdp_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(event);
    }
}

struct ShellObservations {
    strict_capture: bool,
    created: AtomicU64,
    closed: AtomicU64,
    first_fault: Mutex<Option<String>>,
}

impl ShellObservations {
    fn new(strict_capture: bool) -> Self {
        Self {
            strict_capture,
            created: AtomicU64::new(0),
            closed: AtomicU64::new(0),
            first_fault: Mutex::new(None),
        }
    }
}

impl BrowserEventSink for ShellObservations {
    fn publish(&self, event: BrowserEventEnvelope) -> PublishResult {
        match event.event {
            BrowserEvent::Created => {
                self.created.fetch_add(1, Ordering::Release);
            }
            BrowserEvent::Closed => {
                self.closed.fetch_add(1, Ordering::Release);
            }
            BrowserEvent::CaptureFailed(EngineError::Capture {
                kind: CaptureFailureKind::IncompleteFrame,
                code: None,
            }) if !self.strict_capture => {}
            fault @ (BrowserEvent::CreateFailed(_)
            | BrowserEvent::CaptureFailed(_)
            | BrowserEvent::RendererTerminated { .. }) => {
                let message = format!("shell Browser {:?}: {fault:?}", event.browser);
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

fn shell_demo_settings() -> RuntimeSettings {
    let mut assets = BTreeMap::new();
    assets.insert(
        "index.html".into(),
        MemoryAsset::html(
            br##"<!doctype html><html><head><meta charset="utf-8"><title>GPUI-CEF Browser Shell</title></head>
<body style="margin:0;background:#1e293b;color:#f8fafc;font-family:sans-serif;padding:20px">
<h1>GPUI CEF Shell - Multi-tab Desktop Showcase</h1>
<p>Searchable text: GPUI CEF Shell</p>
<p>
  <a href="app://demo/page2.html">Go to Page 2</a> |
  <a href="#" onclick="window.open('app://demo/popup.html', '_blank'); return false;">Open Popup Tab</a> |
  <a id="download-sample" href="app://demo/sample.txt" download="sample.txt">Download sample.txt</a>
</p>
<div style="margin-top:14px;padding:10px;border:1px solid #475569;display:inline-block" draggable="true">
  Draggable Shell Snippet
</div>
</body></html>"##
                .to_vec(),
        ),
    );
    assets.insert(
        "page2.html".into(),
        MemoryAsset::html(
            br#"<!doctype html><html><head><meta charset="utf-8"><title>GPUI-CEF Shell - Page 2</title></head>
<body style="margin:0;background:#0f766e;color:#f8fafc;font-family:sans-serif;padding:20px">
<h1>Second Page</h1>
<p><a href="app://demo/index.html">Back to Start Page</a></p>
</body></html>"#
                .to_vec(),
        ),
    );
    assets.insert(
        "popup.html".into(),
        MemoryAsset::html(
            br#"<!doctype html><html><head><meta charset="utf-8"><title>GPUI-CEF Shell - Popup Tab</title></head>
<body style="margin:0;background:#4338ca;color:#f8fafc;font-family:sans-serif;padding:20px">
<h1>Popup Accepted Into Shell Tab</h1>
</body></html>"#
                .to_vec(),
        ),
    );
    assets.insert(
        "sample.txt".into(),
        MemoryAsset {
            status_code: 200,
            mime_type: "application/octet-stream".into(),
            headers: vec![(
                "Content-Disposition".into(),
                "attachment; filename=\"sample.txt\"".into(),
            )],
            body: b"GPUI-CEF component demo download payload\n".to_vec(),
        },
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

fn shell_shortcut_filter() -> ShortcutFilter {
    ShortcutFilter::new(vec![
        ShortcutRule {
            id: ShortcutId(1),
            windows_key_code: 0xBB,
            shift: false,
            control: true,
            alt: false,
            platform: false,
        },
        ShortcutRule {
            id: ShortcutId(2),
            windows_key_code: 0xBD,
            shift: false,
            control: true,
            alt: false,
            platform: false,
        },
        ShortcutRule {
            id: ShortcutId(3),
            windows_key_code: 0x30,
            shift: false,
            control: true,
            alt: false,
            platform: false,
        },
        ShortcutRule {
            id: ShortcutId(4),
            windows_key_code: 0x4D,
            shift: false,
            control: true,
            alt: false,
            platform: false,
        },
    ])
    .expect("valid shell shortcuts")
}

struct TabRuntime {
    browser_id: BrowserId,
    handle: BrowserHandle,
    first_paint: bool,
}

#[derive(Default)]
struct ShellAccounting {
    tabs_opened: u64,
    tabs_closed: u64,
    created_images: u64,
    released_images: u64,
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
            Err("shell wake channel closed".into())
        };
    };
    let mut wake = std::pin::pin!(receiver.recv());
    let mut timer = std::pin::pin!(cx.background_executor().timer(timeout));
    std::future::poll_fn(|cx| {
        if let Poll::Ready(ok) = wake.as_mut().poll(cx) {
            return Poll::Ready(if ok {
                Ok(true)
            } else {
                Err("shell wake channel closed".into())
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
        rt.pump().map_err(|e| format!("shell pump: {e:?}"))?;
    }
    Ok(woke)
}

enum ShellShortcutOp {
    Zoom(f64),
    Mute(bool),
}

fn plan_shell_shortcut(
    id: ShortcutId,
    zoom_level: f64,
    audio_muted: bool,
) -> Option<ShellShortcutOp> {
    match id.0 {
        1 => Some(ShellShortcutOp::Zoom(step_canonical_zoom_level(zoom_level, 1))),
        2 => Some(ShellShortcutOp::Zoom(step_canonical_zoom_level(zoom_level, -1))),
        3 => Some(ShellShortcutOp::Zoom(0.0)),
        4 => Some(ShellShortcutOp::Mute(!audio_muted)),
        _ => None,
    }
}

fn dispatch_shell_eval_outcome(
    eval_root: &gpui::WeakEntity<ShellRoot>,
    outcome: EvalOutcome,
    app: &mut App,
) {
    let found = matches!(&outcome, EvalOutcome::Value(json) if json == "true");
    let msg = match outcome {
        EvalOutcome::Value(json) if json == "true" => "match found".to_owned(),
        EvalOutcome::Value(json) if json == "false" => "no match".to_owned(),
        EvalOutcome::Value(other) => format!("eval={other}"),
        EvalOutcome::Err(err) => format!("err={err:?}"),
    };
    let eval_root = eval_root.clone();
    app.defer(move |app| {
        let _ = eval_root.update(app, |shell_root, cx| {
            if found {
                shell_root.smoke_find_ok = true;
            }
            shell_root.model.find_last_result = Some(msg);
            cx.notify();
        });
    });
}

fn spawn_tab(
    cx: &mut AsyncApp,
    rt_cell: &Rc<RefCell<Option<Runtime>>>,
    root: WindowHandle<ShellRoot>,
    config: &Config,
    tab_id: u64,
    url: String,
    accounting: &mut ShellAccounting,
    tabs: &mut BTreeMap<u64, TabRuntime>,
) -> Result<ShellView, String> {
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
    let width = f32::from(available.width).round().max(1.0) as i32;
    let height = (f32::from(available.height) - SHELL_CHROME_HEIGHT)
        .round()
        .max(1.0) as i32;
    let viewport = Viewport {
        width,
        height,
        scale,
    };
    let mut options = BrowserOptions::default()
        .with_frame_rate(rust_cef_gpui::recommended_windowless_frame_rate())
        .with_tooltip_events(true)
        .with_shortcut_filter(shell_shortcut_filter())
        .with_headless_cdp(config.host_capabilities)
        .with_outbound_drag_policy(OutboundDragPolicy::AllowAsync)
        .with_accessibility_state(CefFeatureState::Enabled)
        .with_js_bridge(JsBridgeConfig {
            enabled: true,
            allowed_origins: vec!["app://demo".into()],
            max_message_bytes: MAX_JS_PAYLOAD_BYTES,
            ..Default::default()
        });
    if config.host_mode == HostMode::NativeHwnd {
        let parent_hwnd =
            parent_hwnd.ok_or("shell window HWND unavailable for native child mode")?;
        options =
            options.with_surface_mode(BrowserSurfaceMode::NativeChildHwnd(NativeChildHwndConfig {
                parent_hwnd,
                initial_bounds: PixelRect {
                    x: 0,
                    y: (64.0 * scale).round() as i32,
                    width: (width as f32 * scale).round().max(1.0) as u32,
                    height: (height as f32 * scale).round().max(1.0) as u32,
                },
                activate_on_creation: false,
            }));
    }

    let (handle, attachment, mode) = {
        let mut guard = rt_cell.borrow_mut();
        let rt = guard.as_mut().ok_or("shell Runtime absent")?;
        let profile = Some(rt.default_profile_handle());
        let handle = rt
            .create_browser(BrowserConfig {
                initial_url: url.clone(),
                profile,
                render_preference: config.backend,
                viewport,
                options,
            })
            .map_err(|e| format!("shell create tab {tab_id}: {e:?}"))?;
        let attachment = rt
            .attach_browser(&handle)
            .map_err(|e| format!("shell attach tab {tab_id}: {e:?}"))?;
        (handle, attachment, rt.mode())
    };
    let browser_id = handle.snapshot().browser;

    let shell_view = root
        .update(cx, |root, window, cx| {
            let root_weak = cx.entity().downgrade();
            let shell_view = match config.host_mode {
                HostMode::Osr => {
                    let view =
                        CefBrowser::new(attachment, window, cx).map_err(|e| e.to_string())?;
                    let rt_pump = rt_cell.clone();
                    let _ = view.update(cx, |v, _| {
                        v.set_pinch_zoom_enabled(true);
                        v.set_ctrl_wheel_zoom_enabled(true);
                        if mode == ThreadMode::Integrated {
                            v.set_outbound_drag_pump(Some(Rc::new(move || {
                                if let Ok(mut guard) = rt_pump.try_borrow_mut()
                                    && let Some(rt) = guard.as_mut()
                                {
                                    let _ = rt.pump();
                                }
                            })));
                        }
                        v.set_on_shortcut(Some(Box::new(|id, view, window, cx| {
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
                        v.set_on_eval_completed(Some(Box::new(move |_, outcome, _, _, app| {
                            dispatch_shell_eval_outcome(&root_weak, outcome, app);
                        })));
                    });
                    ShellView::Osr(view)
                }
                HostMode::NativeHwnd => {
                    let view =
                        NativeCefBrowser::new(attachment, window, cx).map_err(|e| e.to_string())?;
                    let _ = view.update(cx, |v, _| {
                        v.set_on_shortcut(Some(Box::new(|id, view, _, cx| {
                            match plan_shell_shortcut(id, view.zoom_level(), view.audio_muted()) {
                                Some(ShellShortcutOp::Zoom(next)) => {
                                    let _ = view.set_zoom_level(next);
                                }
                                Some(ShellShortcutOp::Mute(muted)) => {
                                    let _ = view.set_audio_muted(muted);
                                }
                                None => {}
                            }
                            cx.notify();
                        })));
                        v.set_on_eval_completed(Some(Box::new(move |_, outcome, _, _, app| {
                            dispatch_shell_eval_outcome(&root_weak, outcome, app);
                        })));
                    });
                    ShellView::Native(view)
                }
            };
            root.model.add_tab(tab_id, url);
            root.model.status_banner = format!("Opened tab {tab_id}");
            root.views.insert(tab_id, shell_view.clone());
            root.sync_native_tab_visibility(cx);
            let view_focus = shell_view.focus_handle(cx);
            window.focus(&view_focus, cx);
            cx.notify();
            Ok::<_, String>(shell_view)
        })
        .map_err(|e| e.to_string())??;

    accounting.tabs_opened += 1;
    tabs.insert(
        tab_id,
        TabRuntime {
            browser_id,
            handle,
            first_paint: false,
        },
    );
    Ok(shell_view)
}

fn close_one_tab(
    cx: &mut AsyncApp,
    rt_cell: &Rc<RefCell<Option<Runtime>>>,
    root: WindowHandle<ShellRoot>,
    tab_id: u64,
    accounting: &mut ShellAccounting,
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

    let removed: Option<ShellView> = root
        .update(cx, |root, _, cx| {
            root.model.close_tab(tab_id);
            let removed = root.views.remove(&tab_id);
            root.sync_native_tab_visibility(cx);
            cx.notify();
            removed
        })
        .map_err(|e| e.to_string())?;
    let diag: Option<ViewDiagnostics> = if let Some(view) = removed {
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
    let _ = tab_rt.handle.request_close(CloseMode::Graceful);
    accounting.tabs_closed += 1;
    Ok(())
}

fn announce_shell_gate(gate: &Path) -> Result<(), String> {
    let ready = gate.with_extension("ready");
    let temp = gate.with_extension("ready.pending");
    if gate.exists() || ready.exists() || temp.exists() {
        return Err("shell smoke observer paths must be unused".into());
    }
    let mut marker = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|e| format!("shell smoke marker: {e}"))?;
    write!(marker, "{}", std::process::id()).map_err(|e| format!("shell smoke marker: {e}"))?;
    drop(marker);
    std::fs::rename(temp, ready).map_err(|e| format!("shell smoke ready: {e}"))
}

async fn drive_shell(
    cx: &mut AsyncApp,
    root: WindowHandle<ShellRoot>,
    config: Config,
    close: Rc<Cell<bool>>,
    sender: Arc<UiWakeSender>,
    receiver: UiWakeReceiver,
    actions: Rc<RefCell<Vec<ShellAction>>>,
) -> i32 {
    let obs = Arc::new(ShellObservations::new(config.smoke.is_some()));
    let cap_queues = Arc::new(ShellCapQueues::default());
    let cap_adapter = Arc::new(ShellCapAdapter {
        queues: cap_queues.clone(),
        wake: sender.clone(),
    });
    let output_dir = config
        .cache_root
        .parent()
        .unwrap_or(&config.cache_root)
        .join("demo-shell-output");
    let _ = std::fs::create_dir_all(&output_dir);

    let rt = match Runtime::start(
        RuntimeConfig {
            files: RuntimeFiles {
                directory: config.bundle.clone(),
                helper_executable: config.helper.clone(),
            },
            root_cache_path: config.cache_root.clone(),
            thread_mode: config.mode,
            default_profile: ProfileStorage::Memory,
            content_policy: TrustedContentPolicy {
                allowed_origins: vec!["app://demo".into()],
                allow_embedded_data: true,
            },
            settings: {
                let mut settings = shell_demo_settings();
                settings.windowless_rendering_enabled = config.host_mode == HostMode::Osr;
                settings
            },
        },
        RuntimeServices {
            events: obs.clone(),
            frames: Arc::new(IgnoreFrame),
            ui: sender.clone(),
        },
    ) {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("component demo shell start: {e:?}");
            return match e {
                StartError::ProcessExitRequired { exit_code }
                | StartError::InitializeFailed { exit_code } => {
                    exit_code.filter(|code| *code != 0).unwrap_or(1)
                }
                _ => 1,
            };
        }
    };

    let nz = |n: usize| std::num::NonZeroUsize::new(n).expect("non-zero");
    let _ = rt.set_host_capabilities(HostCapabilities {
        download: Some(cap_adapter.clone()),
        js_dialog: Some(cap_adapter.clone()),
        popup: Some(cap_adapter.clone()),
        auth: Some(cap_adapter.clone()),
        cdp_observer: config.host_capabilities.then_some(cap_adapter),
        ..Default::default()
    });
    let _ = rt.configure_devtools(nz(4));
    if config.context_menu {
        let limits = MenuLimits {
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
            capacity: nz(MAX_SHELL_TABS),
            model_limits: limits,
            context_limits: ctx_limits,
        });
        if config.menu_structure {
            let _ = rt.configure_menu_structure(true);
        }
    }

    let rt_cell = Rc::new(RefCell::new(Some(rt)));
    let mut accounting = ShellAccounting::default();
    let mut tabs: BTreeMap<u64, TabRuntime> = BTreeMap::new();
    let mut menus_owned: Option<(
        ContextMenuServices,
        ContextMenuRegistration,
        Rc<RefCell<DefaultMenuPresenter>>,
        u64,
    )> = None;
    let mut next_tab_id = 1u64;
    let mut active_pdf: Option<PdfExportTicket> = None;
    let mut pdf_path: Option<PathBuf> = None;
    let mut pdf_completed = false;
    let mut download_dest: Option<PathBuf> = None;
    let mut download_done = false;
    let mut dl_req_seen = 0u32;
    let mut dl_prog_count = 0u32;
    let mut dl_last_state: Option<DownloadState> = None;
    let mut dl_trigger_err: Option<String> = None;
    let mut dl_confirm_err: Option<String> = None;
    let mut smoke_phase = 0u8;
    let mut smoke_end: Option<Instant> = None;
    let mut smoke_deadline: Option<Instant> = None;
    let mut observer_announced = false;
    let mut observer_released = config.observer_gate.is_none();
    let mut pending_observer_until: Option<Instant> = None;
    let mut find_issued = false;
    let mut find_ok = false;
    let mut download_issued = false;
    let mut nav_step = 0u8;
    let mut nav_ok = false;
    let mut devtools_opened = false;
    let mut devtools_closed = false;
    let mut native_tab1_hidden_ok = config.host_mode != HostMode::NativeHwnd;
    let mut native_switch_back_ok = config.host_mode != HostMode::NativeHwnd;
    let mut native_switch_fwd_ok = config.host_mode != HostMode::NativeHwnd;

    let startup = Instant::now() + STARTUP_BUDGET;
    loop {
        if close.get() || Instant::now() >= startup {
            break;
        }
        let ready = rt_cell
            .borrow()
            .as_ref()
            .is_some_and(|r| r.state() == RuntimeState::Ready);
        if ready {
            break;
        }
        if let Err(err) = service(cx, &rt_cell, config.mode, &receiver, Some(startup)).await {
            eprintln!("{err}");
            break;
        }
    }

    let mut run_error: Option<String> = None;
    if !close.get() {
        match spawn_tab(
            cx,
            &rt_cell,
            root,
            &config,
            next_tab_id,
            START_URL.into(),
            &mut accounting,
            &mut tabs,
        ) {
            Ok(tab1_view) => {
                if config.context_menu
                    && let ShellView::Osr(ref tab1_osr) = tab1_view
                {
                    let limits = MenuLimits {
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
                    let mut svc = ContextMenuServices::new(
                        nz(MAX_SHELL_TABS),
                        nz(MAX_SHELL_TABS),
                        limits,
                        ctx_limits,
                    );
                    if let Ok(Ok(target)) =
                        root.update(cx, |_, window, cx| svc.register(tab1_osr, window, cx))
                        && let Some(rt) = rt_cell.borrow().as_ref()
                        && let Ok(reg) = rt.bind_context_menu(target)
                        && let Ok(Ok(pres)) = root.update(cx, |root, window, cx| {
                            let p =
                                DefaultMenuPresenter::new(tab1_osr, window, limits, ctx_limits, cx)
                                    .map(|p| Rc::new(RefCell::new(p)));
                            if let Ok(ref pr) = p {
                                root.menu_presenter = Some(pr.clone());
                            }
                            p
                        })
                    {
                        menus_owned = Some((svc, reg, pres, 1));
                    }
                }
                next_tab_id += 1;
            }
            Err(e) => run_error = Some(e),
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
            .update(cx, |root, _, _| root.model.quit_requested)
            .unwrap_or(true);
        if quit_from_tabs {
            break;
        }

        // Drain JS dialogs (default cancel in demo shell) and auth requests (default deny).
        for req in cap_queues
            .js_dialogs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            if let Some(rt) = rt_cell.borrow().as_ref() {
                let _ = rt.resolve_js_dialog(req.id, JsDialogDecision::Cancel);
            }
        }
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

        // Downloads
        while let Some(req) = cap_queues
            .downloads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
        {
            let clean_name = Path::new(&req.suggested_name)
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("download.bin")
                .to_owned();
            dl_req_seen = dl_req_seen.saturating_add(1);
            if config.smoke.is_some() {
                let dest = output_dir.join(format!("{}-{clean_name}", req.id.sequence));
                if let Some(rt) = rt_cell.borrow().as_ref() {
                    match rt.confirm_download(req.id, dest.clone()) {
                        Ok(()) => download_dest = Some(dest),
                        Err(err) => {
                            let msg = format!("{err:?}");
                            eprintln!("COMPONENT_DEMO_SHELL_DL_CONFIRM_ERR {msg}");
                            if dl_confirm_err.is_none() {
                                dl_confirm_err = Some(msg);
                            }
                        }
                    }
                }
            } else {
                let _ = root.update(cx, |root, _, cx| {
                    root.model.pending_download = Some(PendingDownloadPrompt {
                        id: req.id,
                        url: req.url,
                        suggested_name: clean_name,
                    });
                    cx.notify();
                });
            }
        }
        while let Some(prog) = cap_queues
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
        {
            dl_prog_count = dl_prog_count.saturating_add(1);
            dl_last_state = Some(prog.state);
            if config.smoke.is_some()
                && prog.state == DownloadState::Complete
                && prog.received_bytes > 0
                && download_dest.as_ref().is_some_and(|p| {
                    std::fs::read(p).ok().as_deref()
                        == Some(b"GPUI-CEF component demo download payload\n".as_slice())
                })
            {
                download_done = true;
            }
            let _ = root.update(cx, |root, _, cx| {
                let pending_name = root
                    .model
                    .pending_download
                    .as_ref()
                    .filter(|p| p.id == prog.id)
                    .map(|p| p.suggested_name.clone());
                if pending_name.is_some() {
                    root.model.pending_download = None;
                }
                root.model
                    .note_download_progress(pending_name.as_deref(), &prog);
                cx.notify();
            });
        }

        // Popups -> controlled new tab within policy and MAX_SHELL_TABS.
        while let Some(popup) = cap_queues
            .popups
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
        {
            let allow = root
                .update(cx, |root, _, _| root.model.can_accept_popup(&popup))
                .unwrap_or(false);
            if allow {
                let url = popup.target_url;
                let id = next_tab_id;
                next_tab_id += 1;
                if let Err(e) = spawn_tab(
                    cx,
                    &rt_cell,
                    root,
                    &config,
                    id,
                    url,
                    &mut accounting,
                    &mut tabs,
                ) {
                    run_error = Some(e);
                    break;
                }
            }
        }

        // Poll active PDF export ticket.
        if let Some(ticket) = active_pdf.as_ref() {
            match ticket.poll() {
                PdfExportStatus::Pending => {}
                PdfExportStatus::Completed(outcome) => {
                    let verified_pdf = outcome.ok
                        && pdf_path.as_ref().is_some_and(|p| {
                            std::fs::read(p).is_ok_and(|b| b.starts_with(b"%PDF-"))
                        });
                    pdf_completed |= verified_pdf;
                    let msg = format!("PDF #{} ok={verified_pdf}", outcome.id.sequence);
                    println!("COMPONENT_DEMO_SHELL_PDF ok={verified_pdf}");
                    let _ = root.update(cx, |root, _, cx| {
                        root.model.pdf_status = Some(msg);
                        cx.notify();
                    });
                    active_pdf = None;
                }
                PdfExportStatus::Failed(err) => {
                    let msg = format!("PDF failed: {err:?}");
                    let _ = root.update(cx, |root, _, cx| {
                        root.model.pdf_status = Some(msg);
                        cx.notify();
                    });
                    active_pdf = None;
                }
            }
        }

        // Drain UI actions.
        let pending_actions: Vec<ShellAction> = actions.borrow_mut().drain(..).collect();
        for action in pending_actions {
            match action {
                ShellAction::NewTab(opt_url) => {
                    let url = opt_url.unwrap_or_else(|| START_URL.into());
                    let id = next_tab_id;
                    next_tab_id += 1;
                    let _ = spawn_tab(
                        cx,
                        &rt_cell,
                        root,
                        &config,
                        id,
                        url,
                        &mut accounting,
                        &mut tabs,
                    );
                }
                ShellAction::SwitchTab(id) => {
                    if tabs.contains_key(&id) {
                        let _ = root.update(cx, |root, window, cx| {
                            root.model.switch_tab(id);
                            root.sync_native_tab_visibility(cx);
                            if let Some(view) = root.views.get(&id).cloned() {
                                let h = view.focus_handle(cx);
                                window.focus(&h, cx);
                            }
                            cx.notify();
                        });
                    }
                }
                ShellAction::CloseTab(id) => {
                    if let Err(e) =
                        close_one_tab(cx, &rt_cell, root, id, &mut accounting, &mut tabs)
                    {
                        run_error = Some(e);
                        break;
                    }
                }
                ShellAction::NavigateActive(raw) => {
                    if let Some(clean) = ShellModel::normalize_shell_url(&raw) {
                        if let Some(active_id) =
                            root.update(cx, |r, _, _| r.model.active_tab).ok().flatten()
                            && let Some(tab) = tabs.get(&active_id)
                        {
                            let _ = tab.handle.submit(BrowserCommand::Navigate(clean.clone()));
                            let _ = root.update(cx, |r, _, cx| {
                                r.model.address_draft = clean;
                                r.model.status_banner = "Navigating…".into();
                                cx.notify();
                            });
                        }
                    } else {
                        let _ = root.update(cx, |r, _, cx| {
                            r.model.status_banner =
                                "Blocked URL (only app://demo/*, data:*, about:blank allowed)"
                                    .into();
                            cx.notify();
                        });
                    }
                }
                ShellAction::Back => {
                    if let Some(id) = root.update(cx, |r, _, _| r.model.active_tab).ok().flatten()
                        && let Some(tab) = tabs.get(&id)
                    {
                        let _ = tab.handle.submit(BrowserCommand::Back);
                    }
                }
                ShellAction::Forward => {
                    if let Some(id) = root.update(cx, |r, _, _| r.model.active_tab).ok().flatten()
                        && let Some(tab) = tabs.get(&id)
                    {
                        let _ = tab.handle.submit(BrowserCommand::Forward);
                    }
                }
                ShellAction::Reload => {
                    if let Some(id) = root.update(cx, |r, _, _| r.model.active_tab).ok().flatten()
                        && let Some(tab) = tabs.get(&id)
                    {
                        let _ = tab.handle.submit(BrowserCommand::Reload);
                    }
                }
                ShellAction::StopLoading => {
                    if let Some(id) = root.update(cx, |r, _, _| r.model.active_tab).ok().flatten()
                        && let Some(tab) = tabs.get(&id)
                    {
                        let _ = tab.handle.submit(BrowserCommand::StopLoading);
                    }
                }
                ShellAction::ZoomSteps(steps) => {
                    let _ = root.update(cx, |root, window, cx| {
                        if let Some(active_id) = root.model.active_tab
                            && let Some(view) = root.views.get(&active_id).cloned()
                        {
                            view.step_zoom(steps, window, cx);
                            if let Some(milli) = view.zoom_level_milli(cx)
                                && let Some(meta) =
                                    root.model.tabs.iter_mut().find(|t| t.id == active_id)
                            {
                                meta.zoom_level_milli = milli;
                            }
                        }
                        cx.notify();
                    });
                }
                ShellAction::ZoomReset => {
                    let _ = root.update(cx, |root, window, cx| {
                        if let Some(active_id) = root.model.active_tab
                            && let Some(view) = root.views.get(&active_id).cloned()
                        {
                            view.reset_zoom(window, cx);
                            if let Some(meta) =
                                root.model.tabs.iter_mut().find(|t| t.id == active_id)
                            {
                                meta.zoom_level_milli = 0;
                            }
                        }
                        cx.notify();
                    });
                }
                ShellAction::ToggleMute => {
                    let _ = root.update(cx, |root, _, cx| {
                        if let Some(view) = root
                            .model
                            .active_tab
                            .and_then(|id| root.views.get(&id))
                            .cloned()
                        {
                            view.toggle_audio_muted(cx);
                        }
                        cx.notify();
                    });
                }
                ShellAction::ToggleFind => {
                    let _ = root.update(cx, |root, _, cx| {
                        root.model.find_open = !root.model.find_open;
                        root.model.find_focused = root.model.find_open;
                        root.model.address_focused = false;
                        cx.notify();
                    });
                }
                ShellAction::FindStep { forward } => {
                    let target = root
                        .update(cx, |root, _, cx| {
                            let query = root.model.find_query.clone();
                            if query.is_empty() {
                                root.model.find_last_result = Some("empty query".into());
                                cx.notify();
                                return None;
                            }
                            let view = root
                                .model
                                .active_tab
                                .and_then(|id| root.views.get(&id))
                                .cloned()?;
                            Some((query, view))
                        })
                        .ok()
                        .flatten();
                    if let Some((query, view)) = target {
                        let escaped = escape_js_string(&query);
                        let script = format!("window.find({escaped}, false, {}, true)", !forward);
                        let _ = cx.update(|cx| {
                            view.evaluate_script(script, JsFrameScope::MainFrame, 2000, cx);
                        });
                    }
                }
                ShellAction::CloseFind => {
                    let _ = root.update(cx, |root, _, cx| {
                        root.model.find_open = false;
                        root.model.find_focused = false;
                        root.model.find_last_result = None;
                        cx.notify();
                    });
                }
                ShellAction::ConfirmPendingDownload { accept } => {
                    let prompt = root
                        .update(cx, |root, _, cx| {
                            let p = root.model.pending_download.take();
                            cx.notify();
                            p
                        })
                        .ok()
                        .flatten();
                    if let Some(p) = prompt
                        && let Some(rt) = rt_cell.borrow().as_ref()
                    {
                        if accept {
                            let dest =
                                output_dir.join(format!("{}-{}", p.id.sequence, p.suggested_name));
                            let _ = rt.confirm_download(p.id, dest);
                        } else {
                            let _ = rt.cancel_download(p.id);
                        }
                    }
                }
                ShellAction::ExportPdf => {
                    if let Some(active_id) =
                        root.update(cx, |r, _, _| r.model.active_tab).ok().flatten()
                        && let Some(tab) = tabs.get(&active_id)
                        && let Some(rt) = rt_cell.borrow().as_ref()
                    {
                        let pdf_path = output_dir.join(format!("shell-tab-{active_id}.pdf"));
                        match rt.print_to_pdf(tab.browser_id, pdf_path, PdfExportOptions::default())
                        {
                            Ok(ticket) => {
                                active_pdf = Some(ticket);
                                let _ = root.update(cx, |r, _, cx| {
                                    r.model.pdf_status = Some("PDF: exporting…".into());
                                    cx.notify();
                                });
                            }
                            Err(err) => {
                                let _ = root.update(cx, |r, _, cx| {
                                    r.model.pdf_status = Some(format!("PDF err: {err:?}"));
                                    cx.notify();
                                });
                            }
                        }
                    }
                }
                ShellAction::ToggleDevTools => {
                    if let Some(active_id) =
                        root.update(cx, |r, _, _| r.model.active_tab).ok().flatten()
                        && let Some(tab) = tabs.get(&active_id)
                    {
                        let currently_open = root
                            .update(cx, |r, _, _| {
                                r.model.active_meta().is_some_and(|m| m.devtools_open)
                            })
                            .unwrap_or(false);
                        if let Some(rt) = rt_cell.borrow().as_ref() {
                            let ok = if currently_open {
                                rt.close_dev_tools(tab.browser_id).is_ok()
                            } else {
                                rt.open_devtools(tab.browser_id).is_ok()
                            };
                            if ok {
                                let _ = root.update(cx, |r, _, cx| {
                                    if let Some(meta) = r.model.active_meta_mut() {
                                        meta.devtools_open = !currently_open;
                                    }
                                    cx.notify();
                                });
                            }
                        }
                    }
                }
            }
        }

        // Consume view frames + sync per-tab navigation snapshots.
        let tab_ids: Vec<u64> = tabs.keys().copied().collect();
        for tab_id in tab_ids {
            let Some(tab) = tabs.get_mut(&tab_id) else {
                continue;
            };
            if tab.handle.snapshot().state == SessionState::Closed {
                let _ = close_one_tab(cx, &rt_cell, root, tab_id, &mut accounting, &mut tabs);
                continue;
            }
            let view = root
                .update(cx, |root, _, _| root.views.get(&tab_id).cloned())
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
                    HostMode::Osr => {
                        if let Some(d) = diag
                            && d.paint_calls > 0
                            && d.painted_sequence > 0
                            && d.frame_width > 0
                        {
                            tab.first_paint = true;
                            println!(
                                "COMPONENT_DEMO_SHELL_PAINT tab={} browser={} seq={}",
                                tab_id,
                                tab.browser_id.local(),
                                d.painted_sequence
                            );
                        }
                    }
                    HostMode::NativeHwnd => {
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
                                "COMPONENT_DEMO_SHELL_NATIVE_READY tab={} browser={} child_hwnd=0x{:x}",
                                tab_id,
                                tab.browser_id.local(),
                                hwnd.get()
                            );
                        }
                    }
                }
            }
            let _ = root.update(cx, |root, _, cx| {
                root.model
                    .apply_nav_snapshot(tab_id, &tab.handle.snapshot().navigation);
                if let Some(ShellView::Osr(v)) = root.views.get(&tab_id)
                    && let Ok(milli) = zoom_level_to_milli(v.read(cx).zoom_level())
                    && let Some(meta) = root.model.tabs.iter_mut().find(|t| t.id == tab_id)
                {
                    meta.zoom_level_milli = milli;
                }
                cx.notify();
            });
        }

        if let Some((ref mut svc, _, ref pres, menu_tab_id)) = menus_owned
            && let Some(tab) = tabs.get(&menu_tab_id)
        {
            let _ = root.update(cx, |_, window, cx| {
                let _ = svc.dispatch(tab.browser_id, window, cx, |update, window, cx| {
                    pres.borrow_mut().update(&update, window, cx)
                });
            });
        }

        // Smoke orchestration for `--browser-shell true --smoke-ms ...`
        if let Some(smoke_dur) = config.smoke {
            if !observer_released && let Some(gate) = config.observer_gate.as_ref() {
                if !observer_announced && tabs.get(&1).is_some_and(|t| t.first_paint) {
                    if let Err(err) = announce_shell_gate(gate) {
                        run_error = Some(err);
                        break;
                    }
                    observer_announced = true;
                    pending_observer_until = Some(Instant::now() + STARTUP_BUDGET);
                } else if let Some(until) = pending_observer_until {
                    if gate.is_file() {
                        match std::fs::read_to_string(gate) {
                            Ok(txt) if txt.trim() == "release" => {
                                observer_released = true;
                                pending_observer_until = None;
                            }
                            Ok(_) => {
                                run_error = Some("invalid shell smoke observer release".into());
                                break;
                            }
                            Err(err) => {
                                run_error = Some(format!("shell observer release: {err}"));
                                break;
                            }
                        }
                    } else if Instant::now() >= until {
                        run_error = Some("shell smoke observer timed out".into());
                        break;
                    }
                }
            } else if smoke_phase == 0 && tabs.get(&1).is_some_and(|t| t.first_paint) {
                smoke_phase = 1;
                smoke_deadline = Some(Instant::now() + Duration::from_secs(15));
                let id = next_tab_id;
                next_tab_id += 1;
                if let Err(e) = spawn_tab(
                    cx,
                    &rt_cell,
                    root,
                    &config,
                    id,
                    PAGE2_URL.into(),
                    &mut accounting,
                    &mut tabs,
                ) {
                    run_error = Some(e);
                    break;
                }
            } else if smoke_phase >= 1 {
                if !find_issued
                    && (config.host_mode == HostMode::Osr || native_switch_back_ok)
                    && tabs.get(&1).is_some_and(|t| {
                        let s = t.handle.snapshot();
                        t.first_paint
                            && !s.navigation.is_loading
                            && s.navigation.address == START_URL
                    })
                {
                    find_issued = true;
                    let view = root
                        .update(cx, |root, _, _| root.views.get(&1).cloned())
                        .ok()
                        .flatten();
                    if let Some(view) = view {
                        let _ = cx.update(|cx| {
                            view.evaluate_script(
                                "window.find(\"GPUI CEF Shell\", false, false, true)",
                                JsFrameScope::MainFrame,
                                4000,
                                cx,
                            );
                        });
                    }
                }
                find_ok = root.update(cx, |r, _, _| r.smoke_find_ok).unwrap_or(false);
                if find_ok
                    && !download_issued
                    && let Some(tab1) = tabs.get(&1)
                {
                    if let Err(err) = tab1
                        .handle
                        .submit(BrowserCommand::Navigate(DOWNLOAD_URL.into()))
                    {
                        let msg = format!("{err:?}");
                        eprintln!("COMPONENT_DEMO_SHELL_DL_TRIGGER_ERR {msg}");
                        dl_trigger_err = Some(msg);
                    }
                    download_issued = true;
                }
                if config.host_mode == HostMode::NativeHwnd
                    && find_ok
                    && download_done
                    && !native_switch_fwd_ok
                {
                    native_switch_fwd_ok = root
                        .update(cx, |r, _, cx| {
                            r.model.switch_tab(2);
                            r.sync_native_tab_visibility(cx);
                            cx.notify();
                            let t1 = r.views.get(&1).is_some_and(|v| v.is_native_visible(cx));
                            let t2 = r.views.get(&2).is_some_and(|v| v.is_native_visible(cx));
                            !t1 && t2
                        })
                        .unwrap_or(false);
                    if native_switch_fwd_ok {
                        println!("COMPONENT_DEMO_SHELL_NATIVE_SWITCH_OK");
                    }
                }
                if smoke_phase == 1 && tabs.get(&2).is_some_and(|t| t.first_paint) {
                    smoke_phase = 2;
                    if config.host_mode == HostMode::NativeHwnd {
                        let (hidden_ok, switched_ok) = root
                            .update(cx, |r, _, cx| {
                                let t1_before =
                                    r.views.get(&1).is_some_and(|v| v.is_native_visible(cx));
                                let t2_before =
                                    r.views.get(&2).is_some_and(|v| v.is_native_visible(cx));
                                r.model.switch_tab(1);
                                r.sync_native_tab_visibility(cx);
                                cx.notify();
                                let t1_after =
                                    r.views.get(&1).is_some_and(|v| v.is_native_visible(cx));
                                let t2_after =
                                    r.views.get(&2).is_some_and(|v| v.is_native_visible(cx));
                                (!t1_before && t2_before, t1_after && !t2_after)
                            })
                            .unwrap_or((false, false));
                        native_tab1_hidden_ok = hidden_ok;
                        native_switch_back_ok = switched_ok;
                    }
                    if let Some(tab2) = tabs.get(&2) {
                        let _ = tab2
                            .handle
                            .submit(BrowserCommand::Navigate(POPUP_URL.into()));
                        nav_step = 1;
                    }
                    if let Some(tab1) = tabs.get(&1)
                        && let Some(rt) = rt_cell.borrow().as_ref()
                    {
                        let out = output_dir.join("shell-smoke.pdf");
                        pdf_path = Some(out.clone());
                        active_pdf = rt
                            .print_to_pdf(tab1.browser_id, out, PdfExportOptions::default())
                            .ok();
                        if rt.open_devtools(tab1.browser_id).is_ok() {
                            devtools_opened = true;
                            let _ = root.update(cx, |r, _, _| {
                                if let Some(m) = r.model.tabs.iter_mut().find(|t| t.id == 1) {
                                    m.devtools_open = true;
                                }
                            });
                        }
                    }
                    smoke_end = Some(Instant::now() + smoke_dur);
                } else if smoke_phase == 2 {
                    if let Some(tab2) = tabs.get(&2) {
                        let snap = tab2.handle.snapshot();
                        if nav_step == 1
                            && !snap.navigation.is_loading
                            && snap.navigation.address == POPUP_URL
                        {
                            let _ = tab2.handle.submit(BrowserCommand::Back);
                            nav_step = 2;
                        } else if nav_step == 2
                            && !snap.navigation.is_loading
                            && snap.navigation.address == PAGE2_URL
                        {
                            nav_ok = true;
                        }
                    }
                    if nav_ok
                        && find_ok
                        && download_done
                        && pdf_completed
                        && devtools_opened
                        && native_tab1_hidden_ok
                        && native_switch_back_ok
                        && native_switch_fwd_ok
                        && smoke_end.is_some_and(|end| Instant::now() >= end)
                    {
                        if let Some(tab1) = tabs.get(&1)
                            && let Some(rt) = rt_cell.borrow().as_ref()
                        {
                            devtools_closed = rt.close_dev_tools(tab1.browser_id).is_ok();
                        }
                        break;
                    }
                }
                if smoke_deadline.is_some_and(|dl| Instant::now() >= dl) {
                    run_error = Some(format!(
                        "shell smoke verification timed out (host_mode={:?} phase={smoke_phase} nav_ok={nav_ok} find_ok={find_ok} download_done={download_done} pdf_completed={pdf_completed} devtools_opened={devtools_opened} native_tab1_hidden_ok={native_tab1_hidden_ok} native_switch_back_ok={native_switch_back_ok} native_switch_fwd_ok={native_switch_fwd_ok} dl_req_seen={dl_req_seen} dl_prog_count={dl_prog_count} dl_last_state={dl_last_state:?} dl_trigger_err={dl_trigger_err:?} dl_confirm_err={dl_confirm_err:?})",
                        config.host_mode
                    ));
                    break;
                }
            }
        }

        let next_deadline = if smoke_end.is_some() { smoke_end } else { None };
        if let Err(err) = service(cx, &rt_cell, config.mode, &receiver, next_deadline).await {
            run_error = Some(err);
            break;
        }
    }

    // Orderly all-tab teardown while the window is still open.
    drop(menus_owned);
    let remaining_ids: Vec<u64> = tabs.keys().copied().collect();
    for id in remaining_ids {
        let _ = close_one_tab(cx, &rt_cell, root, id, &mut accounting, &mut tabs);
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
                    eprintln!("shell finish_shutdown: {err:?}");
                    return 1;
                }
                break;
            }
            ShutdownStatus::Draining { .. } | ShutdownStatus::Blocked { .. } => {
                if Instant::now() >= deadline {
                    eprintln!("shell shutdown timed out");
                    std::mem::forget(rt);
                    return 1;
                }
            }
            other => {
                eprintln!("shell shutdown failed: {other:?}");
                std::mem::forget(rt);
                return 1;
            }
        }
        let step = if config.mode == ThreadMode::Integrated {
            PUMP_PERIOD
        } else {
            Duration::from_millis(25)
        };
        let timeout = deadline.saturating_duration_since(Instant::now()).min(step);
        let _ = wait(cx, &receiver, Some(timeout)).await;
        if config.mode == ThreadMode::Integrated {
            let _ = rt.pump();
        }
    }

    if let Some(err) = run_error {
        eprintln!("{err}");
        return 1;
    }

    let d = rt.diagnostics();
    let created = obs.created.load(Ordering::Acquire);
    let closed = obs.closed.load(Ordering::Acquire);
    let smoke_surface_ok = match config.host_mode {
        HostMode::Osr => accounting.created_images > 0,
        HostMode::NativeHwnd => {
            accounting.created_images == 0
                && native_tab1_hidden_ok
                && native_switch_back_ok
                && native_switch_fwd_ok
        }
    };
    if d.cef_shutdown_calls != 1
        || d.native_calls_after_shutdown != 0
        || created != closed
        || accounting.created_images != accounting.released_images
        || d.tasks.pending != 0
        || d.tasks.posted != d.tasks.completed
        || d.tasks.post_failed != 0
        || d.pending_requests != 0
        || d.pending_profiles != 0
        || d.pending_cookie_flushes != 0
        || d.profile_references_retained != 0
        || d.root_lease_held
        || d.app_reference_retained
        || (config.smoke.is_some()
            && (accounting.tabs_opened < 2
                || accounting.tabs_opened != accounting.tabs_closed
                || created < 2
                || !smoke_surface_ok
                || !nav_ok
                || !find_ok
                || !download_done
                || !pdf_completed
                || !devtools_opened
                || !devtools_closed))
    {
        eprintln!(
            "shell termination evidence incomplete: {d:?}, created={created}, closed={closed}, accounting={:?}",
            (
                accounting.tabs_opened,
                accounting.tabs_closed,
                accounting.created_images,
                accounting.released_images
            )
        );
        return 1;
    }

    println!(
        "COMPONENT_DEMO_SHELL_STOP tabs_opened={} tabs_closed={} nav_ok={} find_ok={} download_done={} pdf_done={} devtools_ok={}",
        accounting.tabs_opened,
        accounting.tabs_closed,
        nav_ok,
        find_ok,
        download_done,
        pdf_completed,
        devtools_opened && devtools_closed
    );
    println!(
        "COMPONENT_DEMO_STOP mode={:?} backend={:?} main={} ui={} shutdown_thread={} pump={} created={created} closed={closed} tasks={} images={} final_cpu=0 shutdown={} native_after={}",
        d.mode,
        d.observed_backend,
        d.main_thread_id,
        d.cef_ui_thread_id.unwrap_or(0),
        d.shutdown_thread_id.unwrap_or(0),
        d.manual_pump_count,
        d.tasks.completed,
        accounting.created_images,
        d.cef_shutdown_calls,
        d.native_calls_after_shutdown
    );
    0
}

pub(super) fn run(config: Config) -> i32 {
    let result = Rc::new(RefCell::new(None));
    let final_result = result.clone();
    gpui_platform::application().run(move |cx| {
        if config.context_menu {
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
        let root = match cx.open_window(
            WindowOptions {
                focus: false,
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(960.), px(640.)),
                    cx,
                ))),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("GPUI-CEF Browser Shell".into()),
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
                let host_mode = config.host_mode;
                cx.new(|cx| ShellRoot {
                    model: ShellModel {
                        status_banner: "Starting GPUI-CEF Browser Shell…".into(),
                        ..ShellModel::default()
                    },
                    host_mode,
                    views: BTreeMap::new(),
                    menu_presenter: None,
                    actions: actions_root,
                    wake: wake_root,
                    focus: cx.focus_handle(),
                    smoke_find_ok: false,
                })
            },
        ) {
            Ok(root) => root,
            Err(e) => {
                eprintln!("shell window: {e}");
                *final_result.borrow_mut() = Some(1);
                cx.quit();
                return;
            }
        };

        cx.spawn(async move |cx: &mut AsyncApp| {
            let code = drive_shell(cx, root, config, close, sender, receiver, actions).await;
            *final_result.borrow_mut() = Some(code);
            allow_close.set(true);
            cx.update(|app: &mut App| app.quit());
        })
        .detach();
    });
    result.take().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::contracts::RuntimeGeneration;
    use rust_cef_core::host_capabilities::PopupDisposition;

    #[test]
    fn url_normalization_enforces_confined_origins_and_rejects_untrusted() {
        assert_eq!(
            ShellModel::normalize_shell_url("  app://demo/page2.html  "),
            Some("app://demo/page2.html".into())
        );
        assert_eq!(
            ShellModel::normalize_shell_url("about:blank"),
            Some("about:blank".into())
        );
        assert_eq!(
            ShellModel::normalize_shell_url("data:text/html,<h1>Hi</h1>"),
            Some("data:text/html,<h1>Hi</h1>".into())
        );
        assert_eq!(
            ShellModel::normalize_shell_url("page2.html"),
            Some("app://demo/page2.html".into())
        );
        assert_eq!(ShellModel::normalize_shell_url(""), None);
        assert_eq!(ShellModel::normalize_shell_url("javascript:alert(1)"), None);
        assert_eq!(
            ShellModel::normalize_shell_url("https://evil.example"),
            None
        );
        assert_eq!(ShellModel::normalize_shell_url("app://demo/bad\n"), None);
    }

    #[test]
    fn tab_lifecycle_cap_switch_neighbor_selection_and_quit() {
        let mut model = ShellModel::default();
        for id in 1..=MAX_SHELL_TABS as u64 {
            assert!(model.add_tab(id, format!("app://demo/{id}.html")));
        }
        assert!(!model.add_tab(99, START_URL.into()));
        assert_eq!(model.active_tab, Some(MAX_SHELL_TABS as u64));

        assert!(model.switch_tab(2));
        assert_eq!(model.address_draft, "app://demo/2.html");
        assert!(!model.switch_tab(999));

        assert!(model.close_tab(2));
        assert_eq!(model.active_tab, Some(3));
        assert!(!model.quit_requested);

        while let Some(id) = model.active_tab {
            assert!(model.close_tab(id));
        }
        assert!(model.quit_requested);
        assert!(model.tabs.is_empty());
    }

    #[test]
    fn popup_gating_nav_snapshot_download_progress_and_js_escape() {
        let mut model = ShellModel::default();
        assert!(model.add_tab(1, START_URL.into()));

        let browser = BrowserId::new(RuntimeGeneration::new(1), 1);
        assert!(model.can_accept_popup(&PopupRequest {
            browser,
            target_url: POPUP_URL.into(),
            target_frame_name: "_blank".into(),
            disposition: PopupDisposition::NewForegroundTab,
            user_gesture: true,
            policy_allowed: true,
        }));
        assert!(!model.can_accept_popup(&PopupRequest {
            browser,
            target_url: "https://blocked.example".into(),
            target_frame_name: String::new(),
            disposition: PopupDisposition::NewPopup,
            user_gesture: false,
            policy_allowed: false,
        }));

        let nav = NavigationSnapshot {
            address: PAGE2_URL.into(),
            title: "Page 2".into(),
            is_loading: false,
            can_go_back: true,
            can_go_forward: false,
            loading_progress_per_mille: 1500,
            zoom_level_milli: 500,
            audio_muted: true,
            ..NavigationSnapshot::default()
        };
        model.apply_nav_snapshot(1, &nav);
        assert_eq!(model.address_draft, PAGE2_URL);
        let active = model.active_meta().unwrap();
        assert_eq!(active.title, "Page 2");
        assert!(active.can_go_back);
        assert_eq!(active.loading_progress_per_mille, 1000);
        assert!(active.audio_muted);

        let dl_id = DownloadId {
            browser,
            sequence: 1,
            cef_id: 7,
        };
        model.note_download_progress(
            Some("sample.txt"),
            &DownloadProgress {
                id: dl_id,
                state: DownloadState::InProgress,
                received_bytes: 12,
                total_bytes: Some(40),
                current_speed: 100,
                percent_complete: 30,
                full_path: None,
            },
        );
        model.note_download_progress(
            None,
            &DownloadProgress {
                id: dl_id,
                state: DownloadState::Complete,
                received_bytes: 40,
                total_bytes: Some(40),
                current_speed: 0,
                percent_complete: 100,
                full_path: Some("D:\\out\\1-sample.txt".into()),
            },
        );
        assert_eq!(model.active_downloads.len(), 1);
        assert_eq!(model.active_downloads[0].state, DownloadState::Complete);
        assert_eq!(model.active_downloads[0].name, "1-sample.txt");

        assert_eq!(escape_js_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
        shell_demo_settings()
            .validate()
            .expect("valid shell demo settings");
    }
}
