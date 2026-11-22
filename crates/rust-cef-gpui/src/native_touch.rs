//! Explicit host-owned Win32 `WM_POINTER*` touch ingress for GPUI `CefBrowser` views.
//!
//! Mirrors `native_keyboard.rs` teardown hygiene: no global locator, explicit
//! thread+process lease mutex, per-window private property routing, and ordered
//! `RemovePropW` (verified) -> `UnhookWindowsHookEx` -> `ReleaseMutex`/`CloseHandle`
//! retirement with synthetic `Cancelled` events for all active touch pointers.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    ffi::c_void,
    marker::PhantomData,
    num::NonZeroUsize,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    rc::{Rc, Weak},
};

use gpui::{AnyWindowHandle, App, AppContext as _, AsyncApp, Entity, WeakEntity, Window};
use raw_window_handle::RawWindowHandle;
use rust_cef_core::{
    input::{BrowserTouchEvent, TouchEventType, TouchPointerType},
    ports::{BrowserCommand, BrowserHandle, EngineError, MouseModifiers},
};

use crate::{binding::WindowToken, view::CefBrowser};

type Handle = *mut c_void;
const MAX_ROUTES: usize = 256;
const WH_GETMESSAGE: i32 = 3;
const PM_REMOVE: usize = 1;
const WAIT_OBJECT_0: u32 = 0;

pub const WM_POINTERUPDATE: u32 = 0x0245;
pub const WM_POINTERDOWN: u32 = 0x0246;
pub const WM_POINTERUP: u32 = 0x0247;
pub const WM_POINTERCAPTURECHANGED: u32 = 0x024C;

pub const PT_TOUCH: u32 = 2;
pub const PT_PEN: u32 = 3;
pub const PT_MOUSE: u32 = 4;
pub const PT_TOUCHPAD: u32 = 5;

pub const POINTER_FLAG_CANCELED: u32 = 0x0000_0400;
const POINTER_MOD_SHIFT: u32 = 0x0004;
const POINTER_MOD_CTRL: u32 = 0x0008;

pub const TOUCH_MASK_CONTACTAREA: u32 = 0x0001;
pub const TOUCH_MASK_ORIENTATION: u32 = 0x0002;
pub const TOUCH_MASK_PRESSURE: u32 = 0x0004;

