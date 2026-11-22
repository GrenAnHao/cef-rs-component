//! Minimal Win32 OSR fixture: one activated foreground window on the caller's
//! thread, real BGRA frames and real mouse input.
//!
//! Scope: this Runtime's OSR browser activation only. It is not GPUI input, not
//! IME evidence, and does not replace the future eleven GPUI/IME groups. Only
//! user32/gdi32/kernel32 are linked; no CEF object and no process-wide DPI mode.
//!
//! The owner thread pumps [`InputFixture::poll`]; that call translates and
//! dispatches only this window's messages, invalidates on a new frame, and
//! `WM_PAINT` stretches the latest frame to the real client rect. `WM_CLOSE`
//! only records a request: close the Runtime first, then drop the fixture.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::{
    cell::Cell,
    ffi::c_void,
    marker::PhantomData,
    mem::size_of,
    ptr,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use rust_cef_core::ports::{FrameSink, OwnedFrame, PublishResult, SurfaceKind};

/// Positive 32-bit transport value; still compared across the full ULONG_PTR.
/// Real case 80708c... preserved the old low word but not its complete 64-bit
/// marker. Do not truncate received metadata or accept a low-word fallback.
const MAGIC: usize = 0x4346_4530;

#[rustfmt::skip]
mod raw {
    #![allow(dead_code, non_camel_case_types, non_snake_case, clippy::too_many_arguments,
        clippy::type_complexity, clippy::upper_case_acronyms)]
    use std::ffi::c_void;
    pub type HWND = isize; pub type HINSTANCE = isize; pub type HDC = isize;
    pub type LRESULT = isize; pub type LPARAM = isize; pub type WPARAM = usize;
    pub type WndProc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;
    pub const WM_SETFOCUS: u32 = 0x0007; pub const WM_KILLFOCUS: u32 = 0x0008;
    pub const WM_PAINT: u32 = 0x000F; pub const WM_CLOSE: u32 = 0x0010;
    pub const WM_NCCREATE: u32 = 0x0081; pub const WM_NCDESTROY: u32 = 0x0082;
    pub const WM_LBUTTONDOWN: u32 = 0x0201; pub const WM_LBUTTONUP: u32 = 0x0202;
    pub const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000; pub const WS_VISIBLE: u32 = 0x1000_0000;
    pub const PM_REMOVE: u32 = 0x0001; pub const GWLP_USERDATA: i32 = -21;
    pub const CW_USEDEFAULT: i32 = i32::MIN; pub const IDC_ARROW: usize = 32512;
    pub const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410; pub const GDI_ERROR: i32 = -1;
    pub const BI_RGB: u32 = 0; pub const DIB_RGB_COLORS: u32 = 0;
    pub const SRCCOPY: u32 = 0x00CC_0020; pub const INPUT_MOUSE: u32 = 0;
    pub const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002; pub const MOUSEEVENTF_LEFTUP: u32 = 0x0004;

    #[repr(C)] #[derive(Clone, Copy, Default, PartialEq, Eq)]
    pub struct POINT { pub x: i32, pub y: i32 }
    #[repr(C)] #[derive(Clone, Copy, Default)]
    pub struct RECT { pub left: i32, pub top: i32, pub right: i32, pub bottom: i32 }
    #[repr(C)] #[derive(Clone, Copy, Default)]
    pub struct MSG { pub hwnd: HWND, pub message: u32, pub wParam: WPARAM, pub lParam: LPARAM,
        pub time: u32, pub pt: POINT, pub lPrivate: u32 }
    #[repr(C)] #[derive(Clone, Copy, Default)]
    pub struct PAINTSTRUCT { pub hdc: HDC, pub fErase: i32, pub rcPaint: RECT,
        pub fRestore: i32, pub fIncUpdate: i32, pub rgbReserved: [u8; 32] }
    /// Only `lpCreateParams` (offset zero) is read; the rest is SDK-owned.
    #[repr(C)] pub struct CREATESTRUCTW { pub lpCreateParams: *mut c_void }
    #[repr(C)]
    pub struct WNDCLASSEXW { pub cbSize: u32, pub style: u32, pub lpfnWndProc: Option<WndProc>,
        pub cbClsExtra: i32, pub cbWndExtra: i32, pub hInstance: HINSTANCE, pub hIcon: isize,
        pub hCursor: isize, pub hbrBackground: isize, pub lpszMenuName: *const u16,
        pub lpszClassName: *const u16, pub hIconSm: isize }
    #[repr(C)]
    pub struct BITMAPINFOHEADER { pub biSize: u32, pub biWidth: i32, pub biHeight: i32,
        pub biPlanes: u16, pub biBitCount: u16, pub biCompression: u32, pub biSizeImage: u32,
        pub biXPelsPerMeter: i32, pub biYPelsPerMeter: i32, pub biClrUsed: u32,
        pub biClrImportant: u32 }
    #[repr(C)] pub struct BITMAPINFO { pub bmiHeader: BITMAPINFOHEADER, pub bmiColors: [u32; 1] }
    #[repr(C)] #[derive(Clone, Copy, Default)]
    pub struct INPUT_MESSAGE_SOURCE { pub deviceType: u32, pub originId: u32 }
    #[repr(C)] #[derive(Clone, Copy)]
    pub struct MOUSEINPUT { pub dx: i32, pub dy: i32, pub mouseData: u32, pub dwFlags: u32,
        pub time: u32, pub dwExtraInfo: usize }
    #[repr(C)] #[derive(Clone, Copy)]
    pub struct KEYBDINPUT { pub wVk: u16, pub wScan: u16, pub dwFlags: u32, pub time: u32,
        pub dwExtraInfo: usize }
    #[repr(C)] #[derive(Clone, Copy)]
    pub struct HARDWAREINPUT { pub uMsg: u32, pub wParamL: u16, pub wParamH: u16 }
    #[repr(C)] #[derive(Clone, Copy)]
    pub union INPUT_UNION { pub mouse: MOUSEINPUT, pub key: KEYBDINPUT, pub hardware: HARDWAREINPUT }
    /// x64: 8-byte type slot, then the union sized by MOUSEINPUT (32); total 40.
    #[repr(C)] #[derive(Clone, Copy)]
    pub struct INPUT { pub r#type: u32, pub u: INPUT_UNION }

    #[link(name = "user32")]
    unsafe extern "system" {
        pub fn RegisterClassExW(class: *const WNDCLASSEXW) -> u16;
        pub fn CreateWindowExW(ex_style: u32, class: *const u16, name: *const u16, style: u32,
            x: i32, y: i32, w: i32, h: i32, parent: HWND, menu: isize, instance: HINSTANCE,
            param: *mut c_void) -> HWND;
        pub fn DestroyWindow(hwnd: HWND) -> i32;
        pub fn DefWindowProcW(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT;
        pub fn ShowWindow(hwnd: HWND, command: i32) -> i32;
        pub fn SetForegroundWindow(hwnd: HWND) -> i32;
        pub fn GetForegroundWindow() -> HWND;
        pub fn SetFocus(hwnd: HWND) -> HWND;
        pub fn GetClientRect(hwnd: HWND, rect: *mut RECT) -> i32;
        pub fn ClientToScreen(hwnd: HWND, point: *mut POINT) -> i32;
        pub fn WindowFromPoint(point: POINT) -> HWND;
        pub fn GetCursorPos(point: *mut POINT) -> i32;
        pub fn SetCursorPos(x: i32, y: i32) -> i32;
        pub fn SendInput(count: u32, inputs: *const INPUT, size: i32) -> u32;
        pub fn SetWindowLongPtrW(hwnd: HWND, index: i32, value: isize) -> isize;
        pub fn GetWindowLongPtrW(hwnd: HWND, index: i32) -> isize;
        pub fn GetMessageExtraInfo() -> isize;
        pub fn GetCurrentInputMessageSource(source: *mut INPUT_MESSAGE_SOURCE) -> i32;
        pub fn PeekMessageW(msg: *mut MSG, hwnd: HWND, min: u32, max: u32, remove: u32) -> i32;
        pub fn TranslateMessage(msg: *const MSG) -> i32;
        pub fn DispatchMessageW(msg: *const MSG) -> LRESULT;
        pub fn IsWindow(hwnd: HWND) -> i32;
        pub fn IsWindowVisible(hwnd: HWND) -> i32;
        pub fn GetWindowThreadProcessId(hwnd: HWND, process: *mut u32) -> u32;
        pub fn EnumWindows(callback: Option<unsafe extern "system" fn(HWND, LPARAM) -> i32>,
            param: LPARAM) -> i32;
        pub fn AdjustWindowRectEx(rect: *mut RECT, style: u32, menu: i32, ex_style: u32) -> i32;
        pub fn InvalidateRect(hwnd: HWND, rect: *const RECT, erase: i32) -> i32;
        pub fn BeginPaint(hwnd: HWND, paint: *mut PAINTSTRUCT) -> HDC;
        pub fn EndPaint(hwnd: HWND, paint: *const PAINTSTRUCT) -> i32;
        pub fn LoadCursorW(instance: HINSTANCE, name: *const u16) -> isize;
    }
    #[link(name = "gdi32")]
    unsafe extern "system" {
        pub fn StretchDIBits(hdc: HDC, x_dest: i32, y_dest: i32, w_dest: i32, h_dest: i32,
            x_src: i32, y_src: i32, w_src: i32, h_src: i32, bits: *const c_void,
            info: *const BITMAPINFO, usage: u32, rop: u32) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GetModuleHandleW(name: *const u16) -> HINSTANCE;
        pub fn GetCurrentThreadId() -> u32;
        pub fn GetCurrentProcessId() -> u32;
        pub fn GetLastError() -> u32;
    }
}
use raw::*;

// x64 ABI sizes; MSG/PAINTSTRUCT/WNDCLASSEXW include their tail padding.
const _: () = assert!(size_of::<INPUT_MESSAGE_SOURCE>() == 8);
const _: () = assert!(size_of::<MOUSEINPUT>() == 32);
const _: () = assert!(size_of::<INPUT>() == 40);
const _: () = assert!(size_of::<MSG>() == 48);
const _: () = assert!(size_of::<PAINTSTRUCT>() == 72);
const _: () = assert!(size_of::<WNDCLASSEXW>() == 80);
// The sink crosses into the CEF capture thread; the fixture itself does not.
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<FrameInputSender>();
};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
fn thread_id() -> u32 {
    // SAFETY: no arguments; reads only this thread's id.
    unsafe { GetCurrentThreadId() }
}
fn failed(what: &str) -> String {
    // SAFETY: no arguments; reads this thread's last-error value.
    format!("{what} (error {})", unsafe { GetLastError() })
}

