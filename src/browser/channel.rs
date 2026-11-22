//! Shared main-thread state between the GPUI view and the CEF client handlers.
//!
//! CEF requires every OSR call (`was_resized`, key/mouse events, IME) to happen
//! on the thread that runs the message loop. That thread is GPUI's main thread,
//! so a small shared cell is enough — no background plumbing.

use cef::{Browser, ImplBrowser, ImplRunContextMenuCallback, RunContextMenuCallback};
use std::sync::{
    Mutex, RwLock,
    atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};

/// Browser viewport in device-independent pixels plus the display scale used to
/// turn it into the device pixels CEF paints.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub width: i32,
    pub height: i32,
    pub scale: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            width: 1024,
            height: 640,
            scale: 1.0,
        }
    }
}

/// The subset of CEF cursor types this demo renders. Chromium reports a cursor
/// per hovered element; we mirror the common ones onto GPUI's cursor styles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    #[default]
    Arrow,
    Pointer,
    IBeam,
    Crosshair,
    Grab,
    Grabbing,
    NotAllowed,
    ContextMenu,
    ResizeEastWest,
    ResizeNorthSouth,
    ResizeNortheastSouthwest,
    ResizeNorthwestSoutheast,
    ResizeColumn,
    ResizeRow,
    Move,
    VerticalText,
    Cell,
    None,
}

impl CursorShape {
    fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Pointer,
            2 => Self::IBeam,
            3 => Self::Crosshair,
            4 => Self::Grab,
            5 => Self::Grabbing,
            6 => Self::NotAllowed,
            7 => Self::ContextMenu,
            8 => Self::ResizeEastWest,
            9 => Self::ResizeNorthSouth,
            10 => Self::ResizeNortheastSouthwest,
            11 => Self::ResizeNorthwestSoutheast,
            12 => Self::ResizeColumn,
            13 => Self::ResizeRow,
            14 => Self::Move,
            15 => Self::VerticalText,
            16 => Self::Cell,
            17 => Self::None,
            _ => Self::Arrow,
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::Arrow => 0,
            Self::Pointer => 1,
            Self::IBeam => 2,
            Self::Crosshair => 3,
            Self::Grab => 4,
            Self::Grabbing => 5,
            Self::NotAllowed => 6,
            Self::ContextMenu => 7,
            Self::ResizeEastWest => 8,
            Self::ResizeNorthSouth => 9,
            Self::ResizeNortheastSouthwest => 10,
            Self::ResizeNorthwestSoutheast => 11,
            Self::ResizeColumn => 12,
            Self::ResizeRow => 13,
            Self::Move => 14,
            Self::VerticalText => 15,
            Self::Cell => 16,
            Self::None => 17,
        }
    }
}

pub struct BrowserChannel {
    browser: Mutex<Option<Browser>>,
    viewport: RwLock<Viewport>,
    resize_recovery: Mutex<gpui_cef_poc::resize_recovery::ResizeRecovery>,
    cursor: AtomicU8,
    /// Latest document title, used by the headless interaction self-test.
    title: Mutex<String>,
    /// Caret rectangle reported by Chromium for the active composition, in CEF
    /// view coordinates (x, y, height). Used to place the IME candidate window.
    ime_caret: Mutex<Option<(i32, i32, i32, i32)>>,
    /// HWND of the gpui window hosting this browser, needed to place the IME
    /// candidate window. Recorded by the view on first paint.
    window_hwnd: Mutex<Option<isize>>,
    /// Top-left of the browser surface in client pixels. CEF reports the caret
    /// relative to its own view, so the IME anchor has to add this inset or the
    /// candidate window lands above and left of the text.
    surface_origin: Mutex<(f32, f32)>,
    /// Pending candidate-window placement, handed from CEF's thread to the UI.
    ime_reposition: Mutex<Option<(isize, (i32, i32, i32, i32))>>,
    /// Last pointer position in view coordinates, needed when CEF asks where a
    /// drag-out ended.
    last_cursor: Mutex<(i32, i32)>,
    /// Whether frames arrive as D3D11 shared textures rather than CPU buffers.
    accelerated: AtomicBool,
    /// Whether Chromium currently has a text field focused. Drives whether the
    /// host offers an IME context at all: without a focused input, keystrokes
    /// must go straight through as key events instead of opening a composition.
    accepts_text: AtomicBool,
    /// Cost of the most recent GPU→CPU read-back of an accelerated frame.
    last_readback: Mutex<Option<(u128, bool)>>,
    /// Menu the page wants shown, and the callback CEF is waiting on. CEF keeps
    /// the browser inside `RunContextMenu` until `continue`/`cancel` is called,
    /// so both have to be parked here between the callback and the click.
    context_menu: Mutex<Option<OpenContextMenu>>,
    next_menu_id: AtomicU64,
}

