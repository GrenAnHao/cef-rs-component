use super::{BrowserChannel, CursorShape, FrameStore, frame_len};
use cef::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone)]
pub struct CefRenderHandler {
    frames: Arc<FrameStore>,
    channel: Arc<BrowserChannel>,
    popup: Arc<PopupState>,
    frame_log: Option<String>,
    /// D3D11 device used to receive shared textures. `None` means the software
    /// path is in use, either because it was not requested or because the
    /// adapter refused cross-process sharing.
    receiver: Option<Arc<crate::browser::d3d11_receiver::TextureReceiver>>,
    /// Reserved CPU scratch shares the store's total budget, including while a
    /// failed readback keeps it for reuse. Successful buffers transfer the same
    /// allocation and lease to the store without another pixel copy.
    scratch: Arc<std::sync::Mutex<Option<rust_cef_core::frame_budget::FrameBuffer>>>,
}

#[derive(Default)]
pub struct PopupState {
    showing: AtomicBool,
}

impl CefRenderHandler {
    pub fn new(
        frames: Arc<FrameStore>,
        channel: Arc<BrowserChannel>,
        receiver: Option<Arc<crate::browser::d3d11_receiver::TextureReceiver>>,
    ) -> Self {
        Self {
            frames,
            channel,
            popup: Arc::new(PopupState::default()),
            frame_log: std::env::var("POC_FRAME_LOG").ok(),
            receiver,
            scratch: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

wrap_render_handler! {
    pub(crate) struct CefRenderHandlerBuilder {
        handler: CefRenderHandler,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let Some(rect) = rect else { return };
            // CEF expects DIP (view) coordinates here; device scaling is applied
            // separately via device_scale_factor from screen_info.
            let viewport = self.handler.channel.viewport();
            *rect = Rect { x: 0, y: 0, width: viewport.width, height: viewport.height };
        }

        fn screen_info(
            &self,
            _browser: Option<&mut Browser>,
            screen_info: Option<&mut ScreenInfo>,
        ) -> ::std::os::raw::c_int {
            let Some(screen_info) = screen_info else { return false as _ };
            let viewport = self.handler.channel.viewport();
            // Without this, Chromium assumes scale 1.0 and paints blurry,
            // under-sized frames on a HiDPI display.
            screen_info.device_scale_factor = viewport.scale;
            let view = Rect { x: 0, y: 0, width: viewport.width, height: viewport.height };
            screen_info.rect = view.clone();
            screen_info.available_rect = view;
            true as _
        }

        /// CEF rendered into a shared D3D11 texture instead of a CPU buffer.
        ///
        /// We open the handle on our own device and read it back; see
        /// `d3d11_receiver` for why a copy is unavoidable with gpui 0.2.2.
        fn on_accelerated_paint(
            &self,
            _browser: Option<&mut Browser>,
            element_type: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            info: Option<&AcceleratedPaintInfo>,
        ) {
            if element_type != PaintElementType::VIEW {
                return;
            }
            let (Some(info), Some(receiver)) = (info, self.handler.receiver.as_ref()) else {
                return;
            };
            let Some(texture) = receiver.open(info.shared_texture_handle) else {
                return;
            };
            let width = info.extra.coded_size.width.max(0) as u32;
            let height = info.extra.coded_size.height.max(0) as u32;
            if frame_len(width, height).is_none() {
                return;
            }
            let visible = &info.extra.visible_rect;
            let content = &info.extra.content_rect;
            if !crate::browser::shared_texture::is_full_frame(
                (visible.x, visible.y, visible.width, visible.height),
                (content.x, content.y, content.width, content.height),
            ) {
                // CEF can capture a resized buffer with old, letterboxed content
                // (#3929). Keep the last good image and leave resize recovery
                // armed; cropping/stretching this frame would hide the problem.
                if self.handler.frame_log.is_some() {
                    eprintln!("Skipped letterboxed CEF frame: {width}x{height} content={content:?}");
                }
                return;
            }
            let Some(format) =
                crate::browser::shared_texture::FrameFormat::from_cef_raw(info.format.get_raw())
            else {
                return;
            };

            // CEF returns the shared handle to its pool when this callback
            // returns, so everything has to happen here.
            let texture_info = crate::browser::d3d11_receiver::TextureInfo {
                width,
                height,
                format,
            };
            if !receiver.copy_to_staging(&texture, texture_info) {
                return;
            }
            // Read back inline. Deferring the blocking map to the pump was
            // measured slower (456 vs 473 frames per 8 s): the frame then waits
            // a whole pump period before it can be displayed, which costs more
            // than the ~2 ms the map stalls for.
            {
                let Some(total) = frame_len(width, height) else { return; };
                let cached = {
                    let Ok(mut slot) = self.handler.scratch.lock() else { return; };
                    slot.take()
                };
                // Mismatched scratch drops outside the scratch mutex. Neither
                // allocation/reaping, D3D readback nor host publication holds it.
                let mut buffer = match cached.filter(|buffer| buffer.as_slice().len() == total) {
                    Some(buffer) => buffer,
                    None => match self.handler.frames.allocate(width, height) {
                        Ok(buffer) => buffer,
                        Err(_) => return, // Budget refusal is recorded by the shared ledger.
                    },
                };
                let started = std::time::Instant::now();
                let ok = receiver.read_staging(texture_info, buffer.as_mut_slice());
                self.handler
                    .channel
                    .record_readback(started.elapsed().as_micros(), ok);
                if ok {
                    if self.handler.frames.publish_buffer(width, height, buffer) {
                        self.handler.channel.frame_received(width, height);
                        let sequence = self.handler.frames.sequence();
                        if sequence == 1 {
                            eprintln!(
                                "First accelerated CEF frame: {width}x{height} {format:?}"
                            );
                        }
                        self.handler.channel.set_accelerated(true);
                        // Same diagnostic format as the CPU path: the resize and
                        // focus tests read this file, and without it they would
                        // silently report "0 frames" on the accelerated path.
                        if let Some(path) = self.handler.frame_log.as_ref()
                            && let Ok(mut file) = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(path)
                        {
                            use std::io::Write as _;
                            let _ = writeln!(file, "{sequence} {width}x{height} visible={:?} content={:?}", info.extra.visible_rect, info.extra.content_rect);
                        }
                    }
                } else {
                    // Do not replace another callback's cached scratch. Any
                    // unneeded buffer and its lease drop after the mutex guard.
                    let mut spare = Some(buffer);
                    if let Ok(mut slot) = self.handler.scratch.lock() {
                        if slot.is_none() { *slot = spare.take(); }
                    }
                    drop(spare);
                }
            }
        }

        /// Chromium reports whether a text field is focused.
        ///
        /// `TEXT_INPUT_MODE_NONE` means the keystrokes belong to the page (arrow
        /// keys, shortcuts, type-ahead) and no IME composition should start. The
        /// host mirrors this so its input handler stops accepting text, which is
        /// what previously opened a Chinese candidate window over plain text.
        fn on_virtual_keyboard_requested(
            &self,
            _browser: Option<&mut Browser>,
            input_mode: TextInputMode,
        ) {
            let accepts = input_mode != TextInputMode::NONE;
            if self.handler.channel.accepts_text() != accepts {
                self.handler.channel.set_accepts_text(accepts);
            }
        }

        fn on_ime_composition_range_changed(
            &self,
            _browser: Option<&mut Browser>,
            _selected_range: Option<&Range>,
            character_bounds: Option<&[Rect]>,
        ) {
            // Chromium reports one rectangle per composed character in view
            // coordinates. The caret is that of the last character; its
            // bottom-right corner is the IME anchor, so the raw left/top plus
            // width/height are kept and the caller adds them once.
            let caret = character_bounds.and_then(|bounds| {
                let last = bounds.last()?;
                Some((last.x, last.y, last.width, last.height))
            });
            self.handler.channel.set_ime_caret(caret);
            // Place the candidate window now. gpui positions it once, on
            // WM_IME_STARTCOMPOSITION, and CEF has not produced this rectangle
            // yet at that point; by the time it arrives the IME has already
            // opened the list at the window origin. This is the same moment in
            // the sequence that CefSharp's IME handler uses.
            if let (Some(hwnd), Some(caret)) =
                (self.handler.channel.window_hwnd(), caret)
            {
                self.handler
                    .channel
                    .request_ime_reposition(hwnd, caret);
            }
        }

        /// A drag started inside the page: a link, an image or selected text.
        ///
        /// Chromium expects the embedder to run the OS drag loop here and says
        /// an application message loop may be used. Our message pump is manual
        /// (`external_message_pump: 0` plus `do_message_loop_work`), and
        /// `DoDragDrop` runs its own modal loop on the same thread: in testing
        /// it never returned, leaving the window unresponsive.
        ///
        /// So this currently declines the drag. CEF then cancels it, which is
        /// the documented behaviour for returning false, and the page stays
        /// usable. Dragging *into* the browser (files) is implemented and works.
        /// `POC_DRAG_OUT=1` runs the OLE loop anyway for experiments.
        fn start_dragging(
            &self,
            browser: Option<&mut Browser>,
            drag_data: Option<&mut DragData>,
            allowed_ops: DragOperationsMask,
            _x: ::std::os::raw::c_int,
            _y: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            if std::env::var("POC_DRAG_OUT").is_err() {
                return false as _;
            }
            let Some(data) = drag_data else { return false as _ };
            let payload = crate::browser::ole_drag::DraggedPayload::from_cef(data);
            if payload.is_empty() {
                return false as _;
            }
            // CEF hands us DRAG_OPERATION_EVERY (-1) when the page accepts any
            // operation; clamp it to bits the target can actually complete.
            let raw_allowed = crate::browser::dnd::raw(allowed_ops) as u32;
            let allowed = raw_allowed & crate::browser::dnd::ACCEPTED_OPERATIONS;
            let allowed = if allowed == 0 {
                crate::browser::dnd::ACCEPTED_OPERATIONS
            } else {
                allowed
            };

            let result = crate::browser::ole_drag::run_drag_loop(&payload, allowed);
            if let Some(host) = browser.and_then(|browser| browser.host()) {
                let (x, y) = self.handler.channel.last_cursor();
                host.drag_source_ended_at(x, y, crate::browser::dnd::mask(result));
                host.drag_source_system_drag_ended();
            }
            true as _
        }

        fn update_drag_cursor(
            &self,
            _browser: Option<&mut Browser>,
            _operation: DragOperationsMask,
        ) {
            // The OLE loop draws its own cursors.
        }

        fn on_popup_show(&self, _browser: Option<&mut Browser>, show: ::std::os::raw::c_int) {
            self.handler.popup.showing.store(show != 0, Ordering::Relaxed);
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            element_type: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            if element_type != PaintElementType::VIEW || buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            let Some(length) = frame_len(width as u32, height as u32) else { return; };
            // SAFETY: CEF owns width*height*4 bytes until this callback returns.
            // Dimensions are validated, and publish_bgra copies the bytes.
            let pixels = unsafe { std::slice::from_raw_parts(buffer, length) };
            if self.handler.frames.publish_bgra(width as u32, height as u32, pixels) {
                self.handler.channel.frame_received(width as u32, height as u32);
                let sequence = self.handler.frames.sequence();
                if sequence == 1 {
                    eprintln!("First CEF frame: {width}x{height} BGRA");
                }
                if let Some(path) = self.handler.frame_log.as_ref()
                    && let Ok(mut file) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                {
                    // One line per frame: `<sequence> <wxh>`. Used by the
                    // headless resize/focus test instead of parsing stdout,
                    // which a dead pipe reader can block on.
                    use std::io::Write as _;
                    let _ = writeln!(file, "{sequence} {width}x{height}");
                }
            }
        }
    }
}

wrap_life_span_handler! {
    pub(crate) struct CefLifeSpanHandlerBuilder {
        closed: Arc<AtomicBool>,
    }

    impl LifeSpanHandler {
        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            self.closed.store(true, Ordering::Release);
        }
    }
}

wrap_display_handler! {
    pub(crate) struct CefDisplayHandlerBuilder {
        channel: Arc<BrowserChannel>,
    }

    impl DisplayHandler {
        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            if let Some(title) = title {
                self.channel.set_title(&title.to_string());
            }
        }

        fn on_cursor_change(
            &self,
            _browser: Option<&mut Browser>,
            _cursor: CursorHandle,
            type_: CursorType,
            _custom_cursor_info: Option<&CursorInfo>,
        ) -> ::std::os::raw::c_int {
            let shape = if type_ == CursorType::POINTER {
                CursorShape::Pointer
            } else if type_ == CursorType::IBEAM {
                CursorShape::IBeam
            } else if type_ == CursorType::CROSS {
                CursorShape::Crosshair
            } else if type_ == CursorType::HAND {
                // CT_HAND is the link cursor (a pointing hand), not "grab".
                CursorShape::Pointer
            } else if type_ == CursorType::NOTALLOWED {
                CursorShape::NotAllowed
            } else if type_ == CursorType::CONTEXTMENU {
                CursorShape::ContextMenu
            } else if type_ == CursorType::EASTWESTRESIZE {
                CursorShape::ResizeEastWest
            } else if type_ == CursorType::NORTHSOUTHRESIZE {
                CursorShape::ResizeNorthSouth
            } else if type_ == CursorType::NORTHEASTSOUTHWESTRESIZE {
                CursorShape::ResizeNortheastSouthwest
            } else if type_ == CursorType::NORTHWESTSOUTHEASTRESIZE {
                CursorShape::ResizeNorthwestSoutheast
            } else if type_ == CursorType::COLUMNRESIZE {
                CursorShape::ResizeColumn
            } else if type_ == CursorType::ROWRESIZE {
                CursorShape::ResizeRow
            } else if type_ == CursorType::MOVE {
                CursorShape::Move
            } else if type_ == CursorType::VERTICALTEXT {
                CursorShape::VerticalText
            } else if type_ == CursorType::CELL {
                CursorShape::Cell
            } else if type_ == CursorType::NONE {
                CursorShape::None
            } else {
                CursorShape::Arrow
            };
            self.channel.set_cursor(shape);
            false as _
        }
    }
}