/// A mouse/focus message from the real fixture window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputMessage {
    Focus(bool),
    MouseClick {
        /// Coordinates in the current frame's view pixel space.
        x: i32,
        y: i32,
        up: bool,
        /// True only when the button message carried our SendInput marker.
        os_injected: bool,
        /// Diagnostic classifications only, never accepted as marker fallback.
        low32_marker_match: bool,
        extra_info_zero: bool,
        injected_origin: bool,
    },
}

/// Clone + Send + Sync frame sink holding only the latest owned view frame.
/// It never calls Win32, CEF or GPUI; the poll loop notices the new frame.
#[derive(Clone, Default)]
pub struct FrameInputSender {
    shared: Arc<Mutex<Option<OwnedFrame>>>,
}
impl FrameInputSender {
    pub fn new() -> Self {
        Self::default()
    }
    /// Store an owned BGRA frame without touching Win32, CEF or GPUI.
    pub fn publish(&self, frame: OwnedFrame) -> PublishResult {
        let Some(len) = frame
            .info
            .width
            .checked_mul(frame.info.height)
            .and_then(|pixels| pixels.checked_mul(4))
        else {
            return PublishResult::Rejected;
        };
        if frame.info.surface != SurfaceKind::View
            || frame.info.width == 0
            || frame.info.height == 0
            || len as usize != frame.pixels.len()
        {
            return PublishResult::Rejected;
        }
        *lock(&self.shared) = Some(frame);
        PublishResult::Published
    }
}
impl FrameSink for FrameInputSender {
    fn publish(&self, frame: OwnedFrame) -> PublishResult {
        FrameInputSender::publish(self, frame)
    }
}

