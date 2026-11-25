//! One-shot view attachment for a Runtime Browser.
//!
//! This module owns pure data only: the host's own `BrowserHandle`, the Browser
//! identity and the authoritative `FrameSource` endpoint. It never borrows
//! GPUI, never touches CEF/Win32, and never pumps or shuts down the Runtime.

use std::sync::Arc;

use rust_cef_core::{
    accessibility::AxTreeSlot,
    contracts::{BrowserId, BrowserSurfaceMode},
    cursor::CursorSlot,
    menu::NativeContextMenuSlot,
    ports::{BrowserHandle, CloseMode, EngineError, FrameSource},
};

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
}

impl Drop for BrowserAttachment {
    fn drop(&mut self) {
        // The token lives only while its view does. Whether it was used or not,
        // dropping requests the ordinary close: asynchronous, pure-data
        // admission on the original control, never native teardown here.
        self.handle.request_close(CloseMode::Graceful);
    }
}