/// A context menu waiting for the user to pick an item.
pub struct OpenContextMenu {
    pub id: u64,
    pub menu: super::context_menu::ContextMenu,
    pub callback: RunContextMenuCallback,
}

impl Default for BrowserChannel {
    fn default() -> Self {
        Self {
            browser: Mutex::new(None),
            viewport: RwLock::new(Viewport::default()),
            resize_recovery: Mutex::new(Default::default()),
            cursor: AtomicU8::new(CursorShape::Arrow.code()),
            title: Mutex::new(String::new()),
            ime_caret: Mutex::new(None),
            window_hwnd: Mutex::new(None),
            surface_origin: Mutex::new((0.0, 0.0)),
            ime_reposition: Mutex::new(None),
            last_cursor: Mutex::new((0, 0)),
            accelerated: AtomicBool::new(false),
            accepts_text: AtomicBool::new(false),
            last_readback: Mutex::new(None),
            context_menu: Mutex::new(None),
            next_menu_id: AtomicU64::new(1),
        }
    }
}

impl BrowserChannel {
    /// Records the page title. A title is the cheapest way for the demo page to
    /// report back what actually reached the renderer process.
    pub fn set_title(&self, title: &str) {
        if let Ok(mut current) = self.title.lock()
            && current.as_str() != title
        {
            *current = title.to_string();
            eprintln!("page title: {title}");
        }
    }

    /// Records Chromium's caret rectangle for the current composition so the
    /// platform IME can hang its candidate window off the right place.
    pub fn set_ime_caret(&self, caret: Option<(i32, i32, i32, i32)>) {
        if let Ok(mut current) = self.ime_caret.lock() {
            *current = caret;
        }
    }

    pub fn record_readback(&self, micros: u128, ok: bool) {
        if let Ok(mut slot) = self.last_readback.lock() {
            *slot = Some((micros, ok));
        }
    }

    /// Human-readable summary of the last read-back, for the pump report.
    pub fn last_readback(&self) -> String {
        match self.last_readback.lock().ok().and_then(|slot| *slot) {
            Some((micros, ok)) => format!("{}us ok={ok}", micros),
            None => "n/a".to_string(),
        }
    }

    /// Parks the menu and its callback. Replaces any previously open menu: CEF
    /// only ever asks for one at a time, and a second one would leak the first.
    pub fn open_context_menu(
        &self,
        menu: super::context_menu::ContextMenu,
        callback: RunContextMenuCallback,
    ) {
        let id = self.next_menu_id.fetch_add(1, Ordering::Relaxed);
        if std::env::var("POC_LOG_MENU").is_ok() {
            eprintln!("MENU_OPEN id={id} view=({}, {})", menu.x, menu.y);
        }
        let previous = self
            .context_menu
            .lock()
            .ok()
            .and_then(|mut slot| slot.replace(OpenContextMenu { id, menu, callback }));
        // CEF callbacks may re-enter the handler: do not hold the menu lock.
        if let Some(previous) = previous {
            previous.callback.cancel();
        }
    }

    /// Takes only the requested opening. A late event from an old popup must
    /// never answer a newer CEF callback.
    fn take_context_menu(&self, expected_id: Option<u64>) -> Option<OpenContextMenu> {
        let mut slot = self.context_menu.lock().ok()?;
        if expected_id.is_some_and(|id| slot.as_ref().is_none_or(|open| open.id != id)) {
            return None;
        }
        slot.take()
    }

    pub fn has_context_menu(&self) -> bool {
        self.context_menu
            .lock()
            .map(|slot| slot.is_some())
            .unwrap_or(false)
    }

    /// Reads the open menu without consuming it, for rendering.
    pub fn with_context_menu<R>(&self, f: impl FnOnce(&OpenContextMenu) -> R) -> Option<R> {
        let slot = self.context_menu.lock().ok()?;
        slot.as_ref().map(f)
    }

    /// Answers the open menu with `command_id`, or cancels it when `None`.
    pub fn resolve_context_menu(&self, command_id: Option<i32>) -> bool {
        self.resolve_context_menu_for(None, command_id)
    }

    pub fn resolve_context_menu_for(
        &self,
        expected_id: Option<u64>,
        command_id: Option<i32>,
    ) -> bool {
        let Some(open) = self.take_context_menu(expected_id) else {
            return false;
        };
        if std::env::var("POC_LOG_MENU").is_ok() {
            eprintln!("MENU_RESOLVE id={} command={command_id:?}", open.id);
            match command_id {
                Some(id) => eprintln!("CONTEXT_MENU pick {id}"),
                None => eprintln!("CONTEXT_MENU dismissed"),
            }
        }
        match command_id {
            Some(id) => open.callback.cont(id, cef::EventFlags::default()),
            None => open.callback.cancel(),
        }
        true
    }

    pub fn set_accepts_text(&self, accepts: bool) {
        self.accepts_text.store(accepts, Ordering::Relaxed);
    }

    pub fn accepts_text(&self) -> bool {
        self.accepts_text.load(Ordering::Relaxed)
    }

