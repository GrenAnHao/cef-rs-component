//! Unified CefSharp-equivalent per-view callback registry and event payload
//! types shared by [`crate::CefBrowser`] and [`crate::NativeCefBrowser`].
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use gpui::{Context, Window};
use rust_cef_core::{
    contracts::{
        BrowserCursor, ConsoleSeverity, DraggableRegionRect, FocusedDomNodeInfo,
        FrameLifecycleInfo, JsUncaughtExceptionInfo, ResourceLoadCompleteInfo,
        ResourceResponseInfo,
    },
    ports::{
        BrowserEvent, EXT_EVENT_AUTO_RESIZE, EXT_EVENT_CONSOLE, EXT_EVENT_DOCUMENT_AVAILABLE,
        EXT_EVENT_DRAGGABLE_REGIONS, EXT_EVENT_FOCUSED_NODE, EXT_EVENT_FRAME_LIFECYCLE,
        EXT_EVENT_MEDIA_ACCESS, EXT_EVENT_RESOURCE_OBSERVE, EXT_EVENT_TOOLTIP,
        EXT_EVENT_UNCAUGHT_EXCEPTION, EXT_EVENT_V8_CONTEXT, EngineError, PolicyAction, RequestId,
    },
};

pub type BeforeUnloadRequestId = RequestId;
pub type ViewEventCallback<V, T> = Box<dyn FnMut(T, &mut V, &mut Window, &mut Context<V>)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadingStateChange {
    pub is_loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadStartInfo {
    pub is_main_frame: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadEndInfo {
    pub is_main_frame: bool,
    pub http_status_code: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadErrorInfo {
    pub is_main_frame: bool,
    pub error_code: i32,
    pub cancelled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NavigationRequestEvent {
    pub is_main_frame: bool,
    pub is_redirect: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsoleMessageInfo {
    pub severity: ConsoleSeverity,
    pub message: String,
    pub line: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BeforeUnloadInfo {
    pub request: BeforeUnloadRequestId,
    pub is_reload: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RendererTerminatedInfo {
    pub native_status: i32,
    pub error_code: Option<i32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutoResizeInfo {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaAccessInfo {
    pub has_video_access: bool,
    pub has_audio_access: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameAttachedInfo {
    pub frame: FrameLifecycleInfo,
    pub reattached: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameDetachedInfo {
    pub frame_id: String,
    pub is_main: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MainFrameChangedInfo {
    pub old_frame_id: Option<String>,
    pub new_frame: Option<FrameLifecycleInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V8ContextLifecycleInfo {
    pub frame_id: String,
    pub is_main_frame: bool,
}

pub(crate) struct CallbackSlot<V, T> {
    cb: Option<ViewEventCallback<V, T>>,
    epoch: u64,
}

impl<V, T> Default for CallbackSlot<V, T> {
    fn default() -> Self {
        Self { cb: None, epoch: 0 }
    }
}

impl<V, T> CallbackSlot<V, T> {
    pub(crate) fn set(&mut self, handler: Option<ViewEventCallback<V, T>>) {
        self.epoch = self.epoch.wrapping_add(1);
        self.cb = handler;
    }

    pub(crate) fn is_some(&self) -> bool {
        self.cb.is_some()
    }

    pub(crate) fn clear(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.cb = None;
    }

    pub(crate) fn take_with_epoch(&mut self) -> Option<(ViewEventCallback<V, T>, u64)> {
        self.cb.take().map(|cb| (cb, self.epoch))
    }

    pub(crate) fn restore_if_unchanged(&mut self, cb: ViewEventCallback<V, T>, epoch: u64) {
        if self.epoch == epoch && self.cb.is_none() {
            self.cb = Some(cb);
        }
    }
}

pub(crate) struct BrowserViewCallbacks<V> {
    pub(crate) on_browser_created: CallbackSlot<V, ()>,
    pub(crate) on_browser_closed: CallbackSlot<V, ()>,
    pub(crate) on_create_failed: CallbackSlot<V, EngineError>,
    pub(crate) on_close_cancelled: CallbackSlot<V, ()>,
    pub(crate) on_before_unload: CallbackSlot<V, BeforeUnloadInfo>,
    pub(crate) on_got_focus: CallbackSlot<V, ()>,
    pub(crate) on_title_changed: CallbackSlot<V, String>,
    pub(crate) on_loading_state_changed: CallbackSlot<V, LoadingStateChange>,
    pub(crate) on_load_started: CallbackSlot<V, LoadStartInfo>,
    pub(crate) on_load_finished: CallbackSlot<V, LoadEndInfo>,
    pub(crate) on_load_error: CallbackSlot<V, LoadErrorInfo>,
    pub(crate) on_navigation_requested: CallbackSlot<V, NavigationRequestEvent>,
    pub(crate) on_loading_progress_changed: CallbackSlot<V, f64>,
    pub(crate) on_document_available_in_main_frame: CallbackSlot<V, ()>,
    pub(crate) on_tooltip_changed: CallbackSlot<V, String>,
    pub(crate) on_status_message: CallbackSlot<V, String>,
    pub(crate) on_favicon_urls_changed: CallbackSlot<V, Vec<String>>,
    pub(crate) on_fullscreen_mode_changed: CallbackSlot<V, bool>,
    pub(crate) on_console_message: CallbackSlot<V, ConsoleMessageInfo>,
    pub(crate) on_cursor_changed: CallbackSlot<V, BrowserCursor>,
    pub(crate) on_auto_resize: CallbackSlot<V, AutoResizeInfo>,
    pub(crate) on_media_access_changed: CallbackSlot<V, MediaAccessInfo>,
    pub(crate) on_zoom_level_changed: CallbackSlot<V, f64>,
    pub(crate) on_audio_muted_changed: CallbackSlot<V, bool>,
    pub(crate) on_draggable_regions_changed: CallbackSlot<V, Vec<DraggableRegionRect>>,
    pub(crate) on_frame_created: CallbackSlot<V, FrameLifecycleInfo>,
    pub(crate) on_frame_attached: CallbackSlot<V, FrameAttachedInfo>,
    pub(crate) on_frame_detached: CallbackSlot<V, FrameDetachedInfo>,
    pub(crate) on_main_frame_changed: CallbackSlot<V, MainFrameChangedInfo>,
    pub(crate) on_context_created: CallbackSlot<V, V8ContextLifecycleInfo>,
    pub(crate) on_context_released: CallbackSlot<V, V8ContextLifecycleInfo>,
    pub(crate) on_focused_node_changed: CallbackSlot<V, Option<FocusedDomNodeInfo>>,
    pub(crate) on_uncaught_exception: CallbackSlot<V, JsUncaughtExceptionInfo>,
    pub(crate) on_resource_response: CallbackSlot<V, ResourceResponseInfo>,
    pub(crate) on_resource_load_complete: CallbackSlot<V, ResourceLoadCompleteInfo>,
    pub(crate) on_render_view_ready: CallbackSlot<V, ()>,
    pub(crate) on_renderer_terminated: CallbackSlot<V, RendererTerminatedInfo>,
    pub(crate) on_render_process_responsive_changed: CallbackSlot<V, bool>,
    pub(crate) on_policy_denied: CallbackSlot<V, PolicyAction>,
}

impl<V> Default for BrowserViewCallbacks<V> {
    fn default() -> Self {
        Self {
            on_browser_created: CallbackSlot::default(),
            on_browser_closed: CallbackSlot::default(),
            on_create_failed: CallbackSlot::default(),
            on_close_cancelled: CallbackSlot::default(),
            on_before_unload: CallbackSlot::default(),
            on_got_focus: CallbackSlot::default(),
            on_title_changed: CallbackSlot::default(),
            on_loading_state_changed: CallbackSlot::default(),
            on_load_started: CallbackSlot::default(),
            on_load_finished: CallbackSlot::default(),
            on_load_error: CallbackSlot::default(),
            on_navigation_requested: CallbackSlot::default(),
            on_loading_progress_changed: CallbackSlot::default(),
            on_document_available_in_main_frame: CallbackSlot::default(),
            on_tooltip_changed: CallbackSlot::default(),
            on_status_message: CallbackSlot::default(),
            on_favicon_urls_changed: CallbackSlot::default(),
            on_fullscreen_mode_changed: CallbackSlot::default(),
            on_console_message: CallbackSlot::default(),
            on_cursor_changed: CallbackSlot::default(),
            on_auto_resize: CallbackSlot::default(),
            on_media_access_changed: CallbackSlot::default(),
            on_zoom_level_changed: CallbackSlot::default(),
            on_audio_muted_changed: CallbackSlot::default(),
            on_draggable_regions_changed: CallbackSlot::default(),
            on_frame_created: CallbackSlot::default(),
            on_frame_attached: CallbackSlot::default(),
            on_frame_detached: CallbackSlot::default(),
            on_main_frame_changed: CallbackSlot::default(),
            on_context_created: CallbackSlot::default(),
            on_context_released: CallbackSlot::default(),
            on_focused_node_changed: CallbackSlot::default(),
            on_uncaught_exception: CallbackSlot::default(),
            on_resource_response: CallbackSlot::default(),
            on_resource_load_complete: CallbackSlot::default(),
            on_render_view_ready: CallbackSlot::default(),
            on_renderer_terminated: CallbackSlot::default(),
            on_render_process_responsive_changed: CallbackSlot::default(),
            on_policy_denied: CallbackSlot::default(),
        }
    }
}

impl<V> BrowserViewCallbacks<V> {
    pub(crate) fn extended_event_mask(&self) -> u32 {
        let mut mask = 0u32;
        if self.on_tooltip_changed.is_some() {
            mask |= EXT_EVENT_TOOLTIP;
        }
        if self.on_console_message.is_some() {
            mask |= EXT_EVENT_CONSOLE;
        }
        if self.on_auto_resize.is_some() {
            mask |= EXT_EVENT_AUTO_RESIZE;
        }
        if self.on_media_access_changed.is_some() {
            mask |= EXT_EVENT_MEDIA_ACCESS;
        }
        if self.on_document_available_in_main_frame.is_some() {
            mask |= EXT_EVENT_DOCUMENT_AVAILABLE;
        }
        if self.on_draggable_regions_changed.is_some() {
            mask |= EXT_EVENT_DRAGGABLE_REGIONS;
        }
        if self.on_frame_created.is_some()
            || self.on_frame_attached.is_some()
            || self.on_frame_detached.is_some()
            || self.on_main_frame_changed.is_some()
        {
            mask |= EXT_EVENT_FRAME_LIFECYCLE;
        }
        if self.on_context_created.is_some() || self.on_context_released.is_some() {
            mask |= EXT_EVENT_V8_CONTEXT;
        }
        if self.on_focused_node_changed.is_some() {
            mask |= EXT_EVENT_FOCUSED_NODE;
        }
        if self.on_uncaught_exception.is_some() {
            mask |= EXT_EVENT_UNCAUGHT_EXCEPTION;
        }
        if self.on_resource_response.is_some() || self.on_resource_load_complete.is_some() {
            mask |= EXT_EVENT_RESOURCE_OBSERVE;
        }
        mask
    }

    pub(crate) fn clear_all(&mut self) {
        self.on_browser_created.clear();
        self.on_browser_closed.clear();
        self.on_create_failed.clear();
        self.on_close_cancelled.clear();
        self.on_before_unload.clear();
        self.on_got_focus.clear();
        self.on_title_changed.clear();
        self.on_loading_state_changed.clear();
        self.on_load_started.clear();
        self.on_load_finished.clear();
        self.on_load_error.clear();
        self.on_navigation_requested.clear();
        self.on_loading_progress_changed.clear();
        self.on_document_available_in_main_frame.clear();
        self.on_tooltip_changed.clear();
        self.on_status_message.clear();
        self.on_favicon_urls_changed.clear();
        self.on_fullscreen_mode_changed.clear();
        self.on_console_message.clear();
        self.on_cursor_changed.clear();
        self.on_auto_resize.clear();
        self.on_media_access_changed.clear();
        self.on_zoom_level_changed.clear();
        self.on_audio_muted_changed.clear();
        self.on_draggable_regions_changed.clear();
        self.on_frame_created.clear();
        self.on_frame_attached.clear();
        self.on_frame_detached.clear();
        self.on_main_frame_changed.clear();
        self.on_context_created.clear();
        self.on_context_released.clear();
        self.on_focused_node_changed.clear();
        self.on_uncaught_exception.clear();
        self.on_resource_response.clear();
        self.on_resource_load_complete.clear();
        self.on_render_view_ready.clear();
        self.on_renderer_terminated.clear();
        self.on_render_process_responsive_changed.clear();
        self.on_policy_denied.clear();
    }
}

fn invoke_slot<V: 'static, T>(
    view: &mut V,
    payload: T,
    window: &mut Window,
    cx: &mut Context<V>,
    callbacks_mut: fn(&mut V) -> &mut BrowserViewCallbacks<V>,
    select_slot: fn(&mut BrowserViewCallbacks<V>) -> &mut CallbackSlot<V, T>,
) {
    let taken = {
        let callbacks = callbacks_mut(view);
        select_slot(callbacks).take_with_epoch()
    };
    let Some((mut cb, epoch)) = taken else {
        return;
    };
    cb(payload, view, window, cx);
    let callbacks = callbacks_mut(view);
    select_slot(callbacks).restore_if_unchanged(cb, epoch);
}

macro_rules! dispatch_to {
    ($view:expr, $payload:expr, $window:expr, $cx:expr, $callbacks_mut:expr, $field:ident) => {
        invoke_slot(
            $view,
            $payload,
            $window,
            $cx,
            $callbacks_mut,
            |c| &mut c.$field,
        )
    };
}

pub(crate) fn dispatch_single_view_event<V: 'static>(
    view: &mut V,
    event: BrowserEvent,
    window: &mut Window,
    cx: &mut Context<V>,
    callbacks_mut: fn(&mut V) -> &mut BrowserViewCallbacks<V>,
) {
    match event {
        BrowserEvent::Created => dispatch_to!(view, (), window, cx, callbacks_mut, on_browser_created),
        BrowserEvent::Closed => dispatch_to!(view, (), window, cx, callbacks_mut, on_browser_closed),
        BrowserEvent::CreateFailed(err) => {
            dispatch_to!(view, err, window, cx, callbacks_mut, on_create_failed)
        }
        BrowserEvent::CloseCancelled => {
            dispatch_to!(view, (), window, cx, callbacks_mut, on_close_cancelled)
        }
        BrowserEvent::BeforeUnloadRequested { request, is_reload } => dispatch_to!(
            view,
            BeforeUnloadInfo { request, is_reload },
            window,
            cx,
            callbacks_mut,
            on_before_unload
        ),
        BrowserEvent::GotFocus => {
            dispatch_to!(view, (), window, cx, callbacks_mut, on_got_focus)
        }
        BrowserEvent::TitleChanged(title) => {
            dispatch_to!(view, title, window, cx, callbacks_mut, on_title_changed)
        }
        BrowserEvent::LoadingChanged {
            loading,
            can_go_back,
            can_go_forward,
        } => dispatch_to!(
            view,
            LoadingStateChange {
                is_loading: loading,
                can_go_back,
                can_go_forward,
            },
            window,
            cx,
            callbacks_mut,
            on_loading_state_changed
        ),
        BrowserEvent::LoadStarted { is_main_frame } => dispatch_to!(
            view,
            LoadStartInfo { is_main_frame },
            window,
            cx,
            callbacks_mut,
            on_load_started
        ),
        BrowserEvent::LoadFinished {
            is_main_frame,
            http_status_code,
        } => dispatch_to!(
            view,
            LoadEndInfo {
                is_main_frame,
                http_status_code,
            },
            window,
            cx,
            callbacks_mut,
            on_load_finished
        ),
        BrowserEvent::LoadFailed {
            is_main_frame,
            code,
        } => dispatch_to!(
            view,
            LoadErrorInfo {
                is_main_frame,
                error_code: code,
                cancelled: false,
            },
            window,
            cx,
            callbacks_mut,
            on_load_error
        ),
        BrowserEvent::LoadCancelled {
            is_main_frame,
            code,
        } => dispatch_to!(
            view,
            LoadErrorInfo {
                is_main_frame,
                error_code: code,
                cancelled: true,
            },
            window,
            cx,
            callbacks_mut,
            on_load_error
        ),
        BrowserEvent::NavigationRequested {
            is_main_frame,
            is_redirect,
        } => dispatch_to!(
            view,
            NavigationRequestEvent {
                is_main_frame,
                is_redirect,
            },
            window,
            cx,
            callbacks_mut,
            on_navigation_requested
        ),
        BrowserEvent::LoadingProgressChanged(per_mille) => dispatch_to!(
            view,
            f64::from(per_mille) / 1000.0,
            window,
            cx,
            callbacks_mut,
            on_loading_progress_changed
        ),
        BrowserEvent::DocumentAvailableInMainFrame => dispatch_to!(
            view,
            (),
            window,
            cx,
            callbacks_mut,
            on_document_available_in_main_frame
        ),
        BrowserEvent::TooltipChanged(text) => {
            dispatch_to!(view, text, window, cx, callbacks_mut, on_tooltip_changed)
        }
        BrowserEvent::StatusMessageChanged(text) => {
            dispatch_to!(view, text, window, cx, callbacks_mut, on_status_message)
        }
        BrowserEvent::FaviconUrlsChanged(urls) => {
            dispatch_to!(view, urls, window, cx, callbacks_mut, on_favicon_urls_changed)
        }
        BrowserEvent::FullscreenModeChanged(fullscreen) => dispatch_to!(
            view,
            fullscreen,
            window,
            cx,
            callbacks_mut,
            on_fullscreen_mode_changed
        ),
        BrowserEvent::ConsoleMessage {
            severity,
            message,
            line,
        } => dispatch_to!(
            view,
            ConsoleMessageInfo {
                severity,
                message,
                line,
            },
            window,
            cx,
            callbacks_mut,
            on_console_message
        ),
        BrowserEvent::CursorChanged(cursor) => {
            dispatch_to!(view, cursor, window, cx, callbacks_mut, on_cursor_changed)
        }
        BrowserEvent::AutoResized { width, height } => dispatch_to!(
            view,
            AutoResizeInfo {
                width: width.max(0) as u32,
                height: height.max(0) as u32,
            },
            window,
            cx,
            callbacks_mut,
            on_auto_resize
        ),
        BrowserEvent::MediaAccessChanged {
            has_video,
            has_audio,
        } => dispatch_to!(
            view,
            MediaAccessInfo {
                has_video_access: has_video,
                has_audio_access: has_audio,
            },
            window,
            cx,
            callbacks_mut,
            on_media_access_changed
        ),
        BrowserEvent::ZoomLevelChanged(milli) => dispatch_to!(
            view,
            f64::from(milli) / 1000.0,
            window,
            cx,
            callbacks_mut,
            on_zoom_level_changed
        ),
        BrowserEvent::AudioMutedChanged(muted) => {
            dispatch_to!(view, muted, window, cx, callbacks_mut, on_audio_muted_changed)
        }
        BrowserEvent::DraggableRegionsChanged(regions) => dispatch_to!(
            view,
            regions,
            window,
            cx,
            callbacks_mut,
            on_draggable_regions_changed
        ),
        BrowserEvent::FrameCreated(info) => {
            dispatch_to!(view, info, window, cx, callbacks_mut, on_frame_created)
        }
        BrowserEvent::FrameAttached { frame, reattached } => dispatch_to!(
            view,
            FrameAttachedInfo { frame, reattached },
            window,
            cx,
            callbacks_mut,
            on_frame_attached
        ),
        BrowserEvent::FrameDetached { frame_id, is_main } => dispatch_to!(
            view,
            FrameDetachedInfo { frame_id, is_main },
            window,
            cx,
            callbacks_mut,
            on_frame_detached
        ),
        BrowserEvent::MainFrameChanged {
            old_frame_id,
            new_frame,
        } => dispatch_to!(
            view,
            MainFrameChangedInfo {
                old_frame_id,
                new_frame,
            },
            window,
            cx,
            callbacks_mut,
            on_main_frame_changed
        ),
        BrowserEvent::ContextCreated {
            frame_id,
            is_main_frame,
        } => dispatch_to!(
            view,
            V8ContextLifecycleInfo {
                frame_id,
                is_main_frame,
            },
            window,
            cx,
            callbacks_mut,
            on_context_created
        ),
        BrowserEvent::ContextReleased {
            frame_id,
            is_main_frame,
        } => dispatch_to!(
            view,
            V8ContextLifecycleInfo {
                frame_id,
                is_main_frame,
            },
            window,
            cx,
            callbacks_mut,
            on_context_released
        ),
        BrowserEvent::FocusedNodeChanged(node) => {
            dispatch_to!(view, node, window, cx, callbacks_mut, on_focused_node_changed)
        }
        BrowserEvent::UncaughtJsException(info) => {
            dispatch_to!(view, info, window, cx, callbacks_mut, on_uncaught_exception)
        }
        BrowserEvent::ResourceResponseObserved(info) => {
            dispatch_to!(view, info, window, cx, callbacks_mut, on_resource_response)
        }
        BrowserEvent::ResourceLoadCompleted(info) => dispatch_to!(
            view,
            info,
            window,
            cx,
            callbacks_mut,
            on_resource_load_complete
        ),
        BrowserEvent::RenderViewReady => {
            dispatch_to!(view, (), window, cx, callbacks_mut, on_render_view_ready)
        }
        BrowserEvent::RendererTerminated {
            native_status,
            code,
        } => dispatch_to!(
            view,
            RendererTerminatedInfo {
                native_status,
                error_code: code,
            },
            window,
            cx,
            callbacks_mut,
            on_renderer_terminated
        ),
        BrowserEvent::Unresponsive => dispatch_to!(
            view,
            false,
            window,
            cx,
            callbacks_mut,
            on_render_process_responsive_changed
        ),
        BrowserEvent::Responsive => dispatch_to!(
            view,
            true,
            window,
            cx,
            callbacks_mut,
            on_render_process_responsive_changed
        ),
        BrowserEvent::PolicyDenied { action } => {
            dispatch_to!(view, action, window, cx, callbacks_mut, on_policy_denied)
        }
        BrowserEvent::AddressChanged(_)
        | BrowserEvent::FocusTaken { .. }
        | BrowserEvent::HostShortcutTriggered { .. }
        | BrowserEvent::StartOutboundDrag(_)
        | BrowserEvent::OutboundDragCursorChanged(_)
        | BrowserEvent::AccessibilityTreeUpdated { .. }
        | BrowserEvent::PopupRequested(_)
        | BrowserEvent::CaptureFailed(_)
        | BrowserEvent::JsMessageReceived { .. }
        | BrowserEvent::EvalCompleted { .. } => {}
    }
}

macro_rules! impl_view_callback_setter {
    ($method:ident, $field:ident, $payload:ty) => {
        pub fn $method(
            &mut self,
            handler: Option<$crate::events::ViewEventCallback<Self, $payload>>,
        ) {
            self.callbacks.$field.set(handler);
            self.handle()
                .set_extended_event_mask(self.callbacks.extended_event_mask());
        }
    };
}

macro_rules! impl_view_surface_api {
    () => {
        pub fn is_browser_initialized(&self) -> bool {
            self.handle().snapshot().state
                == rust_cef_core::contracts::SessionState::Alive
        }

        pub fn address(&self) -> &str {
            &self.ux.address
        }

        pub fn title(&self) -> &str {
            &self.ux.title
        }

        pub fn is_loading(&self) -> bool {
            self.ux.is_loading
        }

        pub fn can_go_back(&self) -> bool {
            self.ux.can_go_back
        }

        pub fn can_go_forward(&self) -> bool {
            self.ux.can_go_forward
        }

        pub fn main_http_status(&self) -> Option<i32> {
            self.ux.main_http_status
        }

        pub fn last_main_frame_error(&self) -> Option<i32> {
            self.ux.last_main_frame_error
        }

        pub fn draggable_regions(
            &self,
        ) -> &[rust_cef_core::contracts::DraggableRegionRect] {
            &self.ux.draggable_regions
        }

        pub fn navigation_snapshot(&self) -> rust_cef_core::contracts::NavigationSnapshot {
            self.handle().snapshot().navigation
        }

        pub fn load_url(
            &mut self,
            url: impl Into<String>,
        ) -> Result<rust_cef_core::ports::CommandId, ViewError> {
            self.check_live()?;
            self.handle()
                .submit(rust_cef_core::ports::BrowserCommand::Navigate(url.into()))
                .map_err(ViewError::Input)
        }

        pub fn load_url_with_post_data(
            &mut self,
            url: impl Into<String>,
            post_data: Vec<u8>,
            content_type: Option<String>,
        ) -> Result<rust_cef_core::ports::CommandId, ViewError> {
            self.check_live()?;
            self.handle()
                .submit(rust_cef_core::ports::BrowserCommand::LoadUrlWithPostData {
                    url: url.into(),
                    post_data,
                    content_type,
                })
                .map_err(ViewError::Input)
        }

        pub fn go_back(&mut self) -> Result<rust_cef_core::ports::CommandId, ViewError> {
            self.check_live()?;
            self.handle()
                .submit(rust_cef_core::ports::BrowserCommand::Back)
                .map_err(ViewError::Input)
        }

        pub fn go_forward(&mut self) -> Result<rust_cef_core::ports::CommandId, ViewError> {
            self.check_live()?;
            self.handle()
                .submit(rust_cef_core::ports::BrowserCommand::Forward)
                .map_err(ViewError::Input)
        }

        pub fn reload(&mut self) -> Result<rust_cef_core::ports::CommandId, ViewError> {
            self.check_live()?;
            self.handle()
                .submit(rust_cef_core::ports::BrowserCommand::Reload)
                .map_err(ViewError::Input)
        }

        pub fn stop_loading(&mut self) -> Result<rust_cef_core::ports::CommandId, ViewError> {
            self.check_live()?;
            self.handle()
                .submit(rust_cef_core::ports::BrowserCommand::StopLoading)
                .map_err(ViewError::Input)
        }

        pub fn resolve_before_unload(
            &mut self,
            request: $crate::events::BeforeUnloadRequestId,
            allow: bool,
        ) -> Result<rust_cef_core::ports::CommandId, ViewError> {
            self.check_live()?;
            self.handle()
                .submit(rust_cef_core::ports::BrowserCommand::ResolveBeforeUnload {
                    request,
                    allow,
                })
                .map_err(ViewError::Input)
        }

        pub fn set_popup_policy(
            &mut self,
            policy: rust_cef_core::host_capabilities::PopupPolicy,
        ) {
            self.handle().set_popup_policy(policy);
        }

        pub fn popup_policy(&self) -> rust_cef_core::host_capabilities::PopupPolicy {
            self.handle().popup_policy()
        }

        pub fn set_popup_filter(
            &mut self,
            filter: Option<rust_cef_core::host_capabilities::PopupFilter>,
        ) {
            self.handle().set_popup_filter(filter);
        }

        pub fn set_navigation_filter(
            &mut self,
            filter: Option<rust_cef_core::host_capabilities::NavigationFilter>,
        ) {
            self.handle().set_navigation_filter(filter);
        }

        pub fn set_drag_enter_filter(
            &mut self,
            filter: Option<rust_cef_core::host_capabilities::DragEnterFilter>,
        ) {
            self.handle().set_drag_enter_filter(filter);
        }

        pub fn set_focus_filter(
            &mut self,
            filter: Option<rust_cef_core::host_capabilities::SetFocusFilter>,
        ) {
            self.handle().set_focus_filter(filter);
        }

        pub fn set_pre_key_filter(
            &mut self,
            filter: Option<rust_cef_core::host_capabilities::PreKeyFilter>,
        ) {
            self.handle().set_pre_key_filter(filter);
        }

        pub fn set_key_event_filter(
            &mut self,
            filter: Option<rust_cef_core::host_capabilities::KeyEventFilter>,
        ) {
            self.handle().set_key_event_filter(filter);
        }

        pub fn set_protocol_execution_filter(
            &mut self,
            filter: Option<rust_cef_core::host_capabilities::ProtocolExecutionFilter>,
        ) {
            self.handle().set_protocol_execution_filter(filter);
        }

        pub fn set_host_capabilities(
            &mut self,
            capabilities: Option<rust_cef_core::host_capabilities::HostCapabilities>,
        ) {
            self.handle().set_browser_host_capabilities(capabilities);
        }

        pub fn set_resource_interceptor(
            &mut self,
            interceptor: Option<
                std::sync::Arc<dyn rust_cef_core::custom_schemes::ResourceInterceptor>,
            >,
        ) {
            self.handle().set_browser_resource_interceptor(interceptor);
        }

        $crate::events::impl_view_callback_setter!(set_on_browser_created, on_browser_created, ());
        $crate::events::impl_view_callback_setter!(set_on_browser_closed, on_browser_closed, ());
        $crate::events::impl_view_callback_setter!(
            set_on_create_failed,
            on_create_failed,
            rust_cef_core::ports::EngineError
        );
        $crate::events::impl_view_callback_setter!(set_on_close_cancelled, on_close_cancelled, ());
        $crate::events::impl_view_callback_setter!(
            set_on_before_unload,
            on_before_unload,
            $crate::events::BeforeUnloadInfo
        );
        $crate::events::impl_view_callback_setter!(set_on_got_focus, on_got_focus, ());
        $crate::events::impl_view_callback_setter!(set_on_title_changed, on_title_changed, String);
        $crate::events::impl_view_callback_setter!(
            set_on_loading_state_changed,
            on_loading_state_changed,
            $crate::events::LoadingStateChange
        );
        $crate::events::impl_view_callback_setter!(
            set_on_load_started,
            on_load_started,
            $crate::events::LoadStartInfo
        );
        $crate::events::impl_view_callback_setter!(
            set_on_load_finished,
            on_load_finished,
            $crate::events::LoadEndInfo
        );
        $crate::events::impl_view_callback_setter!(
            set_on_load_error,
            on_load_error,
            $crate::events::LoadErrorInfo
        );
        $crate::events::impl_view_callback_setter!(
            set_on_navigation_requested,
            on_navigation_requested,
            $crate::events::NavigationRequestEvent
        );
        $crate::events::impl_view_callback_setter!(
            set_on_loading_progress_changed,
            on_loading_progress_changed,
            f64
        );
        $crate::events::impl_view_callback_setter!(
            set_on_document_available_in_main_frame,
            on_document_available_in_main_frame,
            ()
        );
        $crate::events::impl_view_callback_setter!(set_on_tooltip_changed, on_tooltip_changed, String);
        $crate::events::impl_view_callback_setter!(set_on_status_message, on_status_message, String);
        $crate::events::impl_view_callback_setter!(
            set_on_favicon_urls_changed,
            on_favicon_urls_changed,
            Vec<String>
        );
        $crate::events::impl_view_callback_setter!(
            set_on_fullscreen_mode_changed,
            on_fullscreen_mode_changed,
            bool
        );
        $crate::events::impl_view_callback_setter!(
            set_on_console_message,
            on_console_message,
            $crate::events::ConsoleMessageInfo
        );
        $crate::events::impl_view_callback_setter!(
            set_on_cursor_changed,
            on_cursor_changed,
            rust_cef_core::contracts::BrowserCursor
        );
        $crate::events::impl_view_callback_setter!(
            set_on_auto_resize,
            on_auto_resize,
            $crate::events::AutoResizeInfo
        );
        $crate::events::impl_view_callback_setter!(
            set_on_media_access_changed,
            on_media_access_changed,
            $crate::events::MediaAccessInfo
        );
        $crate::events::impl_view_callback_setter!(set_on_zoom_level_changed, on_zoom_level_changed, f64);
        $crate::events::impl_view_callback_setter!(
            set_on_audio_muted_changed,
            on_audio_muted_changed,
            bool
        );
        $crate::events::impl_view_callback_setter!(
            set_on_draggable_regions_changed,
            on_draggable_regions_changed,
            Vec<rust_cef_core::contracts::DraggableRegionRect>
        );
        $crate::events::impl_view_callback_setter!(
            set_on_frame_created,
            on_frame_created,
            rust_cef_core::contracts::FrameLifecycleInfo
        );
        $crate::events::impl_view_callback_setter!(
            set_on_frame_attached,
            on_frame_attached,
            $crate::events::FrameAttachedInfo
        );
        $crate::events::impl_view_callback_setter!(
            set_on_frame_detached,
            on_frame_detached,
            $crate::events::FrameDetachedInfo
        );
        $crate::events::impl_view_callback_setter!(
            set_on_main_frame_changed,
            on_main_frame_changed,
            $crate::events::MainFrameChangedInfo
        );
        $crate::events::impl_view_callback_setter!(
            set_on_context_created,
            on_context_created,
            $crate::events::V8ContextLifecycleInfo
        );
        $crate::events::impl_view_callback_setter!(
            set_on_context_released,
            on_context_released,
            $crate::events::V8ContextLifecycleInfo
        );
        $crate::events::impl_view_callback_setter!(
            set_on_focused_node_changed,
            on_focused_node_changed,
            Option<rust_cef_core::contracts::FocusedDomNodeInfo>
        );
        $crate::events::impl_view_callback_setter!(
            set_on_uncaught_exception,
            on_uncaught_exception,
            rust_cef_core::contracts::JsUncaughtExceptionInfo
        );
        $crate::events::impl_view_callback_setter!(
            set_on_resource_response,
            on_resource_response,
            rust_cef_core::contracts::ResourceResponseInfo
        );
        $crate::events::impl_view_callback_setter!(
            set_on_resource_load_complete,
            on_resource_load_complete,
            rust_cef_core::contracts::ResourceLoadCompleteInfo
        );
        $crate::events::impl_view_callback_setter!(set_on_render_view_ready, on_render_view_ready, ());
        $crate::events::impl_view_callback_setter!(
            set_on_renderer_terminated,
            on_renderer_terminated,
            $crate::events::RendererTerminatedInfo
        );
        $crate::events::impl_view_callback_setter!(
            set_on_render_process_responsive_changed,
            on_render_process_responsive_changed,
            bool
        );
        $crate::events::impl_view_callback_setter!(
            set_on_policy_denied,
            on_policy_denied,
            rust_cef_core::ports::PolicyAction
        );
    };
}

pub(crate) use {impl_view_callback_setter, impl_view_surface_api};

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyView;

    #[test]
    fn extended_event_mask_tracks_all_subscribed_slots_and_clears() {
        let mut callbacks = BrowserViewCallbacks::<DummyView>::default();
        assert_eq!(callbacks.extended_event_mask(), 0);

        callbacks.on_tooltip_changed.set(Some(Box::new(|_, _, _, _| {})));
        assert_eq!(callbacks.extended_event_mask(), EXT_EVENT_TOOLTIP);

        callbacks.on_console_message.set(Some(Box::new(|_, _, _, _| {})));
        callbacks.on_auto_resize.set(Some(Box::new(|_, _, _, _| {})));
        callbacks.on_media_access_changed.set(Some(Box::new(|_, _, _, _| {})));
        callbacks
            .on_document_available_in_main_frame
            .set(Some(Box::new(|_, _, _, _| {})));
        callbacks
            .on_draggable_regions_changed
            .set(Some(Box::new(|_, _, _, _| {})));
        callbacks.on_frame_created.set(Some(Box::new(|_, _, _, _| {})));
        callbacks.on_context_created.set(Some(Box::new(|_, _, _, _| {})));
        callbacks.on_focused_node_changed.set(Some(Box::new(|_, _, _, _| {})));
        callbacks.on_uncaught_exception.set(Some(Box::new(|_, _, _, _| {})));
        callbacks.on_resource_response.set(Some(Box::new(|_, _, _, _| {})));

        let expected = EXT_EVENT_TOOLTIP
            | EXT_EVENT_CONSOLE
            | EXT_EVENT_AUTO_RESIZE
            | EXT_EVENT_MEDIA_ACCESS
            | EXT_EVENT_DOCUMENT_AVAILABLE
            | EXT_EVENT_DRAGGABLE_REGIONS
            | EXT_EVENT_FRAME_LIFECYCLE
            | EXT_EVENT_V8_CONTEXT
            | EXT_EVENT_FOCUSED_NODE
            | EXT_EVENT_UNCAUGHT_EXCEPTION
            | EXT_EVENT_RESOURCE_OBSERVE;
        assert_eq!(callbacks.extended_event_mask(), expected);

        callbacks.on_frame_created.set(None);
        callbacks.on_frame_detached.set(Some(Box::new(|_, _, _, _| {})));
        assert_ne!(
            callbacks.extended_event_mask() & EXT_EVENT_FRAME_LIFECYCLE,
            0
        );

        callbacks.clear_all();
        assert_eq!(callbacks.extended_event_mask(), 0);
    }

    #[test]
    fn callback_slot_epoch_prevents_stale_restore_after_reentrant_replacement_or_clear() {
        let mut slot = CallbackSlot::<DummyView, i32>::default();
        slot.set(Some(Box::new(|_, _, _, _| {})));
        let (first, first_epoch) = slot.take_with_epoch().expect("first callback");

        // Re-entrant clear before `first` completes must stay cleared.
        slot.clear();
        slot.restore_if_unchanged(first, first_epoch);
        assert!(!slot.is_some());

        // Re-entrant replacement before taken callback finishes must keep replacement.
        slot.set(Some(Box::new(|_, _, _, _| {})));
        let (old_cb, old_epoch) = slot.take_with_epoch().expect("old callback");
        slot.set(Some(Box::new(|_, _, _, _| {})));
        slot.restore_if_unchanged(old_cb, old_epoch);
        assert!(slot.is_some());
        let (_current, current_epoch) = slot.take_with_epoch().expect("replacement kept");
        assert_ne!(current_epoch, old_epoch);
    }
}