struct WndState {
    frames: Arc<Mutex<Option<OwnedFrame>>>,
    messages: Mutex<Vec<InputMessage>>,
    initial_view: (u32, u32),
    close_requested: AtomicBool,
    presented: Mutex<Option<(usize, u64)>>,
    paints: AtomicU64,
    last_painted: Mutex<Option<OwnedFrame>>,
}
impl WndState {
    /// The frame's own size, or the requested size before the first frame.
    fn view_size(&self) -> (u32, u32) {
        let frames = lock(&self.frames);
        frames
            .as_ref()
            .map_or(self.initial_view, |f| (f.info.width, f.info.height))
    }
}

/// One foreground Win32 window owned by the creating thread. `!Send`.
pub struct InputFixture {
    hwnd: HWND,
    state: Arc<WndState>,
    sender: FrameInputSender,
    owner_thread: u32,
    original_foreground: HWND,
    original_cursor: Option<POINT>,
    last_injected_cursor: Cell<Option<POINT>>,
    _not_send: PhantomData<*mut ()>,
}

impl InputFixture {
    /// Creates and activates the window; the client area starts at `width` x
    /// `height` view pixels. No process-wide DPI awareness is changed.
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        let (Ok(w), Ok(h)) = (i32::try_from(width), i32::try_from(height)) else {
            return Err("fixture view size must fit i32".into());
        };
        if width == 0 || height == 0 {
            return Err("fixture view size must be non-zero".into());
        }
        let owner_thread = thread_id();
        // SAFETY: argument-free queries; saved values are restored in Drop.
        let original_foreground = unsafe { GetForegroundWindow() };
        let mut cursor = POINT::default();
        // SAFETY: `cursor` is a writable POINT for this call.
        let original_cursor = (unsafe { GetCursorPos(&mut cursor) } != 0).then_some(cursor);
        // SAFETY: a null module name requests the current executable.
        let instance = unsafe { GetModuleHandleW(ptr::null()) };
        if instance == 0 {
            return Err(failed("GetModuleHandleW failed"));
        }
        let (class_name, title) = (wide("GpuiCefInputFixture"), wide("GPUI-CEF input fixture"));
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: 0x0003, // CS_HREDRAW | CS_VREDRAW
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: 0,
            // SAFETY: IDC_ARROW is an integer resource id, not a real pointer.
            hCursor: unsafe { LoadCursorW(0, IDC_ARROW as *const u16) },
            hbrBackground: 6, // COLOR_WINDOW + 1
            lpszMenuName: ptr::null(),
            lpszClassName: class_name.as_ptr(),
            hIconSm: 0,
        };
        // SAFETY: class and both UTF-16 buffers outlive the call. Same-process
        // reuse of the fixed class name is allowed; no thread/window remains.
        if unsafe { RegisterClassExW(&class) } == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_CLASS_ALREADY_EXISTS {
                return Err(format!("RegisterClassExW failed (error {error})"));
            }
        }
        let sender = FrameInputSender::new();
        let state = Arc::new(WndState {
            frames: sender.shared.clone(),
            messages: Mutex::new(Vec::new()),
            initial_view: (width, height),
            close_requested: AtomicBool::new(false),
            presented: Mutex::new(None),
            paints: AtomicU64::new(0),
            last_painted: Mutex::new(None),
        });
        let raw = Arc::into_raw(state.clone());
        let style = WS_OVERLAPPEDWINDOW | WS_VISIBLE;
        let mut outer = RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        };
        // SAFETY: `outer` is writable; the style values are plain integers.
        if unsafe { AdjustWindowRectEx(&mut outer, style, 0, 0) } == 0 {
            // The window never saw this reference.
            unsafe { drop(Arc::from_raw(raw)) };
            return Err(failed("AdjustWindowRectEx failed"));
        }
        // SAFETY: every pointer is live; `raw` is the single window-owned ref,
        // consumed exactly once by WM_NCDESTROY.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                title.as_ptr(),
                style,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                outer.right - outer.left,
                outer.bottom - outer.top,
                0,
                0,
                instance,
                raw as *mut c_void,
            )
        };
        if hwnd == 0 {
            let error = unsafe { GetLastError() };
            // WM_NCDESTROY consumed `raw` if it ran; otherwise this value still
            // holds the only other strong reference.
            if Arc::strong_count(&state) == 2 {
                unsafe { drop(Arc::from_raw(raw)) };
            }
            return Err(format!("CreateWindowExW failed (error {error})"));
        }
        // SAFETY: `hwnd` was just created on this thread and is still alive.
        unsafe {
            ShowWindow(hwnd, 5 /* SW_SHOW */);
            SetForegroundWindow(hwnd);
            SetFocus(hwnd);
        }
        if unsafe { GetForegroundWindow() } != hwnd {
            // WM_NCDESTROY releases the window's raw reference.
            unsafe { DestroyWindow(hwnd) };
            return Err("fixture window did not become the foreground window".into());
        }
        Ok(Self {
            hwnd,
            sender,
            state,
            owner_thread,
            original_foreground,
            original_cursor,
            last_injected_cursor: Cell::new(None),
            _not_send: PhantomData,
        })
    }

    /// Clone into the Runtime as `Arc<dyn FrameSink>`. Safe from the CEF
    /// thread; it only stores owned pixels.
    pub fn frame_sender(&self) -> FrameInputSender {
        self.sender.clone()
    }

    /// Pumps only this window's messages and returns newly observed input. A
    /// new frame is invalidated first, so the same pump paints it.
    pub fn poll(&self) -> Result<Vec<InputMessage>, String> {
        self.check_owner()?;
        // SAFETY: owner thread; liveness is checked before dispatch.
        if unsafe { IsWindow(self.hwnd) } == 0 {
            return Err("fixture window no longer exists".into());
        }
        self.present_new_frame();
        let mut message = MSG::default();
        // The HWND filter keeps CEF's same-thread messages out of this pump.
        // SAFETY: each removed message is translated and dispatched once.
        while unsafe { PeekMessageW(&mut message, self.hwnd, 0, 0, PM_REMOVE) } != 0 {
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        Ok(std::mem::take(&mut *lock(&self.state.messages)))
    }

    /// Injects a real left click at current view coordinates. A failed check
    /// returns an error; there is no fallback to a direct CEF click.
    pub fn inject_click(&self, x: i32, y: i32) -> Result<(), String> {
        self.check_owner()?;
        if self.close_requested() {
            return Err("fixture window has already received WM_CLOSE".into());
        }
        // SAFETY: read-only window queries on the owning thread.
        if unsafe { IsWindow(self.hwnd) } == 0 || unsafe { IsWindowVisible(self.hwnd) } == 0 {
            return Err("fixture window is not visible".into());
        }
        let (view_w, view_h) = self.state.view_size();
        if x < 0 || y < 0 || x >= view_w as i32 || y >= view_h as i32 {
            return Err(format!(
                "view point ({x}, {y}) is outside {view_w}x{view_h}"
            ));
        }
        let (client_w, client_h) = client_size(self.hwnd)?;
        let mut screen = POINT {
            x: map_view_to_client(x, client_w, view_w)?,
            y: map_view_to_client(y, client_h, view_h)?,
        };
        // SAFETY: `screen` is writable; the window belongs to this thread.
        if unsafe { ClientToScreen(self.hwnd, &mut screen) } == 0 {
            return Err("ClientToScreen failed".into());
        }
        if !point_in_client(self.hwnd, screen)? {
            return Err("mapped screen point is outside the fixture client area".into());
        }
        // SAFETY: read-only query, then activation of our own visible window.
        if unsafe { GetForegroundWindow() } != self.hwnd {
            unsafe { SetForegroundWindow(self.hwnd) };
        }
        if unsafe { GetForegroundWindow() } != self.hwnd {
            return Err("fixture window could not be activated".into());
        }
        if unsafe { WindowFromPoint(screen) } != self.hwnd {
            return Err("fixture client point is covered by another window".into());
        }
        let mut previous = POINT::default();
        let previous = (unsafe { GetCursorPos(&mut previous) } != 0).then_some(previous);
        // SAFETY: best-effort checks confine the intended target to our window.
        // They are not atomic with global SendInput; desktop interference must
        // fail the test rather than be claimed impossible.
        if unsafe { SetCursorPos(screen.x, screen.y) } == 0 {
            return Err(failed("SetCursorPos failed"));
        }
        let mut actual = POINT::default();
        // SAFETY: `actual` is writable; the foreground is re-checked below.
        if unsafe { GetCursorPos(&mut actual) } == 0 || actual != screen {
            return Err("cursor did not reach the requested screen point".into());
        }
        // Recheck immediately before SendInput. Another process can still
        // change focus after this check; the probe must inspect actual WM input.
        if unsafe { GetForegroundWindow() } != self.hwnd
            || !point_in_client(self.hwnd, actual)?
            || unsafe { WindowFromPoint(actual) } != self.hwnd
        {
            self.restore_cursor_if_unchanged(previous, screen);
            return Err("screen point no longer belongs to the fixture window".into());
        }
        let inputs = [
            mouse_input(MOUSEEVENTF_LEFTDOWN),
            mouse_input(MOUSEEVENTF_LEFTUP),
        ];
        // SAFETY: two initialized INPUT values of the asserted x64 size; the
        // fixed magic marks them as fixture input.
        let sent = unsafe { SendInput(2, inputs.as_ptr(), size_of::<INPUT>() as i32) };
        if sent != 2 {
            self.restore_cursor_if_unchanged(previous, screen);
            return Err(format!("SendInput delivered {sent} of 2 events"));
        }
        self.last_injected_cursor.set(Some(screen));
        Ok(())
    }

    fn restore_cursor_if_unchanged(&self, previous: Option<POINT>, injected: POINT) {
        let mut current = POINT::default();
        if let Some(previous) = previous
            && unsafe { GetForegroundWindow() } == self.hwnd
            && unsafe { GetCursorPos(&mut current) } != 0
            && current == injected
        {
            unsafe { SetCursorPos(previous.x, previous.y) };
        }
    }

    pub fn close_requested(&self) -> bool {
        self.state.close_requested.load(Ordering::Acquire)
    }
    /// Successful StretchDIBits calls: a real frame reached the client area,
    /// not merely the CEF sink.
    pub fn painted_frames(&self) -> u64 {
        self.state.paints.load(Ordering::Acquire)
    }

    /// Owned pixels/identity actually copied successfully by WM_PAINT, not just
    /// received by the sink. Lets a probe reject stale previous-Browser paint.
    pub fn painted_frame(&self) -> Option<OwnedFrame> {
        lock(&self.state.last_painted).clone()
    }

    /// Visible top-level windows of this process other than the fixture. This
    /// supports "no unexpected native popup", not full window history.
    pub fn unexpected_visible_windows(&self) -> Result<usize, String> {
        let mut context = EnumContext {
            // SAFETY: argument-free query.
            process: unsafe { GetCurrentProcessId() },
            own: self.hwnd,
            count: 0,
        };
        // SAFETY: the callback only mutates `context` during this call.
        let ok = unsafe {
            EnumWindows(
                Some(enum_visible_window),
                &mut context as *mut EnumContext as LPARAM,
            )
        };
        if ok == 0 {
            return Err(failed("EnumWindows failed"));
        }
        Ok(context.count)
    }

    fn check_owner(&self) -> Result<(), String> {
        if thread_id() == self.owner_thread {
            Ok(())
        } else {
            Err("input fixture must be used from its creating thread".into())
        }
    }
    /// Marks the latest frame dirty once; identity plus sequence avoids a
    /// permanent repaint of the same Arc. `PeekMessage` then synthesizes
    /// `WM_PAINT` for the non-empty update region.
    fn present_new_frame(&self) {
        let Some(frame) = lock(&self.state.frames).clone() else {
            return;
        };
        let marker = (Arc::as_ptr(&frame.pixels) as usize, frame.info.sequence);
        let mut presented = lock(&self.state.presented);
        if *presented == Some(marker) {
            return;
        }
        *presented = Some(marker);
        drop(presented);
        // SAFETY: owner thread, and the caller checked the window is alive.
        unsafe { InvalidateRect(self.hwnd, ptr::null(), 0) };
    }
}