    pub fn set_accelerated(&self, accelerated: bool) {
        self.accelerated.store(accelerated, Ordering::Relaxed);
    }

    /// `true` once an accelerated frame has actually arrived, so status text
    /// reports the real path rather than the requested one.
    pub fn is_accelerated(&self) -> bool {
        self.accelerated.load(Ordering::Relaxed)
    }

    pub fn set_last_cursor(&self, x: i32, y: i32) {
        if let Ok(mut current) = self.last_cursor.lock() {
            *current = (x, y);
        }
    }

    pub fn last_cursor(&self) -> (i32, i32) {
        self.last_cursor
            .lock()
            .map(|cursor| *cursor)
            .unwrap_or((0, 0))
    }

    pub fn set_window_hwnd(&self, hwnd: isize) {
        if let Ok(mut slot) = self.window_hwnd.lock() {
            *slot = Some(hwnd);
        }
    }

    /// Hands the caret to the UI thread, which owns the window and can call the
    /// Win32 IME API. CEF's callback runs on its own thread.
    pub fn request_ime_reposition(&self, hwnd: isize, caret: (i32, i32, i32, i32)) {
        if let Ok(mut slot) = self.ime_reposition.lock() {
            *slot = Some((hwnd, caret));
        }
    }

    /// Takes a pending IME reposition request.
    pub fn take_ime_reposition(&self) -> Option<(isize, (i32, i32, i32, i32))> {
        self.ime_reposition
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }

    /// Records where the browser surface sits inside the window, in client
    /// pixels (already scaled).
    pub fn set_surface_origin(&self, x: f32, y: f32) {
        if let Ok(mut slot) = self.surface_origin.lock() {
            *slot = (x, y);
        }
    }

    pub fn surface_origin(&self) -> (f32, f32) {
        self.surface_origin
            .lock()
            .map(|slot| *slot)
            .unwrap_or((0.0, 0.0))
    }

    pub fn window_hwnd(&self) -> Option<isize> {
        self.window_hwnd.lock().ok().and_then(|slot| *slot)
    }

    pub fn ime_caret(&self) -> Option<(i32, i32, i32, i32)> {
        self.ime_caret.lock().ok().and_then(|caret| *caret)
    }

    pub fn title(&self) -> String {
        self.title.lock().map(|t| t.clone()).unwrap_or_default()
    }

    pub fn set_browser(&self, browser: Browser) {
        if let Ok(mut slot) = self.browser.lock() {
            *slot = Some(browser);
        }
    }

    pub fn host(&self) -> Option<cef::BrowserHost> {
        self.browser.lock().ok()?.as_ref()?.host()
    }

    pub fn viewport(&self) -> Viewport {
        self.viewport
            .read()
            .map(|viewport| *viewport)
            .unwrap_or_default()
    }

    /// Records a new viewport. Returns which kind of change CEF has to be told
    /// about: a new scale requires `notify_screen_info_changed`, any size change
    /// requires `was_resized`.
    pub fn set_viewport(&self, width: i32, height: i32, scale: f32) -> ViewportChange {
        let next = Viewport {
            width: width.max(1),
            height: height.max(1),
            scale: if scale > 0.0 { scale } else { 1.0 },
        };
        let Ok(mut current) = self.viewport.write() else {
            return ViewportChange::None;
        };
        if *current == next {
            return ViewportChange::None;
        }
        let scale_changed = current.scale != next.scale;
        let size_changed = current.width != next.width || current.height != next.height;
        *current = next;
        if let Ok(mut recovery) = self.resize_recovery.lock() {
            recovery.resized(
                next.width,
                next.height,
                next.scale,
                std::time::Instant::now(),
            );
        }
        match (size_changed, scale_changed) {
            (_, true) => ViewportChange::Scale,
            (true, false) => ViewportChange::Size,
            (false, false) => ViewportChange::None,
        }
    }

    /// A successful full-frame copy, not merely a paint callback, completes a resize.
    pub fn frame_received(&self, width: u32, height: u32) {
        if let Ok(mut recovery) = self.resize_recovery.lock() {
            recovery.frame_received(width, height);
        }
    }

    /// Run outside CEF's callbacks so a refresh cannot re-enter an active paint.
    pub fn recover_resize(&self) {
        let request = self
            .resize_recovery
            .lock()
            .is_ok_and(|mut recovery| recovery.take_request(std::time::Instant::now()));
        if request && let Some(host) = self.host() {
            use cef::ImplBrowserHost;
            host.invalidate(cef::PaintElementType::VIEW);
        }
    }

    pub fn cursor(&self) -> CursorShape {
        CursorShape::from_code(self.cursor.load(Ordering::Relaxed))
    }

    pub fn set_cursor(&self, shape: CursorShape) {
        self.cursor.store(shape.code(), Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewportChange {
    None,
    Size,
    Scale,
}