wrap_context_menu_handler! {
    pub(crate) struct CefContextMenuHandlerBuilder {
        channel: Arc<BrowserChannel>,
    }

    impl ContextMenuHandler {
        /// Named here so the menu is reported consistently: CEF calls this for
        /// every right click, and `run_context_menu` afterwards only when the
        /// client asked for a custom display.
        fn on_before_context_menu(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _params: Option<&mut ContextMenuParams>,
            _model: Option<&mut MenuModel>,
        ) {
        }

        /// CEF has no window to hang a native menu on in OSR mode, so the menu
        /// has to be drawn by the embedder. Converting the model here (on the
        /// CEF UI thread) is safe because `model` is only valid in this
        /// callback; the flattened copy is what the UI renders later.
        fn run_context_menu(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            params: Option<&mut ContextMenuParams>,
            model: Option<&mut MenuModel>,
            callback: Option<&mut RunContextMenuCallback>,
        ) -> ::std::os::raw::c_int {
            let (Some(params), Some(model), Some(callback)) = (params, model, callback) else {
                return false as _;
            };
            let menu = menu_from_cef(model, params);
            if menu.is_empty() {
                // Nothing to show: answer immediately so CEF is not left waiting.
                callback.cancel();
                return true as _;
            }
            // The interaction self-test asserts on this through the page title;
            // there is no other way for it to see a host-drawn menu.
            if std::env::var("POC_LOG_MENU").is_ok() {
                eprintln!("CONTEXT_MENU {}", menu.describe());
                self.channel.set_title(&format!("menu:{}", menu.describe()));
            }
            self.channel.open_context_menu(menu, callback.clone());
            // Returning true means "we display it"; CEF then waits for the
            // callback. The gpui side answers through `resolve_context_menu`.
            true as _
        }
    }
}

