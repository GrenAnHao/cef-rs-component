//! Validation-only owned HWND/property facts. No input, focus or CEF calls.
use std::{marker::PhantomData, rc::Rc};
const NAME: &[u16] = &[
    71, 80, 85, 73, 95, 67, 69, 70, 95, 79, 76, 69, 95, 84, 97, 114, 103, 101, 116, 95, 82, 53, 56,
    0,
];
#[repr(C)]
#[derive(Default)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}
#[repr(C)]
#[derive(Default)]
struct Point {
    x: i32,
    y: i32,
}
#[link(name = "user32")]
unsafe extern "system" {
    fn EnumWindows(callback: unsafe extern "system" fn(isize, isize) -> i32, data: isize) -> i32;
    fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
    fn GetWindowTextLengthW(hwnd: isize) -> i32;
    fn GetWindowTextW(hwnd: isize, text: *mut u16, max: i32) -> i32;
    fn IsWindow(hwnd: isize) -> i32;
    fn IsWindowVisible(hwnd: isize) -> i32;
    fn IsIconic(hwnd: isize) -> i32;
    fn GetAncestor(hwnd: isize, flags: u32) -> isize;
    fn SetPropW(hwnd: isize, key: *const u16, value: isize) -> i32;
    fn GetPropW(hwnd: isize, key: *const u16) -> isize;
    fn RemovePropW(hwnd: isize, key: *const u16) -> isize;
    fn GetClientRect(hwnd: isize, rect: *mut Rect) -> i32;
    fn ClientToScreen(hwnd: isize, point: *mut Point) -> i32;
    fn GetDpiForWindow(hwnd: isize) -> u32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
}
struct Find {
    pid: u32,
    thread: u32,
    title: Vec<u16>,
    found: [isize; 2],
    count: usize,
}
unsafe extern "system" fn visit(hwnd: isize, data: isize) -> i32 {
    let find = unsafe { &mut *(data as *mut Find) };
    let mut pid = 0;
    let thread = unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    // NEVER read foreign title/path: first qualify OUR pid and original UI thread.
    if pid != find.pid || thread != find.thread || unsafe { IsWindowVisible(hwnd) } == 0 {
        return 1;
    }
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len < 0 || len as usize != find.title.len() || len >= 1024 {
        return 1;
    }
    let mut text = [0u16; 1024];
    let got = unsafe { GetWindowTextW(hwnd, text.as_mut_ptr(), 1024) };
    if got == len && text[..got as usize] == find.title {
        if find.count < 2 {
            find.found[find.count] = hwnd;
        }
        find.count += 1;
        if find.count >= 2 {
            return 0;
        }
    }
    1
}
pub(crate) struct OwnedOleTarget {
    hwnd: isize,
    pid: u32,
    thread: u32,
    nonce: isize,
    _ui: PhantomData<Rc<()>>,
}
#[derive(PartialEq, Eq)]
pub(crate) struct Facts {
    pub hwnd: isize,
    pub pid: u32,
    pub thread: u32,
    pub nonce: isize,
    pub dpi: u32,
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}
impl Facts {
    pub fn json(&self) -> String {
        format!(
            "{{\"hwnd\":{},\"pid\":{},\"thread\":{},\"nonce\":{},\"property\":\"GPUI_CEF_OLE_Target_R58\",\"dpi\":{},\"client_left\":{},\"client_top\":{},\"client_width\":{},\"client_height\":{}}}",
            self.hwnd,
            self.pid,
            self.thread,
            self.nonce,
            self.dpi,
            self.left,
            self.top,
            self.width,
            self.height
        )
    }
}
impl OwnedOleTarget {
    pub fn install(title: &str, nonce: i32) -> Result<Self, String> {
        if nonce <= 0 {
            return Err("positive target nonce required".into());
        }
        let thread = unsafe { GetCurrentThreadId() };
        let pid = std::process::id();
        let mut f = Find {
            pid,
            thread,
            title: title.encode_utf16().collect(),
            found: [0; 2],
            count: 0,
        };
        if f.title.is_empty() || f.title.len() >= 1024 {
            return Err("bounded target title required".into());
        }
        let enumerated = unsafe { EnumWindows(visit, &mut f as *mut _ as isize) };
        if enumerated == 0 && f.count < 2 {
            return Err("owned target window enumeration failed".into());
        }
        if f.count != 1 {
            return Err("owned target root HWND not unique".into());
        }
        let hwnd = f.found[0];
        if unsafe { GetAncestor(hwnd, 2) } != hwnd || unsafe { GetPropW(hwnd, NAME.as_ptr()) } != 0
        {
            return Err("owned target root/property unavailable".into());
        }
        if unsafe { SetPropW(hwnd, NAME.as_ptr(), nonce as isize) } == 0 {
            return Err("owned target property install failed".into());
        }
        let owner = Self {
            hwnd,
            pid,
            thread,
            nonce: nonce as isize,
            _ui: PhantomData,
        };
        owner.facts()?;
        Ok(owner)
    }
    pub fn facts(&self) -> Result<Facts, String> {
        if unsafe { GetCurrentThreadId() } != self.thread {
            return Err("wrong target UI thread".into());
        }
        let mut pid = 0;
        let thread = unsafe { GetWindowThreadProcessId(self.hwnd, &mut pid) };
        if unsafe { IsWindow(self.hwnd) } == 0
            || pid != self.pid
            || thread != self.thread
            || unsafe { GetPropW(self.hwnd, NAME.as_ptr()) } != self.nonce
            || unsafe { IsWindowVisible(self.hwnd) } == 0
            || unsafe { IsIconic(self.hwnd) } != 0
            || unsafe { GetAncestor(self.hwnd, 2) } != self.hwnd
        {
            return Err("target HWND lifecycle/identity changed".into());
        }
        let mut r = Rect::default();
        let mut p = Point::default();
        if unsafe { GetClientRect(self.hwnd, &mut r) } == 0
            || unsafe { ClientToScreen(self.hwnd, &mut p) } == 0
            || r.right <= r.left
            || r.bottom <= r.top
        {
            return Err("target client geometry unavailable".into());
        }
        let dpi = unsafe { GetDpiForWindow(self.hwnd) };
        if dpi == 0 {
            return Err("target DPI unavailable".into());
        }
        Ok(Facts {
            hwnd: self.hwnd,
            pid,
            thread,
            nonce: self.nonce,
            dpi,
            left: p.x,
            top: p.y,
            width: r.right - r.left,
            height: r.bottom - r.top,
        })
    }
}
impl Drop for OwnedOleTarget {
    fn drop(&mut self) {
        let mut pid = 0;
        let thread = unsafe { GetWindowThreadProcessId(self.hwnd, &mut pid) };
        if unsafe { GetCurrentThreadId() } == self.thread
            && pid == self.pid
            && thread == self.thread
            && unsafe { IsWindow(self.hwnd) } != 0
            && unsafe { GetPropW(self.hwnd, NAME.as_ptr()) } == self.nonce
        {
            unsafe {
                RemovePropW(self.hwnd, NAME.as_ptr());
            }
        }
    }
}
