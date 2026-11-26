//! One-shot view attachment for a Runtime Browser.
//!
//! This module owns pure data only: the host's own `BrowserHandle`, the Browser
//! identity, the authoritative `FrameSource` endpoint, and a GPUI-free
//! [`RuntimeBrowserBridge`] handle (`Weak<UiRegistry>`) for async frame text,
//! on-demand D3D11 shared-texture readback, and transient CDP screenshot calls.
//! It never borrows GPUI, never touches CEF/Win32, and never pumps or shuts
//! down the Runtime.

use std::fmt;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicU64, Ordering},
};

use rust_cef_core::{
    accessibility::AxTreeSlot,
    contracts::{BrowserId, BrowserSurfaceMode},
    cursor::CursorSlot,
    host_capabilities::{CdpMethodId, CdpObserver, HostCapabilityError},
    js_bridge::JsFrameScope,
    menu::NativeContextMenuSlot,
    ports::{BrowserHandle, CloseMode, EngineError, FrameSource, GpuSharedTextureFrame},
};

use crate::{
    executor::TaskPayload,
    frame_text::{FrameTextError, FrameTextKind},
    registry::UiRegistry,
};

static NEXT_REPLY_ID: AtomicU64 = AtomicU64::new(1);

/// Single-shot completion cell carried by `TaskPayload` so a dropped or rejected
/// task always delivers a deterministic fallback outcome without leaking waiters.
pub(crate) struct ReplyOnce<T: Send + 'static> {
    id: u64,
    inner: Arc<ReplyOnceInner<T>>,
}

struct ReplyOnceInner<T: Send + 'static> {
    cb: Mutex<Option<Box<dyn FnOnce(T) + Send>>>,
    on_drop: fn() -> T,
}

impl<T: Send + 'static> ReplyOnce<T> {
    pub(crate) fn new(cb: impl FnOnce(T) + Send + 'static, on_drop: fn() -> T) -> Self {
        Self {
            id: NEXT_REPLY_ID.fetch_add(1, Ordering::Relaxed),
            inner: Arc::new(ReplyOnceInner {
                cb: Mutex::new(Some(Box::new(cb))),
                on_drop,
            }),
        }
    }

    pub(crate) fn complete(&self, value: T) {
        let cb = self
            .inner
            .cb
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(cb) = cb {
            cb(value);
        }
    }
}

impl<T: Send + 'static> Clone for ReplyOnce<T> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T: Send + 'static> PartialEq for ReplyOnce<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl<T: Send + 'static> fmt::Debug for ReplyOnce<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReplyOnce").field("id", &self.id).finish()
    }
}

impl<T: Send + 'static> Drop for ReplyOnceInner<T> {
    fn drop(&mut self) {
        let cb = self
            .cb
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(cb) = cb {
            cb((self.on_drop)());
        }
    }
}

/// Cloneable, `PartialEq`-comparable wrapper for [`GpuSharedTextureFrame`] inside [`TaskPayload`].
#[derive(Clone)]
pub(crate) struct SharedTextureReadbackInput {
    id: u64,
    frame: Arc<GpuSharedTextureFrame>,
}

impl SharedTextureReadbackInput {
    pub(crate) fn new(frame: GpuSharedTextureFrame) -> Self {
        Self {
            id: NEXT_REPLY_ID.fetch_add(1, Ordering::Relaxed),
            frame: Arc::new(frame),
        }
    }

    pub(crate) fn frame(&self) -> &GpuSharedTextureFrame {
        &self.frame
    }
}

impl PartialEq for SharedTextureReadbackInput {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && Arc::ptr_eq(&self.frame, &other.frame)
    }
}

impl fmt::Debug for SharedTextureReadbackInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedTextureReadbackInput")
            .field("id", &self.id)
            .field("width", &self.frame.width)
            .field("height", &self.frame.height)
            .finish()
    }
}

/// GPUI-free, non-owning handle to a Browser's Runtime services, attached in
/// `Runtime::attach_browser` and retained by both `CefBrowser` and `NativeSlotController`.
#[derive(Clone)]
pub struct RuntimeBrowserBridge {
    browser: BrowserId,
    registry: Weak<UiRegistry>,
}

impl RuntimeBrowserBridge {
    pub(crate) fn new(browser: BrowserId, registry: Weak<UiRegistry>) -> Self {
        Self { browser, registry }
    }

    #[must_use]
    pub fn browser(&self) -> BrowserId {
        self.browser
    }