impl Drop for InputFixture {
    fn drop(&mut self) {
        if thread_id() != self.owner_thread {
            // `!Send` makes this unreachable safely; leaking beats destroying a
            // window from another thread.
            return;
        }
        // Restore only if still foreground: never steal focus from the window
        // the user is actually using.
        if unsafe { GetForegroundWindow() } == self.hwnd {
            if let Some(last) = self.last_injected_cursor.get() {
                self.restore_cursor_if_unchanged(self.original_cursor, last);
            }
            let previous = self.original_foreground;
            if previous != 0 && previous != self.hwnd && unsafe { IsWindow(previous) } != 0 {
                unsafe { SetForegroundWindow(previous) };
            }
        }
        // SAFETY: owner thread; WM_NCDESTROY releases the window's Arc once.
        if unsafe { IsWindow(self.hwnd) } != 0 {
            unsafe { DestroyWindow(self.hwnd) };
        }
    }
}

fn client_size(hwnd: HWND) -> Result<(i32, i32), String> {
    let mut rect = RECT::default();
    // SAFETY: `rect` is writable; the window is on this thread.
    if unsafe { GetClientRect(hwnd, &mut rect) } == 0 {
        return Err("GetClientRect failed".into());
    }
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    if width <= 0 || height <= 0 {
        return Err("fixture client area is empty".into());
    }
    Ok((width, height))
}
fn point_in_client(hwnd: HWND, point: POINT) -> Result<bool, String> {
    let (width, height) = client_size(hwnd)?;
    let mut origin = POINT::default();
    // SAFETY: `origin` is writable.
    if unsafe { ClientToScreen(hwnd, &mut origin) } == 0 {
        return Err("ClientToScreen failed".into());
    }
    Ok(point.x >= origin.x
        && point.y >= origin.y
        && point.x < origin.x + width
        && point.y < origin.y + height)
}
fn map_view_to_client(value: i32, client: i32, view: u32) -> Result<i32, String> {
    if client <= 0 || view == 0 {
        return Err("invalid view/client geometry".into());
    }
    // Choose the first physical pixel mapping back to this logical coordinate.
    // A smaller client cannot represent every view pixel: reject, don't misclick.
    let mapped = ((value as i64 * client as i64 + view as i64 - 1) / view as i64) as i32;
    if mapped < 0 || mapped >= client || map_client_to_view(mapped, client, view) != value {
        return Err("requested view coordinate is not representable at this scale".into());
    }
    Ok(mapped)
}

