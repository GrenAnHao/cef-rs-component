//! Read-only native lifetime proof for the exact GPUI-created source window.
//! No search, title, input, foreground, reparenting or native close operation.
use gpui::Window;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::ffi::c_void;
type Handle = *mut c_void;
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcessId() -> u32;
    fn GetCurrentThreadId() -> u32;
}
#[link(name = "user32")]
unsafe extern "system" {
    fn IsWindow(hwnd: Handle) -> i32;
    fn GetWindowThreadProcessId(hwnd: Handle, pid: *mut u32) -> u32;
    fn GetAncestor(hwnd: Handle, kind: u32) -> Handle;
}
#[derive(Clone, Copy)]
pub(super) struct WindowProof {
    hwnd: Handle,
    thread: u32,
}
impl WindowProof {
    pub fn capture(window: &Window) -> Option<Self> {
        let RawWindowHandle::Win32(raw) = HasWindowHandle::window_handle(window).ok()?.as_raw()
        else {
            return None;
        };
        let hwnd = raw.hwnd.get() as Handle;
        let mut pid = 0;
        let thread = unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if thread == 0
            || thread != unsafe { GetCurrentThreadId() }
            || pid != unsafe { GetCurrentProcessId() }
            || unsafe { IsWindow(hwnd) } == 0
            || unsafe { GetAncestor(hwnd, 2) } != hwnd
        {
            return None;
        }
        Some(Self { hwnd, thread })
    }
    pub fn absent(&self) -> bool {
        // If a handle has already been reused, IsWindow is true and the conservative
        // barrier stays set. Never inspect or modify that possibly foreign window.
        self.thread == unsafe { GetCurrentThreadId() } && unsafe { IsWindow(self.hwnd) } == 0
    }
}