    /// Requests HTML source (`FrameTextKind::Source`) or display text (`FrameTextKind::Text`)
    /// from the specified frame scope on the CEF UI thread.
    pub fn request_frame_text(
        &self,
        kind: FrameTextKind,
        scope: JsFrameScope,
        max_bytes: usize,
        on_complete: impl FnOnce(Result<String, FrameTextError>) + Send + 'static,
    ) {
        let reply = ReplyOnce::new(on_complete, || Err(FrameTextError::Closed));
        let Some(registry) = self.registry.upgrade() else {
            reply.complete(Err(FrameTextError::Closed));
            return;
        };
        let Some(executor) = registry.executor() else {
            reply.complete(Err(FrameTextError::Closed));
            return;
        };
        if let Err(err) = executor.post(TaskPayload::ReadFrameText {
            browser: self.browser,
            kind,
            scope,
            max_bytes,
            reply: reply.clone(),
        }) {
            let mapped = match err {
                EngineError::NotReady => FrameTextError::NotReady,
                EngineError::PolicyDenied => FrameTextError::PolicyDenied,
                _ => FrameTextError::Closed,
            };
            reply.complete(Err(mapped));
        }
    }

    /// Reads back a `GpuSharedTextureFrame` into compact premultiplied BGRA8 bytes
    /// on the CEF UI thread via D3D11 staging copy.
    pub fn readback_shared_texture(
        &self,
        frame: GpuSharedTextureFrame,
        on_complete: impl FnOnce(Result<Vec<u8>, EngineError>) + Send + 'static,
    ) {
        let reply = ReplyOnce::new(on_complete, || Err(EngineError::Closed));
        let Some(registry) = self.registry.upgrade() else {
            reply.complete(Err(EngineError::Closed));
            return;
        };
        let Some(executor) = registry.executor() else {
            reply.complete(Err(EngineError::Closed));
            return;
        };
        let input = SharedTextureReadbackInput::new(frame);
        if let Err(err) = executor.post(TaskPayload::ReadbackSharedTexture {
            browser: self.browser,
            frame: input,
            reply: reply.clone(),
        }) {
            reply.complete(Err(err));
        }
    }

    /// Starts an opt-in managed headless CDP `Page.captureScreenshot` invocation,
    /// returning `(observer_token, method_id)` so the caller can detach the transient
    /// observer on completion, timeout, or close.
    pub fn start_cdp_screenshot(
        &self,
        params_json: Option<String>,
        observer: Arc<dyn CdpObserver>,
    ) -> Result<(u64, CdpMethodId), HostCapabilityError> {
        let registry = self
            .registry
            .upgrade()
            .ok_or(HostCapabilityError::BrowserClosed)?;
        if !registry.is_headless_cdp_enabled(self.browser)? {
            return Err(HostCapabilityError::CapabilityDisabled);
        }
        let executor = registry
            .executor()
            .ok_or(HostCapabilityError::BrowserClosed)?;
        if let Some(raw) = params_json.as_deref() {
            rust_cef_core::host_capabilities::validate_cdp_json(raw)?;
            if !raw.trim().starts_with('{') {
                return Err(HostCapabilityError::InvalidCdpPayload);
            }
        }
        let token = registry.headless_cdp.alloc_transient_token();
        let method_id = registry.headless_cdp.alloc_message_id(self.browser)?;
        registry
            .headless_cdp
            .set_pending_transient_observer(self.browser, token, observer);
        if executor
            .post(TaskPayload::CaptureCdpScreenshot {
                browser: self.browser,
                token,
                method_id,
                params_json,
            })
            .is_err()
        {
            let _ = registry
                .headless_cdp
                .take_pending_transient_observer(self.browser, token);
            return Err(HostCapabilityError::PostFailed);
        }
        Ok((token, method_id))
    }

    /// Posts UI-thread detachment for a transient CDP screenshot observer token.
    pub fn detach_cdp_screenshot(&self, token: u64) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let _ = registry
            .headless_cdp
            .take_pending_transient_observer(self.browser, token);
        if let Some(executor) = registry.executor() {
            let _ = executor.post_close(TaskPayload::DetachCdpScreenshot {
                browser: self.browser,
                token,
            });
        }
    }
}

impl fmt::Debug for RuntimeBrowserBridge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeBrowserBridge")
            .field("browser", &self.browser)
            .finish()
    }
}

/// Failure to bind one Browser to one view.
#[derive(Debug)]
pub enum AttachmentError {
    /// The identity is unknown/foreign, the Runtime is draining, or the
    /// instance/source is already terminal.
    Engine(EngineError),
    /// This Browser already handed out its single attachment token, even if
    /// that token has since been dropped unused.
    AlreadyAttached,
}