fn map_client_to_view(value: i32, client: i32, view: u32) -> i32 {
    if client <= 0 || view == 0 {
        return 0;
    }
    (value as i64 * view as i64 / client as i64).clamp(0, view as i64 - 1) as i32
}
fn marker_flags(extra: usize) -> (bool, bool, bool) {
    (
        (extra & !1) == (MAGIC & !1),
        (extra as u32 & !1) == (MAGIC as u32 & !1),
        extra == 0,
    )
}

fn mouse_input(flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        u: INPUT_UNION {
            mouse: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: MAGIC,
            },
        },
    }
}

/// The window owns the only strong reference from `Arc::into_raw` until
/// WM_NCDESTROY, so the borrow is valid for every message of this window.
unsafe fn window_state(hwnd: HWND) -> Option<&'static WndState> {
    // SAFETY: see the invariant above.
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WndState;
    if raw.is_null() {
        None
    } else {
        Some(unsafe { &*raw })
    }
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Capture the message-associated metadata before other user32 queries or
    // any nested window processing can replace the thread's extra-info state.
    let mouse_metadata = if matches!(message, WM_LBUTTONDOWN | WM_LBUTTONUP) {
        let extra = unsafe { GetMessageExtraInfo() } as usize;
        let mut source = INPUT_MESSAGE_SOURCE::default();
        let injected = unsafe { GetCurrentInputMessageSource(&mut source) } != 0
            && source.deviceType == 2
            && source.originId == 2;
        Some((extra, injected))
    } else {
        None
    };
    if message == WM_NCCREATE {
        // SAFETY: lParam is a live CREATESTRUCTW; only its first field is read.
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
    }
    if message == WM_NCDESTROY {
        let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WndState;
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
        let result = unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        if !raw.is_null() {
            // SAFETY: consumes the window's single strong reference.
            unsafe { drop(Arc::from_raw(raw)) };
        }
        return result;
    }
    let Some(state) = (unsafe { window_state(hwnd) }) else {
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    };
    match message {
        WM_LBUTTONDOWN | WM_LBUTTONUP => {
            if let Ok((client_w, client_h)) = client_size(hwnd) {
                let (view_w, view_h) = state.view_size();
                let x = (lparam as u32 & 0xffff) as i16 as i32;
                let y = ((lparam as u32 >> 16) & 0xffff) as i16 as i32;
                let (extra, injected_origin) = mouse_metadata.unwrap_or_default();
                let (marker_match, low32_marker_match, extra_info_zero) = marker_flags(extra);
                lock(&state.messages).push(InputMessage::MouseClick {
                    x: map_client_to_view(x, client_w, view_w),
                    y: map_client_to_view(y, client_h, view_h),
                    up: message == WM_LBUTTONUP,
                    os_injected: marker_match,
                    low32_marker_match,
                    extra_info_zero,
                    injected_origin,
                });
            }
            0
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            lock(&state.messages).push(InputMessage::Focus(message == WM_SETFOCUS));
            0
        }
        WM_CLOSE => {
            // Do not destroy early: the parent closes the Runtime first.
            state.close_requested.store(true, Ordering::Release);
            0
        }
        WM_PAINT => {
            unsafe { paint(hwnd, state) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

unsafe fn paint(hwnd: HWND, state: &WndState) {
    let mut paint = PAINTSTRUCT::default();
    // SAFETY: inside WM_PAINT, `paint` is a valid out-parameter.
    let hdc = unsafe { BeginPaint(hwnd, &mut paint) };
    if hdc == 0 {
        return;
    }
    let frame = lock(&state.frames).clone();
    let mut rect = RECT::default();
    // SAFETY: writable RECT; the window is alive for this message.
    let have_client = unsafe { GetClientRect(hwnd, &mut rect) } != 0;
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    if let Some(frame) = frame
        && have_client
        && width > 0
        && height > 0
        && frame.pixels.len() == frame.info.width as usize * frame.info.height as usize * 4
    {
        let (frame_width, frame_height) = (frame.info.width as i32, frame.info.height as i32);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: frame_width,
                biHeight: -frame_height, // top-down BGRA rows
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: frame.pixels.len() as u32,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [0],
        };
        // SAFETY: pixels match the header and `info` lives for the call.
        // Stretching maps frame pixels onto the real client size at any DPI.
        let copied = unsafe {
            StretchDIBits(
                hdc,
                0,
                0,
                width,
                height,
                0,
                0,
                frame_width,
                frame_height,
                frame.pixels.as_ptr() as *const c_void,
                &info,
                DIB_RGB_COLORS,
                SRCCOPY,
            )
        };
        if copied > 0 && copied != GDI_ERROR {
            *lock(&state.last_painted) = Some(frame.clone());
            state.paints.fetch_add(1, Ordering::Release);
        }
    }
    // SAFETY: pairs with the successful BeginPaint above.
    unsafe { EndPaint(hwnd, &paint) };
}

struct EnumContext {
    process: u32,
    own: HWND,
    count: usize,
}
unsafe extern "system" fn enum_visible_window(hwnd: HWND, lparam: LPARAM) -> i32 {
    // SAFETY: EnumWindows hands back the caller's context, live for the enum.
    let context = unsafe { &mut *(lparam as *mut EnumContext) };
    if hwnd != context.own && unsafe { IsWindowVisible(hwnd) } != 0 {
        let mut process = 0;
        // SAFETY: `process` is writable; the thread-id return is ignored.
        unsafe { GetWindowThreadProcessId(hwnd, &mut process) };
        if process == context.process {
            context.count += 1;
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn representable_marker_keeps_full_pointer_comparison_and_rejects_high_word_changes() {
        assert!(MAGIC <= i32::MAX as usize);
        assert_eq!(marker_flags(MAGIC), (true, true, false));
        assert_eq!(marker_flags(MAGIC | 1), (true, true, false));
        assert_eq!(
            marker_flags(MAGIC | 0x4750_5549_0000_0000),
            (false, true, false)
        );
        assert_eq!(marker_flags(0), (false, false, true));
        assert_eq!(std::mem::offset_of!(MOUSEINPUT, dwExtraInfo), 24);
        assert_eq!(std::mem::offset_of!(INPUT, u), 8);
    }
    #[test]
    fn accepted_view_coordinates_roundtrip_at_noninteger_dpi_scale() {
        for (view, client) in [(320, 400), (800, 1000), (800, 600)] {
            for point in [0, 1, view / 2, view - 1] {
                if let Ok(pixel) = map_view_to_client(point, client, view as u32) {
                    assert_eq!(map_client_to_view(pixel, client, view as u32), point);
                }
            }
        }
        assert_eq!(map_view_to_client(1, 1000, 800), Ok(2));
        assert!(map_view_to_client(3, 600, 800).is_err());
    }
}