/// `"GpuiCefTouchRoute1\0"`
const PROPERTY: &[u16] = &[
    0x47, 0x70, 0x75, 0x69, 0x43, 0x65, 0x66, 0x54, 0x6f, 0x75, 0x63, 0x68, 0x52, 0x6f, 0x75, 0x74,
    0x65, 0x31, 0,
];

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Msg {
    hwnd: Handle,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    pt: Point,
    private: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PointerInfo {
    pub pointer_type: u32,
    pub pointer_id: u32,
    pub frame_id: u32,
    pub pointer_flags: u32,
    pub source_device: Handle,
    pub hwnd_target: Handle,
    pub pt_pixel_location: Point,
    pub pt_himetric_location: Point,
    pub pt_pixel_location_raw: Point,
    pub pt_himetric_location_raw: Point,
    pub dw_time: u32,
    pub history_count: u32,
    pub input_data: i32,
    pub dw_key_states: u32,
    pub performance_count: u64,
    pub button_change_type: i32,
}

impl Default for PointerInfo {
    fn default() -> Self {
        Self {
            pointer_type: 0,
            pointer_id: 0,
            frame_id: 0,
            pointer_flags: 0,
            source_device: ptr::null_mut(),
            hwnd_target: ptr::null_mut(),
            pt_pixel_location: Point::default(),
            pt_himetric_location: Point::default(),
            pt_pixel_location_raw: Point::default(),
            pt_himetric_location_raw: Point::default(),
            dw_time: 0,
            history_count: 0,
            input_data: 0,
            dw_key_states: 0,
            performance_count: 0,
            button_change_type: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PointerTouchInfo {
    pub pointer_info: PointerInfo,
    pub touch_flags: u32,
    pub touch_mask: u32,
    pub rc_contact: Rect,
    pub rc_contact_raw: Rect,
    pub orientation: u32,
    pub pressure: u32,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn SetWindowsHookExW(
        id: i32,
        callback: Option<unsafe extern "system" fn(i32, usize, isize) -> isize>,
        module: Handle,
        thread: u32,
    ) -> Handle;
    fn CallNextHookEx(hook: Handle, code: i32, wparam: usize, lparam: isize) -> isize;
    fn UnhookWindowsHookEx(hook: Handle) -> i32;
    fn SetPropW(hwnd: Handle, name: *const u16, value: Handle) -> i32;
    fn GetPropW(hwnd: Handle, name: *const u16) -> Handle;
    fn RemovePropW(hwnd: Handle, name: *const u16) -> Handle;
    fn GetWindowThreadProcessId(hwnd: Handle, process_id: *mut u32) -> u32;
    fn ScreenToClient(hwnd: Handle, point: *mut Point) -> i32;
    fn GetPointerInfo(pointer_id: u32, pointer_info: *mut PointerInfo) -> i32;
    fn GetPointerTouchInfo(pointer_id: u32, touch_info: *mut PointerTouchInfo) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
    fn GetCurrentProcessId() -> u32;
    fn GetLastError() -> u32;
    fn CreateMutexW(attrs: Handle, initially_owned: i32, name: *const u16) -> Handle;
    fn WaitForSingleObject(handle: Handle, millis: u32) -> u32;
    fn ReleaseMutex(handle: Handle) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeTouchError {
    WrongThread,
    LeaseUnavailable,
    HookUnavailable,
    InvalidWindow,
    DuplicateWindow,
    RouteCapacity,
    PropertyUnavailable,
    BindingRetired,
    Retired,
    Busy,
    RetirementUnconfirmed,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NativeTouchDiagnostics {
    pub received_messages: u64,
    pub routed_events: u64,
    pub refused: u64,
    pub cancelled_pointers: u64,
    pub panics: u64,
    pub last_error: Option<EngineError>,
    pub hook_live: bool,
    pub properties_live: usize,
    pub lease_live: bool,
    pub retired: bool,
}

pub fn classify_pointer_message(msg: u32, pointer_flags: u32) -> Option<TouchEventType> {
    if msg == WM_POINTERCAPTURECHANGED {
        return Some(TouchEventType::Cancelled);
    }
    if (pointer_flags & POINTER_FLAG_CANCELED) != 0 {
        return match msg {
            WM_POINTERDOWN | WM_POINTERUPDATE | WM_POINTERUP => Some(TouchEventType::Cancelled),
            _ => None,
        };
    }
    match msg {
        WM_POINTERDOWN => Some(TouchEventType::Pressed),
        WM_POINTERUPDATE => Some(TouchEventType::Moved),
        WM_POINTERUP => Some(TouchEventType::Released),
        _ => None,
    }
}

pub fn pointer_to_touch_event(
    msg: u32,
    info: &PointerInfo,
    touch: Option<&PointerTouchInfo>,
    client_pt: (i32, i32),
    scale: f32,
) -> Option<BrowserTouchEvent> {
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let pointer_type = match info.pointer_type {
        PT_TOUCH => TouchPointerType::Touch,
        PT_PEN => TouchPointerType::Pen,
        _ => return None,
    };
    let event_type = classify_pointer_message(msg, info.pointer_flags)?;
    let id = i32::try_from(info.pointer_id).ok().filter(|&v| v >= 0)?;
    let x = client_pt.0 as f32 / scale;
    let y = client_pt.1 as f32 / scale;
    if !x.is_finite() || !y.is_finite() {
        return None;
    }

    let (mut radius_x, mut radius_y) = (0.0f32, 0.0f32);
    let mut rotation_angle = 0.0f32;
    let mut pressure = match event_type {
        TouchEventType::Pressed | TouchEventType::Moved => 1.0f32,
        TouchEventType::Released | TouchEventType::Cancelled => 0.0f32,
    };

    if let Some(touch) = touch {
        if (touch.touch_mask & TOUCH_MASK_CONTACTAREA) != 0 {
            let w = (touch.rc_contact.right - touch.rc_contact.left).max(0) as f32;
            let h = (touch.rc_contact.bottom - touch.rc_contact.top).max(0) as f32;
            radius_x = w / (2.0 * scale);
            radius_y = h / (2.0 * scale);
        }
        if (touch.touch_mask & TOUCH_MASK_ORIENTATION) != 0 {
            rotation_angle = (touch.orientation as f32) % 360.0;
        }
        if (touch.touch_mask & TOUCH_MASK_PRESSURE) != 0 {
            pressure = (touch.pressure as f32 / 1024.0).clamp(0.0, 1.0);
        }
    }

    let modifiers = MouseModifiers {
        shift: (info.dw_key_states & POINTER_MOD_SHIFT) != 0,
        control: (info.dw_key_states & POINTER_MOD_CTRL) != 0,
        alt: false,
        platform: false,
    };

    BrowserTouchEvent {
        id,
        x,
        y,
        radius_x,
        radius_y,
        rotation_angle,
        pressure,
        event_type,
        modifiers,
        pointer_type,
    }
    .validate_and_normalize()
    .ok()
}

struct ServiceState {
    thread: u32,
    active: Cell<bool>,
    dispatching: Cell<bool>,
    aborted: Cell<bool>,
    diagnostics: RefCell<NativeTouchDiagnostics>,
    routes: RefCell<Vec<Weak<TouchRoute>>>,
}

struct TouchRoute {
    hwnd: Handle,
    window_id: u64,
    view: WeakEntity<CefBrowser>,
    token: WindowToken,
    browser: BrowserHandle,
    active_pointers: RefCell<BTreeMap<i32, BrowserTouchEvent>>,
}

struct WindowRoutes {
    hwnd: Handle,
    window_handle: AnyWindowHandle,
    app: AsyncApp,
    service: Rc<ServiceState>,
}

impl WindowRoutes {
    fn routes(&self) -> Vec<Rc<TouchRoute>> {
        self.service
            .routes
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|route| route.hwnd == self.hwnd)
            .collect()
    }

    fn process_pointer(&self, msg: u32, pointer_id: u32) {
        if !self.service.active.get() || self.service.dispatching.get() {
            self.service.diagnostics.borrow_mut().refused += 1;
            return;
        }
        self.service.diagnostics.borrow_mut().received_messages += 1;
        let mut info = PointerInfo::default();
        if unsafe { GetPointerInfo(pointer_id, &mut info) } == 0 {
            self.service.diagnostics.borrow_mut().refused += 1;
            return;
        }
        if info.pointer_type != PT_TOUCH && info.pointer_type != PT_PEN {
            return;
        }
        let mut touch_info = PointerTouchInfo::default();
        let has_touch = info.pointer_type == PT_TOUCH
            && unsafe { GetPointerTouchInfo(pointer_id, &mut touch_info) } != 0;
        let mut client_pt = info.pt_pixel_location;
        if unsafe { ScreenToClient(self.hwnd, &mut client_pt) } == 0 {
            self.service.diagnostics.borrow_mut().refused += 1;
            return;
        }

        let routes = self.routes();
        if routes.is_empty() {
            return;
        }
        self.service.dispatching.set(true);
        let mut app = self.app.clone();
        let result = app.update_window(self.window_handle, |_, window, app| {
            let scale = window.scale_factor();
            let Some(mut event) = pointer_to_touch_event(
                msg,
                &info,
                has_touch.then_some(&touch_info),
                (client_pt.x, client_pt.y),
                scale,
            ) else {
                return;
            };
            for route in &routes {
                if !route.token.matches_window(route.window_id) {
                    continue;
                }
                let tracked = route.active_pointers.borrow().contains_key(&event.id);
                if event.event_type != TouchEventType::Pressed && !tracked {
                    continue;
                }
                let Some(view) = route.view.upgrade() else {
                    continue;
                };
                match view.update(app, |view, _| view.native_touch_event(event, window)) {
                    Ok(_) => {
                        let mut active = route.active_pointers.borrow_mut();
                        match event.event_type {
                            TouchEventType::Pressed | TouchEventType::Moved => {
                                active.insert(event.id, event);
                            }
                            TouchEventType::Released | TouchEventType::Cancelled => {
                                active.remove(&event.id);
                            }
                        }
                        self.service.diagnostics.borrow_mut().routed_events += 1;
                        break;
                    }
                    Err(err) => {
                        if tracked
                            && matches!(
                                event.event_type,
                                TouchEventType::Released | TouchEventType::Cancelled
                            )
                        {
                            event.event_type = TouchEventType::Cancelled;
                            let _ = route.browser.submit(BrowserCommand::TouchEvent(event));
                            route.active_pointers.borrow_mut().remove(&event.id);
                            self.service.diagnostics.borrow_mut().cancelled_pointers += 1;
                        }
                        self.service.diagnostics.borrow_mut().last_error = Some(match err {
                            crate::ViewError::Input(e) => e,
                            _ => EngineError::NotReady,
                        });
                    }
                }
            }
        });
        self.service.dispatching.set(false);
        if self.service.aborted.get() {
            return;
        }
        if result.is_err() {
            // App was mutably borrowed (e.g. integrated pump inside update); never re-enter GPUI.
            self.service.diagnostics.borrow_mut().refused += 1;
            if let Ok(pid) = i32::try_from(pointer_id) {
                for route in &routes {
                    if let Some(mut last) = route.active_pointers.borrow_mut().remove(&pid) {
                        last.event_type = TouchEventType::Cancelled;
                        let _ = route.browser.submit(BrowserCommand::TouchEvent(last));
                        self.service.diagnostics.borrow_mut().cancelled_pointers += 1;
                    }
                }
            }
        }
    }
}

pub struct NativeTouchServices {
    service: Rc<ServiceState>,
    routes: Vec<Rc<TouchRoute>>,
    windows: Vec<Rc<WindowRoutes>>,
    capacity: usize,
    hook: Handle,
    lease: Handle,
    lease_owned: bool,
    _ui_thread: PhantomData<Rc<()>>,
}

impl NativeTouchServices {
    pub fn new(capacity: NonZeroUsize) -> Result<Self, NativeTouchError> {
        if capacity.get() > MAX_ROUTES {
            return Err(NativeTouchError::RouteCapacity);
        }
        let thread = unsafe { GetCurrentThreadId() };
        let name: Vec<u16> = format!(
            "Local\\GpuiCefTouchV1-{}-{}",
            unsafe { GetCurrentProcessId() },
            thread
        )
        .encode_utf16()
        .chain(Some(0))
        .collect();
        let lease = unsafe { CreateMutexW(ptr::null_mut(), 0, name.as_ptr()) };
        if lease.is_null() {
            return Err(NativeTouchError::LeaseUnavailable);
        }
        let existed = unsafe { GetLastError() } == 183; // ERROR_ALREADY_EXISTS
        if existed || unsafe { WaitForSingleObject(lease, 0) } != WAIT_OBJECT_0 {
            unsafe {
                CloseHandle(lease);
            }
            return Err(NativeTouchError::LeaseUnavailable);
        }
        let hook = unsafe {
            SetWindowsHookExW(
                WH_GETMESSAGE,
                Some(get_message_hook),
                ptr::null_mut(),
                thread,
            )
        };
        if hook.is_null() {
            unsafe {
                ReleaseMutex(lease);
                CloseHandle(lease);
            }
            return Err(NativeTouchError::HookUnavailable);
        }
        let service = Rc::new(ServiceState {
            thread,
            active: Cell::new(true),
            dispatching: Cell::new(false),
            aborted: Cell::new(false),
            diagnostics: RefCell::new(NativeTouchDiagnostics {
                hook_live: true,
                lease_live: true,
                ..NativeTouchDiagnostics::default()
            }),
            routes: RefCell::new(Vec::with_capacity(capacity.get())),
        });
        Ok(Self {
            service,
            routes: Vec::with_capacity(capacity.get()),
            windows: Vec::with_capacity(capacity.get()),
            capacity: capacity.get(),
            hook,
            lease,
            lease_owned: true,
            _ui_thread: PhantomData,
        })
    }

    pub fn diagnostics(&self) -> NativeTouchDiagnostics {
        self.service.diagnostics.borrow().clone()
    }

    pub fn attach(
        &mut self,
        view: &Entity<CefBrowser>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), NativeTouchError> {
        if unsafe { GetCurrentThreadId() } != self.service.thread {
            return Err(NativeTouchError::WrongThread);
        }
        if !self.service.active.get() {
            return Err(NativeTouchError::Retired);
        }
        if self.routes.len() == self.capacity {
            return Err(NativeTouchError::RouteCapacity);
        }
        let RawWindowHandle::Win32(raw) = raw_window_handle::HasWindowHandle::window_handle(window)
            .map_err(|_| NativeTouchError::InvalidWindow)?
            .as_raw()
        else {
            return Err(NativeTouchError::InvalidWindow);
        };
        let hwnd = raw.hwnd.get() as Handle;
        let mut pid = 0;
        if hwnd.is_null()
            || unsafe { GetWindowThreadProcessId(hwnd, &mut pid) } != self.service.thread
            || pid != unsafe { GetCurrentProcessId() }
        {
            return Err(NativeTouchError::InvalidWindow);
        }
        let (token, bound_window, browser) = view.read(cx).native_touch_attachment();
        if bound_window != window.window_handle()
            || !token.matches_window(window.window_handle().window_id().as_u64())
        {
            return Err(NativeTouchError::BindingRetired);
        }
        if self
            .routes
            .iter()
            .any(|route| route.token.binding().browser == token.binding().browser)
        {
            return Err(NativeTouchError::DuplicateWindow);
        }
        let existing = self
            .windows
            .iter()
            .find(|group| group.hwnd == hwnd)
            .cloned();
        if let Some(group) = &existing {
            if unsafe { GetPropW(hwnd, PROPERTY.as_ptr()) } != Rc::as_ptr(group) as Handle {
                return Err(NativeTouchError::PropertyUnavailable);
            }
        } else if !unsafe { GetPropW(hwnd, PROPERTY.as_ptr()) }.is_null() {
            return Err(NativeTouchError::DuplicateWindow);
        }

        let route = Rc::new(TouchRoute {
            hwnd,
            window_id: window.window_handle().window_id().as_u64(),
            view: view.downgrade(),
            token,
            browser,
            active_pointers: RefCell::new(BTreeMap::new()),
        });
        if existing.is_none() {
            let group = Rc::new(WindowRoutes {
                hwnd,
                window_handle: window.window_handle(),
                app: cx.to_async(),
                service: self.service.clone(),
            });
            let pointer = Rc::as_ptr(&group) as Handle;
            if unsafe { SetPropW(hwnd, PROPERTY.as_ptr(), pointer) } == 0 {
                return Err(NativeTouchError::PropertyUnavailable);
            }
            self.windows.push(group);
            self.service.diagnostics.borrow_mut().properties_live += 1;
            if unsafe { GetPropW(hwnd, PROPERTY.as_ptr()) } != pointer {
                return Err(NativeTouchError::PropertyUnavailable);
            }
        }
        self.service.routes.borrow_mut().push(Rc::downgrade(&route));
        self.routes.push(route);
        Ok(())
    }

    /// Cancels active pointers, verifies window property removal, unhooks, and releases the lease.
    pub fn retire(&mut self) -> Result<NativeTouchDiagnostics, NativeTouchError> {
        if unsafe { GetCurrentThreadId() } != self.service.thread {
            return Err(NativeTouchError::WrongThread);
        }
        if self.service.dispatching.get() {
            return Err(NativeTouchError::Busy);
        }
        self.service.active.set(false);
        for route in &self.routes {
            let remaining: Vec<BrowserTouchEvent> =
                route.active_pointers.borrow_mut().drain_filter_all();
            for mut ev in remaining {
                ev.event_type = TouchEventType::Cancelled;
                let _ = route.browser.submit(BrowserCommand::TouchEvent(ev));
                self.service.diagnostics.borrow_mut().cancelled_pointers += 1;
            }
        }
        for group in &self.windows {
            let pointer = Rc::as_ptr(group) as Handle;
            if unsafe { GetPropW(group.hwnd, PROPERTY.as_ptr()) } == pointer {
                unsafe {
                    RemovePropW(group.hwnd, PROPERTY.as_ptr());
                }
                if unsafe { GetPropW(group.hwnd, PROPERTY.as_ptr()) } == pointer {
                    return Err(NativeTouchError::RetirementUnconfirmed);
                }
            }
        }
        self.service.diagnostics.borrow_mut().properties_live = 0;
        if !self.hook.is_null() {
            if unsafe { UnhookWindowsHookEx(self.hook) } == 0 {
                return Err(NativeTouchError::RetirementUnconfirmed);
            }
            self.hook = ptr::null_mut();
            self.service.diagnostics.borrow_mut().hook_live = false;
        }
        if !self.lease.is_null() {
            if self.lease_owned {
                if unsafe { ReleaseMutex(self.lease) } == 0 {
                    return Err(NativeTouchError::RetirementUnconfirmed);
                }
                self.lease_owned = false;
            }
            if unsafe { CloseHandle(self.lease) } == 0 {
                return Err(NativeTouchError::RetirementUnconfirmed);
            }
            self.lease = ptr::null_mut();
            self.service.diagnostics.borrow_mut().lease_live = false;
        }
        self.routes.clear();
        self.windows.clear();
        self.service.routes.borrow_mut().clear();
        self.service.diagnostics.borrow_mut().retired = true;
        Ok(self.diagnostics())
    }
}

trait DrainValues<V> {
    fn drain_filter_all(&mut self) -> Vec<V>;
}

impl<K: Ord, V> DrainValues<V> for BTreeMap<K, V> {
    fn drain_filter_all(&mut self) -> Vec<V> {
        std::mem::take(self).into_values().collect()
    }
}

impl Drop for NativeTouchServices {
    fn drop(&mut self) {
        if self.retire().is_err() {
            self.service.active.set(false);
            self.service.aborted.set(true);
            for group in &self.windows {
                let pointer = Rc::as_ptr(group) as Handle;
                if unsafe { GetPropW(group.hwnd, PROPERTY.as_ptr()) } == pointer {
                    unsafe {
                        RemovePropW(group.hwnd, PROPERTY.as_ptr());
                    }
                }
            }
            for group in self.windows.drain(..) {
                if unsafe { GetPropW(group.hwnd, PROPERTY.as_ptr()) }
                    == Rc::as_ptr(&group) as Handle
                {
                    let _ = Rc::into_raw(group);
                }
            }
            for route in self.routes.drain(..) {
                let _ = Rc::into_raw(route);
            }
        }
    }
}

fn window_from_property(hwnd: Handle) -> Option<Rc<WindowRoutes>> {
    let raw = unsafe { GetPropW(hwnd, PROPERTY.as_ptr()) } as *const WindowRoutes;
    if raw.is_null() {
        return None;
    }
    unsafe {
        Rc::increment_strong_count(raw);
        Some(Rc::from_raw(raw))
    }
}

unsafe extern "system" fn get_message_hook(code: i32, wparam: usize, lparam: isize) -> isize {
    let next = unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) };
    if code != 0 || wparam != PM_REMOVE || lparam == 0 {
        return next;
    }
    let msg_ptr = lparam as *const Msg;
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let original = unsafe { ptr::read(msg_ptr) };
        if !matches!(
            original.message,
            WM_POINTERDOWN | WM_POINTERUPDATE | WM_POINTERUP | WM_POINTERCAPTURECHANGED
        ) || original.hwnd.is_null()
        {
            return;
        }
        let Some(group) = window_from_property(original.hwnd) else {
            return;
        };
        if unsafe { GetCurrentThreadId() } != group.service.thread || group.hwnd != original.hwnd {
            return;
        }
        let pointer_id = (original.wparam & 0xFFFF) as u32;
        if catch_unwind(AssertUnwindSafe(|| {
            group.process_pointer(original.message, pointer_id);
        }))
        .is_err()
        {
            let mut diag = group.service.diagnostics.borrow_mut();
            diag.panics = diag.panics.saturating_add(1);
        }
    }));
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_structs_match_win64_abi_sizes() {
        assert_eq!(std::mem::size_of::<PointerInfo>(), 96);
        assert_eq!(std::mem::size_of::<PointerTouchInfo>(), 144);
    }

    #[test]
    fn classifies_pointer_messages_and_canceled_flag() {
        assert_eq!(
            classify_pointer_message(WM_POINTERDOWN, 0),
            Some(TouchEventType::Pressed)
        );
        assert_eq!(
            classify_pointer_message(WM_POINTERUPDATE, 0),
            Some(TouchEventType::Moved)
        );
        assert_eq!(
            classify_pointer_message(WM_POINTERUP, 0),
            Some(TouchEventType::Released)
        );
        assert_eq!(
            classify_pointer_message(WM_POINTERCAPTURECHANGED, 0),
            Some(TouchEventType::Cancelled)
        );
        assert_eq!(
            classify_pointer_message(WM_POINTERUPDATE, POINTER_FLAG_CANCELED),
            Some(TouchEventType::Cancelled)
        );
        assert_eq!(classify_pointer_message(0x0100, 0), None);
    }

    #[test]
    fn maps_touch_and_pen_and_rejects_mouse_touchpad_or_bad_scale() {
        let mut info = PointerInfo {
            pointer_type: PT_TOUCH,
            pointer_id: 7,
            dw_key_states: POINTER_MOD_SHIFT | POINTER_MOD_CTRL,
            ..PointerInfo::default()
        };
        let touch = PointerTouchInfo {
            pointer_info: info,
            touch_flags: 0,
            touch_mask: TOUCH_MASK_CONTACTAREA | TOUCH_MASK_ORIENTATION | TOUCH_MASK_PRESSURE,
            rc_contact: Rect {
                left: 100,
                top: 200,
                right: 140,
                bottom: 260,
            },
            rc_contact_raw: Rect::default(),
            orientation: 90,
            pressure: 512,
        };
        let ev = pointer_to_touch_event(WM_POINTERDOWN, &info, Some(&touch), (300, 150), 2.0)
            .expect("PT_TOUCH must convert");
        assert_eq!(ev.id, 7);
        assert_eq!(ev.event_type, TouchEventType::Pressed);
        assert_eq!(ev.pointer_type, TouchPointerType::Touch);
        assert_eq!(ev.x, 150.0);
        assert_eq!(ev.y, 75.0);
        assert_eq!(ev.radius_x, 10.0);
        assert_eq!(ev.radius_y, 15.0);
        assert_eq!(ev.rotation_angle, 90.0);
        assert_eq!(ev.pressure, 0.5);
        assert!(ev.modifiers.shift && ev.modifiers.control && !ev.modifiers.alt);

        info.pointer_type = PT_PEN;
        let pen = pointer_to_touch_event(WM_POINTERUPDATE, &info, None, (10, 20), 1.0).unwrap();
        assert_eq!(pen.pointer_type, TouchPointerType::Pen);
        assert_eq!(pen.pressure, 1.0);

        info.pointer_type = PT_MOUSE;
        assert_eq!(
            pointer_to_touch_event(WM_POINTERDOWN, &info, None, (10, 20), 1.0),
            None
        );
        info.pointer_type = PT_TOUCHPAD;
        assert_eq!(
            pointer_to_touch_event(WM_POINTERDOWN, &info, None, (10, 20), 1.0),
            None
        );
        info.pointer_type = PT_TOUCH;
        assert_eq!(
            pointer_to_touch_event(WM_POINTERDOWN, &info, None, (10, 20), 0.0),
            None
        );
    }

    #[test]
    fn lease_uniqueness_and_retire_reuse_lifecycle() {
        let mut first =
            NativeTouchServices::new(NonZeroUsize::new(4).unwrap()).expect("first lease must pass");
        let diag = first.diagnostics();
        assert!(diag.hook_live && diag.lease_live && !diag.retired);

        assert_eq!(
            NativeTouchServices::new(NonZeroUsize::new(4).unwrap()).err(),
            Some(NativeTouchError::LeaseUnavailable)
        );

        let retired = first.retire().expect("retire must succeed cleanly");
        assert!(!retired.hook_live && !retired.lease_live && retired.properties_live == 0);
        assert!(retired.retired);

        let mut second = NativeTouchServices::new(NonZeroUsize::new(4).unwrap())
            .expect("second lease after retire must succeed");
        let _ = second.retire();
    }
}