/// Flattens a CEF menu model into the UI's representation.
fn menu_from_cef(
    model: &MenuModel,
    params: &ContextMenuParams,
) -> crate::browser::context_menu::ContextMenu {
    use crate::browser::context_menu::{ContextMenu, MenuEntry};
    use cef::ImplMenuModel;
    let mut entries = Vec::new();
    let count = model.count();
    for index in 0..count {
        if model.is_visible_at(index) == 0 {
            continue;
        }
        let item_type = model.type_at(index);
        if item_type == MenuItemType::SEPARATOR {
            entries.push(MenuEntry::Separator);
            continue;
        }
        // Submenus are not flattened: their items would need a nested popup,
        // which is separate UI work. Skip rather than show a dead entry.
        if item_type == MenuItemType::SUBMENU {
            continue;
        }
        let command_id = model.command_id_at(index);
        // Chromium's internal ids (negative) are not meant to be answered with;
        // only the standard command range is forwarded.
        let label =
            crate::browser::context_menu::strip_accelerator(&cef_string(model.label_at(index)));
        if label.is_empty() {
            continue;
        }
        entries.push(MenuEntry::Command {
            command_id,
            label,
            enabled: model.is_enabled_at(index) != 0,
            checked: model.is_checked_at(index) != 0,
        });
    }
    let mut menu = ContextMenu {
        entries,
        x: params.xcoord(),
        y: params.ycoord(),
    };
    menu.normalize();
    menu
}

/// CEF hands back userfree strings that must be converted (and freed) at once.
fn cef_string(value: cef::CefStringUserfree) -> String {
    cef::CefString::from(&value).to_string()
}

wrap_client! {
    pub(crate) struct CefClientBuilder {
        render_handler: RenderHandler,
        life_span_handler: LifeSpanHandler,
        display_handler: DisplayHandler,
        context_menu_handler: ContextMenuHandler,
    }

    impl Client {
        fn context_menu_handler(&self) -> Option<ContextMenuHandler> {
            Some(self.context_menu_handler.clone())
        }

        fn render_handler(&self) -> Option<RenderHandler> {
            Some(self.render_handler.clone())
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span_handler.clone())
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display_handler.clone())
        }
    }
}

impl CefClientBuilder {
    pub fn build(
        handler: CefRenderHandler,
        closed: Arc<AtomicBool>,
        channel: Arc<BrowserChannel>,
    ) -> Client {
        Self::new(
            CefRenderHandlerBuilder::new(handler),
            CefLifeSpanHandlerBuilder::new(closed),
            CefDisplayHandlerBuilder::new(channel.clone()),
            CefContextMenuHandlerBuilder::new(channel),
        )
    }
}