/// A single view's one-shot claim on one Browser.
///
/// The token is deliberately not `Clone`: a Browser can be claimed once and the
/// claim is sticky. It holds only owned data, so it can move into the host's
/// view constructor. Dropping it performs no native work; it submits one
/// ordinary Graceful close through the same `BrowserHandle` that produced it,
/// which preserves the original control and close-ticket sequence.
pub struct BrowserAttachment {
    browser: BrowserId,
    handle: BrowserHandle,
    source: Arc<dyn FrameSource>,
    ax_tree: AxTreeSlot,
    cursor_slot: CursorSlot,
    native_context_menu: NativeContextMenuSlot,
    surface_mode: BrowserSurfaceMode,
    bridge: Option<RuntimeBrowserBridge>,
}

impl BrowserAttachment {
    pub(crate) fn new(
        browser: BrowserId,
        handle: BrowserHandle,
        source: Arc<dyn FrameSource>,
    ) -> Self {
        Self {
            browser,
            handle,
            source,
            ax_tree: AxTreeSlot::new(),
            cursor_slot: CursorSlot::new(),
            native_context_menu: NativeContextMenuSlot::new(),
            surface_mode: BrowserSurfaceMode::Offscreen,
            bridge: None,
        }
    }

    pub(crate) fn with_ax_tree(mut self, ax_tree: AxTreeSlot) -> Self {
        self.ax_tree = ax_tree;
        self
    }

    pub(crate) fn with_cursor_slot(mut self, cursor_slot: CursorSlot) -> Self {
        self.cursor_slot = cursor_slot;
        self
    }

    pub(crate) fn with_native_context_menu(
        mut self,
        native_context_menu: NativeContextMenuSlot,
    ) -> Self {
        self.native_context_menu = native_context_menu;
        self
    }

    pub(crate) fn with_surface_mode(mut self, surface_mode: BrowserSurfaceMode) -> Self {
        self.surface_mode = surface_mode;
        self
    }

    pub(crate) fn with_runtime_bridge(mut self, bridge: RuntimeBrowserBridge) -> Self {
        self.bridge = Some(bridge);
        self
    }

    pub fn browser(&self) -> BrowserId {
        self.browser
    }

    pub fn handle(&self) -> &BrowserHandle {
        &self.handle
    }

    pub fn frame_source(&self) -> &Arc<dyn FrameSource> {
        &self.source
    }

    pub fn ax_tree(&self) -> &AxTreeSlot {
        &self.ax_tree
    }

    pub fn cursor_slot(&self) -> &CursorSlot {
        &self.cursor_slot
    }

    pub fn native_context_menu(&self) -> &NativeContextMenuSlot {
        &self.native_context_menu
    }

    pub fn surface_mode(&self) -> BrowserSurfaceMode {
        self.surface_mode
    }

    pub fn runtime_bridge(&self) -> Option<&RuntimeBrowserBridge> {
        self.bridge.as_ref()
    }
}

impl Drop for BrowserAttachment {
    fn drop(&mut self) {
        // The token lives only while its view does. Whether it was used or not,
        // dropping requests the ordinary close: asynchronous, pure-data
        // admission on the original control, never native teardown here.
        self.handle.request_close(CloseMode::Graceful);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_once_fires_fallback_when_dropped_incomplete_and_runs_at_most_once() {
        let count = Arc::new(AtomicU64::new(0));
        let last = Arc::new(Mutex::new(None));
        {
            let count_c = count.clone();
            let last_c = last.clone();
            let reply = ReplyOnce::new(
                move |v: Result<&'static str, &'static str>| {
                    count_c.fetch_add(1, Ordering::AcqRel);
                    *last_c.lock().unwrap() = Some(v);
                },
                || Err("dropped"),
            );
            let cloned = reply.clone();
            assert_eq!(reply, cloned);
            drop(reply);
            assert_eq!(count.load(Ordering::Acquire), 0);
        }
        assert_eq!(count.load(Ordering::Acquire), 1);
        assert_eq!(*last.lock().unwrap(), Some(Err("dropped")));

        let count2 = Arc::new(AtomicU64::new(0));
        {
            let count2_c = count2.clone();
            let reply = ReplyOnce::new(
                move |v: Result<&'static str, &'static str>| {
                    assert_eq!(v, Ok("ok"));
                    count2_c.fetch_add(1, Ordering::AcqRel);
                },
                || Err("dropped"),
            );
            reply.complete(Ok("ok"));
            reply.complete(Err("late"));
        }
        assert_eq!(count2.load(Ordering::Acquire), 1);
    }
}
