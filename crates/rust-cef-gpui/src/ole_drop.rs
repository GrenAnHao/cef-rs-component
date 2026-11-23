//! Win32 COM `IDropTarget` + `IDropTargetHelper` adapter for OSR inbound and
//! intra-page OSR-to-OSR drag-and-drop (files, URLs, plain text, HTML).
//!
//! Structural reference (`D:\System Dir\Downloads\CefSharp`, rev `41919703`):
//! - `CefSharp.Wpf/ChromiumWebBrowser.cs:1738-1797` (`OnDragEnter`/`Over`/`Leave`/`Drop`)
//! - `CefSharp.Wpf/Internals/WpfExtensions.cs:132-228` (`CF_HDROP`, URL, `CF_UNICODETEXT`, `HTML Format`)
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    num::NonZeroUsize,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    ptr,
    rc::Rc,
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

use gpui::{App, Entity, Window};
use raw_window_handle::RawWindowHandle;
use rust_cef_core::{
    contracts::SessionState,
    inbound_drag::{
        InboundDragCommand, InboundDragPayload, MAX_INBOUND_DRAG_FILES, MAX_INBOUND_HTML_BYTES,
        MAX_INBOUND_TEXT_BYTES, MAX_INBOUND_URL_BYTES, parse_cf_html_fragment,
    },
    outbound_drag::{DRAG_OP_COPY, DRAG_OP_LINK, DRAG_OP_MOVE, DragOperationMask},
    ports::{BrowserCommand, BrowserHandle, MouseModifiers},
};

use crate::{binding::WindowToken, ole_drag::IntegratedPumpCallback, view::CefBrowser};

type Handle = *mut c_void;
const MAX_ROUTES: usize = 64;

const S_OK: i32 = 0;
const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
const E_POINTER: i32 = 0x8000_4003u32 as i32;
const CLSCTX_INPROC_SERVER: u32 = 1;
const DVASPECT_CONTENT: u32 = 1;
const TYMED_HGLOBAL: u32 = 1;

const CF_UNICODETEXT: u16 = 13;
const CF_HDROP: u16 = 15;

const MK_SHIFT: u32 = 0x0004;
const MK_CONTROL: u32 = 0x0008;
const MK_ALT: u32 = 0x0020;

const DROPEFFECT_NONE: u32 = 0;
const DROPEFFECT_COPY: u32 = 1;
const DROPEFFECT_MOVE: u32 = 2;
const DROPEFFECT_LINK: u32 = 4;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

const IID_IUNKNOWN: Guid = Guid {
    data1: 0x0000_0000,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};
const IID_IDROPTARGET: Guid = Guid {
    data1: 0x0000_0122,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};
const CLSID_DRAGDROPHELPER: Guid = Guid {
    data1: 0x4657_278A,
    data2: 0x411B,
    data3: 0x11D2,
    data4: [0x83, 0x9A, 0x00, 0xC0, 0x4F, 0xD9, 0x18, 0xD0],
};
const IID_IDROPTARGETHELPER: Guid = Guid {
    data1: 0x4657_278B,
    data2: 0x411B,
    data3: 0x11D2,
    data4: [0x83, 0x9A, 0x00, 0xC0, 0x4F, 0xD9, 0x18, 0xD0],
};

#[repr(C)]
#[derive(Clone, Copy)]
struct PointL {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FormatEtc {
    cf_format: u16,
    ptd: *mut c_void,
    dw_aspect: u32,
    lindex: i32,
    tymed: u32,
}

#[repr(C)]
struct StgMedium {
    tymed: u32,
    union_ptr: *mut c_void,
    p_unk_for_release: *mut c_void,
}

#[repr(C)]
struct IUnknownVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
struct IDataObjectVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_data: unsafe extern "system" fn(*mut c_void, *const FormatEtc, *mut StgMedium) -> i32,
    get_data_here: unsafe extern "system" fn(*mut c_void, *const FormatEtc, *mut StgMedium) -> i32,
    query_get_data: unsafe extern "system" fn(*mut c_void, *const FormatEtc) -> i32,
}

#[repr(C)]
struct IDropTargetVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    drag_enter: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, PointL, *mut u32) -> i32,
    drag_over: unsafe extern "system" fn(*mut c_void, u32, PointL, *mut u32) -> i32,
    drag_leave: unsafe extern "system" fn(*mut c_void) -> i32,
    drop: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, PointL, *mut u32) -> i32,
}

#[repr(C)]
struct IDropTargetHelperVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    drag_enter:
        unsafe extern "system" fn(*mut c_void, Handle, *mut c_void, *const Point, u32) -> i32,
    drag_leave: unsafe extern "system" fn(*mut c_void) -> i32,
    drag_over: unsafe extern "system" fn(*mut c_void, *const Point, u32) -> i32,
    drop: unsafe extern "system" fn(*mut c_void, *mut c_void, *const Point, u32) -> i32,
    show: unsafe extern "system" fn(*mut c_void, i32) -> i32,
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoCreateInstance(
        rclsid: *const Guid,
        p_unk_outer: *mut c_void,
        dw_cls_context: u32,
        riid: *const Guid,
        ppv: *mut *mut c_void,
    ) -> i32;
    fn RegisterDragDrop(hwnd: Handle, drop_target: *mut c_void) -> i32;
    fn RevokeDragDrop(hwnd: Handle) -> i32;
    fn ReleaseStgMedium(medium: *mut StgMedium);
}

#[link(name = "user32")]
unsafe extern "system" {
    fn IsWindow(hwnd: Handle) -> i32;
    fn GetPropW(hwnd: Handle, name: *const u16) -> Handle;
    fn ScreenToClient(hwnd: Handle, point: *mut Point) -> i32;
    fn RegisterClipboardFormatW(format: *const u16) -> u32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalLock(mem: *mut c_void) -> *mut c_void;
    fn GlobalUnlock(mem: *mut c_void) -> i32;
    fn GlobalSize(mem: *mut c_void) -> usize;
    fn GetCurrentThreadId() -> u32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn DragQueryFileW(hdrop: *mut c_void, i_file: u32, lpsz_file: *mut u16, cch: u32) -> u32;
}

/// Win32 OLE's own internal window property holding the currently registered `IDropTarget*`.
const OLE_DROP_TARGET_PROP: &[u16] = &[
    0x4f, 0x6c, 0x65, 0x44, 0x72, 0x6f, 0x70, 0x54, 0x61, 0x72, 0x67, 0x65, 0x74, 0x49, 0x6e, 0x74,
    0x65, 0x72, 0x66, 0x61, 0x63, 0x65, 0, // "OleDropTargetInterface\0"
];
const CFSTR_HTML: &[u16] = &[
    0x48, 0x54, 0x4d, 0x4c, 0x20, 0x46, 0x6f, 0x72, 0x6d, 0x61, 0x74, 0, // "HTML Format\0"
];
const CFSTR_URL_W: &[u16] = &[
    0x55, 0x6e, 0x69, 0x66, 0x6f, 0x72, 0x6d, 0x52, 0x65, 0x73, 0x6f, 0x75, 0x72, 0x63, 0x65, 0x4c,
    0x6f, 0x63, 0x61, 0x74, 0x6f, 0x72, 0x57, 0, // "UniformResourceLocatorW\0"
];
const CFSTR_URL_A: &[u16] = &[
    0x55, 0x6e, 0x69, 0x66, 0x6f, 0x72, 0x6d, 0x52, 0x65, 0x73, 0x6f, 0x75, 0x72, 0x63, 0x65, 0x4c,
    0x6f, 0x63, 0x61, 0x74, 0x6f, 0x72, 0, // "UniformResourceLocator\0"
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeDropTargetError {
    WrongThread,
    Capacity,
    InvalidWindow,
    DuplicateWindow,
    BindingRetired,
    Retired,
    RegisterFailed(i32),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NativeDropTargetDiagnostics {
    pub attached_windows: usize,
    pub drag_enters: u64,
    pub drag_overs: u64,
    pub drag_leaves: u64,
    pub drops: u64,
    pub retired: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActiveTarget {
    None,
    Browser,
    Fallback,
}

#[repr(C)]
struct ComDropTarget {
    vtbl: *const IDropTargetVtbl,
    refs: AtomicU32,
    hwnd: Handle,
    token: WindowToken,
    handle: BrowserHandle,
    geometry: Rc<Cell<(f32, f32, f32, f32, f32)>>,
    pump: Option<IntegratedPumpCallback>,
    helper: *mut c_void,
    fallback: *mut c_void,
    active_target: Cell<ActiveTarget>,
    last_payload: RefCell<Option<InboundDragPayload>>,
    last_allowed: Cell<DragOperationMask>,
    retired: Cell<bool>,
    drag_enters: Cell<u64>,
    drag_overs: Cell<u64>,
    drag_leaves: Cell<u64>,
    drops: Cell<u64>,
}

const DROP_TARGET_VTBL: IDropTargetVtbl = IDropTargetVtbl {
    query_interface: dt_query_interface,
    add_ref: dt_add_ref,
    release: dt_release,
    drag_enter: dt_drag_enter,
    drag_over: dt_drag_over,
    drag_leave: dt_drag_leave,
    drop: dt_drop,
};

unsafe fn unk_add_ref(ptr: *mut c_void) {
    if !ptr.is_null() {
        let vtbl = unsafe { *(ptr as *const *const IUnknownVtbl) };
        unsafe { ((*vtbl).add_ref)(ptr) };
    }
}

unsafe fn unk_release(ptr: *mut c_void) {
    if !ptr.is_null() {
        let vtbl = unsafe { *(ptr as *const *const IUnknownVtbl) };
        unsafe { ((*vtbl).release)(ptr) };
    }
}

unsafe extern "system" fn dt_query_interface(
    this: *mut c_void,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> i32 {
    if ppv.is_null() || riid.is_null() {
        return E_POINTER;
    }
    let iid = unsafe { *riid };
    if iid == IID_IUNKNOWN || iid == IID_IDROPTARGET {
        unsafe { *ppv = this };
        unsafe { dt_add_ref(this) };
        S_OK
    } else {
        unsafe { *ppv = ptr::null_mut() };
        E_NOINTERFACE
    }
}

unsafe extern "system" fn dt_add_ref(this: *mut c_void) -> u32 {
    let target = unsafe { &*(this as *const ComDropTarget) };
    target.refs.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn dt_release(this: *mut c_void) -> u32 {
    let target = unsafe { &*(this as *const ComDropTarget) };
    let prev = target.refs.fetch_sub(1, Ordering::Release);
    if prev == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        let boxed = unsafe { Box::from_raw(this as *mut ComDropTarget) };
        unsafe { unk_release(boxed.helper) };
        unsafe { unk_release(boxed.fallback) };
        0
    } else {
        prev - 1
    }
}

impl ComDropTarget {
    fn to_surface_point(&self, screen_pt: PointL) -> Option<(i32, i32)> {
        let mut pt = Point {
            x: screen_pt.x,
            y: screen_pt.y,
        };
        if unsafe { IsWindow(self.hwnd) == 0 || ScreenToClient(self.hwnd, &mut pt) == 0 } {
            return None;
        }
        let (sx, sy, sw, sh, scale) = self.geometry.get();
        if !scale.is_finite() || scale <= 0.0 || sw <= 0.0 || sh <= 0.0 {
            return None;
        }
        let dip_x = pt.x as f32 / scale;
        let dip_y = pt.y as f32 / scale;
        if dip_x < sx || dip_x >= sx + sw || dip_y < sy || dip_y >= sy + sh {
            return None;
        }
        Some((
            (dip_x - sx).round().clamp(0.0, sw.max(0.0)) as i32,
            (dip_y - sy).round().clamp(0.0, sh.max(0.0)) as i32,
        ))
    }

    fn effective_drop_effect(&self, allowed: DragOperationMask, requested_win32: u32) -> u32 {
        let cef_op = self.handle.drag_target_operation();
        let allowed_win32 = core_to_win32_effect(allowed) & requested_win32;
        let cef_win32 = core_to_win32_effect(cef_op);
        let combined = cef_win32 & allowed_win32;
        if combined != 0 {
            combined
        } else if cef_op != DragOperationMask::NONE {
            0
        } else if (allowed_win32 & DROPEFFECT_COPY) != 0 {
            DROPEFFECT_COPY
        } else if (allowed_win32 & DROPEFFECT_LINK) != 0 {
            DROPEFFECT_LINK
        } else if (allowed_win32 & DROPEFFECT_MOVE) != 0 {
            DROPEFFECT_MOVE
        } else {
            DROPEFFECT_NONE
        }
    }

    fn pump_briefly(&self) {
        if let Some(pump) = &self.pump {
            pump();
            return;
        }
        // In Threaded mode, wait up to a few ms on DragEnter/Over if CEF hasn't reported
        // `UpdateDragCursor` yet so the initial drop cursor matches page targets.
        let deadline = Instant::now() + Duration::from_millis(6);
        while self.handle.drag_target_operation() == DragOperationMask::NONE
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn is_live(&self) -> bool {
        !self.retired.get()
            && self.token.is_live()
            && self.handle.snapshot().state == SessionState::Alive
    }

    unsafe fn helper_drag_enter(&self, data_obj: *mut c_void, pt: PointL, effect: u32) {
        if !self.helper.is_null() {
            let p = Point { x: pt.x, y: pt.y };
            let vtbl = unsafe { *(self.helper as *const *const IDropTargetHelperVtbl) };
            unsafe { ((*vtbl).drag_enter)(self.helper, self.hwnd, data_obj, &p, effect) };
        }
    }

    unsafe fn helper_drag_over(&self, pt: PointL, effect: u32) {
        if !self.helper.is_null() {
            let p = Point { x: pt.x, y: pt.y };
            let vtbl = unsafe { *(self.helper as *const *const IDropTargetHelperVtbl) };
            unsafe { ((*vtbl).drag_over)(self.helper, &p, effect) };
        }
    }

    unsafe fn helper_drag_leave(&self) {
        if !self.helper.is_null() {
            let vtbl = unsafe { *(self.helper as *const *const IDropTargetHelperVtbl) };
            unsafe { ((*vtbl).drag_leave)(self.helper) };
        }
    }

    unsafe fn helper_drop(&self, data_obj: *mut c_void, pt: PointL, effect: u32) {
        if !self.helper.is_null() {
            let p = Point { x: pt.x, y: pt.y };
            let vtbl = unsafe { *(self.helper as *const *const IDropTargetHelperVtbl) };
            unsafe { ((*vtbl).drop)(self.helper, data_obj, &p, effect) };
        }
    }
}

unsafe extern "system" fn dt_drag_enter(
    this: *mut c_void,
    data_obj: *mut c_void,
    key_state: u32,
    pt: PointL,
    pdw_effect: *mut u32,
) -> i32 {
    if pdw_effect.is_null() {
        return E_POINTER;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let target = unsafe { &*(this as *const ComDropTarget) };
        let requested = unsafe { *pdw_effect };
        let allowed = win32_to_core_effect(requested);
        let payload = if !data_obj.is_null() {
            extract_inbound_payload(data_obj)
        } else {
            None
        };
        *target.last_payload.borrow_mut() = payload.clone();
        target.last_allowed.set(allowed);

        if target.is_live()
            && let Some(payload) = payload
            && let Some((vx, vy)) = target.to_surface_point(pt)
        {
            target
                .drag_enters
                .set(target.drag_enters.get().saturating_add(1));
            target.active_target.set(ActiveTarget::Browser);
            let _ = target
                .handle
                .submit(BrowserCommand::DragTarget(InboundDragCommand::Enter {
                    payload,
                    x: vx,
                    y: vy,
                    modifiers: key_state_to_modifiers(key_state),
                    allowed_ops: allowed,
                }));
            target.pump_briefly();
            let effect = target.effective_drop_effect(allowed, requested);
            unsafe {
                *pdw_effect = effect;
                target.helper_drag_enter(data_obj, pt, effect);
            }
            return S_OK;
        }

        if !target.fallback.is_null() {
            target.active_target.set(ActiveTarget::Fallback);
            let vtbl = unsafe { *(target.fallback as *const *const IDropTargetVtbl) };
            let hr = unsafe {
                ((*vtbl).drag_enter)(target.fallback, data_obj, key_state, pt, pdw_effect)
            };
            unsafe { target.helper_drag_enter(data_obj, pt, *pdw_effect) };
            return hr;
        }

        target.active_target.set(ActiveTarget::None);
        unsafe {
            *pdw_effect = DROPEFFECT_NONE;
            target.helper_drag_enter(data_obj, pt, DROPEFFECT_NONE);
        }
        S_OK
    }));
    result.unwrap_or(S_OK)
}

unsafe extern "system" fn dt_drag_over(
    this: *mut c_void,
    key_state: u32,
    pt: PointL,
    pdw_effect: *mut u32,
) -> i32 {
    if pdw_effect.is_null() {
        return E_POINTER;
    }
    let result =
        catch_unwind(AssertUnwindSafe(|| {
            let target = unsafe { &*(this as *const ComDropTarget) };
            let requested = unsafe { *pdw_effect };
            let allowed = win32_to_core_effect(requested);
            target.last_allowed.set(allowed);
            let modifiers = key_state_to_modifiers(key_state);

            if target.is_live()
                && let Some((vx, vy)) = target.to_surface_point(pt)
                && let Some(payload) = target.last_payload.borrow().clone()
            {
                target
                    .drag_overs
                    .set(target.drag_overs.get().saturating_add(1));
                if target.active_target.get() != ActiveTarget::Browser {
                    if target.active_target.get() == ActiveTarget::Fallback
                        && !target.fallback.is_null()
                    {
                        let vtbl = unsafe { *(target.fallback as *const *const IDropTargetVtbl) };
                        unsafe { ((*vtbl).drag_leave)(target.fallback) };
                    }
                    target.active_target.set(ActiveTarget::Browser);
                    let _ = target.handle.submit(BrowserCommand::DragTarget(
                        InboundDragCommand::Enter {
                            payload,
                            x: vx,
                            y: vy,
                            modifiers,
                            allowed_ops: allowed,
                        },
                    ));
                    target.pump_briefly();
                } else {
                    let _ = target.handle.submit(BrowserCommand::DragTarget(
                        InboundDragCommand::Over {
                            x: vx,
                            y: vy,
                            modifiers,
                            allowed_ops: allowed,
                        },
                    ));
                    if let Some(pump) = &target.pump {
                        pump();
                    }
                }
                let effect = target.effective_drop_effect(allowed, requested);
                unsafe {
                    *pdw_effect = effect;
                    target.helper_drag_over(pt, effect);
                }
                return S_OK;
            }

            if target.active_target.get() == ActiveTarget::Browser {
                target.active_target.set(ActiveTarget::None);
                let _ = target
                    .handle
                    .submit(BrowserCommand::DragTarget(InboundDragCommand::Leave));
            }
            unsafe {
                *pdw_effect = DROPEFFECT_NONE;
                target.helper_drag_over(pt, DROPEFFECT_NONE);
            }
            S_OK
        }));
    result.unwrap_or(S_OK)
}

unsafe extern "system" fn dt_drag_leave(this: *mut c_void) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let target = unsafe { &*(this as *const ComDropTarget) };
        target
            .drag_leaves
            .set(target.drag_leaves.get().saturating_add(1));
        match target.active_target.replace(ActiveTarget::None) {
            ActiveTarget::Browser => {
                let _ = target
                    .handle
                    .submit(BrowserCommand::DragTarget(InboundDragCommand::Leave));
            }
            ActiveTarget::Fallback if !target.fallback.is_null() => {
                let vtbl = unsafe { *(target.fallback as *const *const IDropTargetVtbl) };
                unsafe { ((*vtbl).drag_leave)(target.fallback) };
            }
            _ => {}
        }
        *target.last_payload.borrow_mut() = None;
        unsafe { target.helper_drag_leave() };
        S_OK
    }));
    result.unwrap_or(S_OK)
}

unsafe extern "system" fn dt_drop(
    this: *mut c_void,
    data_obj: *mut c_void,
    key_state: u32,
    pt: PointL,
    pdw_effect: *mut u32,
) -> i32 {
    if pdw_effect.is_null() {
        return E_POINTER;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let target = unsafe { &*(this as *const ComDropTarget) };
        let requested = unsafe { *pdw_effect };
        let allowed = win32_to_core_effect(requested);
        let prev = target.active_target.replace(ActiveTarget::None);
        let payload = if !data_obj.is_null() {
            extract_inbound_payload(data_obj)
        } else {
            target.last_payload.borrow().clone()
        };
        *target.last_payload.borrow_mut() = None;

        if target.is_live()
            && let Some((vx, vy)) = target.to_surface_point(pt)
            && let Some(payload) = payload
        {
            target.drops.set(target.drops.get().saturating_add(1));
            let modifiers = key_state_to_modifiers(key_state);
            if prev != ActiveTarget::Browser {
                let _ =
                    target
                        .handle
                        .submit(BrowserCommand::DragTarget(InboundDragCommand::Enter {
                            payload,
                            x: vx,
                            y: vy,
                            modifiers,
                            allowed_ops: allowed,
                        }));
            }
            let _ = target
                .handle
                .submit(BrowserCommand::DragTarget(InboundDragCommand::Drop {
                    x: vx,
                    y: vy,
                    modifiers,
                }));
            let effect = target.effective_drop_effect(allowed, requested);
            unsafe {
                *pdw_effect = effect;
                target.helper_drop(data_obj, pt, effect);
            }
            return S_OK;
        }

        if prev == ActiveTarget::Browser {
            let _ = target
                .handle
                .submit(BrowserCommand::DragTarget(InboundDragCommand::Leave));
        } else if prev == ActiveTarget::Fallback && !target.fallback.is_null() {
            let vtbl = unsafe { *(target.fallback as *const *const IDropTargetVtbl) };
            let hr =
                unsafe { ((*vtbl).drop)(target.fallback, data_obj, key_state, pt, pdw_effect) };
            unsafe { target.helper_drop(data_obj, pt, *pdw_effect) };
            return hr;
        }
        unsafe {
            *pdw_effect = DROPEFFECT_NONE;
            target.helper_drop(data_obj, pt, DROPEFFECT_NONE);
        }
        S_OK
    }));
    result.unwrap_or(S_OK)
}

pub(crate) fn key_state_to_modifiers(key_state: u32) -> MouseModifiers {
    MouseModifiers {
        shift: (key_state & MK_SHIFT) != 0,
        control: (key_state & MK_CONTROL) != 0,
        alt: (key_state & MK_ALT) != 0,
        platform: false,
    }
}

pub(crate) fn win32_to_core_effect(effect: u32) -> DragOperationMask {
    let mut mask = 0u32;
    if (effect & DROPEFFECT_COPY) != 0 {
        mask |= DRAG_OP_COPY;
    }
    if (effect & DROPEFFECT_MOVE) != 0 {
        mask |= DRAG_OP_MOVE;
    }
    if (effect & DROPEFFECT_LINK) != 0 {
        mask |= DRAG_OP_LINK;
    }
    DragOperationMask(mask)
}

pub(crate) fn core_to_win32_effect(mask: DragOperationMask) -> u32 {
    let mut out = DROPEFFECT_NONE;
    if mask.contains(DRAG_OP_COPY) {
        out |= DROPEFFECT_COPY;
    }
    if mask.contains(DRAG_OP_MOVE) {
        out |= DROPEFFECT_MOVE;
    }
    if mask.contains(DRAG_OP_LINK) {
        out |= DROPEFFECT_LINK;
    }
    out
}

unsafe fn query_hglobal_bytes(
    data_obj: *mut c_void,
    cf_format: u16,
    max_bytes: usize,
) -> Option<Vec<u8>> {
    if data_obj.is_null() || cf_format == 0 {
        return None;
    }
    let fmt = FormatEtc {
        cf_format,
        ptd: ptr::null_mut(),
        dw_aspect: DVASPECT_CONTENT,
        lindex: -1,
        tymed: TYMED_HGLOBAL,
    };
    let vtbl = unsafe { *(data_obj as *const *const IDataObjectVtbl) };
    if unsafe { ((*vtbl).query_get_data)(data_obj, &fmt) } != S_OK {
        return None;
    }
    let mut medium = StgMedium {
        tymed: 0,
        union_ptr: ptr::null_mut(),
        p_unk_for_release: ptr::null_mut(),
    };
    if unsafe { ((*vtbl).get_data)(data_obj, &fmt, &mut medium) } != S_OK {
        return None;
    }
    let out = if medium.tymed == TYMED_HGLOBAL && !medium.union_ptr.is_null() {
        let size = unsafe { GlobalSize(medium.union_ptr) };
        let locked = unsafe { GlobalLock(medium.union_ptr) };
        let bytes = if !locked.is_null() && size > 0 {
            let take = size.min(max_bytes);
            Some(unsafe { std::slice::from_raw_parts(locked as *const u8, take) }.to_vec())
        } else {
            None
        };
        if !locked.is_null() {
            unsafe { GlobalUnlock(medium.union_ptr) };
        }
        bytes
    } else {
        None
    };
    unsafe { ReleaseStgMedium(&mut medium) };
    out
}

unsafe fn query_hdrop_files(data_obj: *mut c_void) -> Vec<PathBuf> {
    if data_obj.is_null() {
        return Vec::new();
    }
    let fmt = FormatEtc {
        cf_format: CF_HDROP,
        ptd: ptr::null_mut(),
        dw_aspect: DVASPECT_CONTENT,
        lindex: -1,
        tymed: TYMED_HGLOBAL,
    };
    let vtbl = unsafe { *(data_obj as *const *const IDataObjectVtbl) };
    if unsafe { ((*vtbl).query_get_data)(data_obj, &fmt) } != S_OK {
        return Vec::new();
    }
    let mut medium = StgMedium {
        tymed: 0,
        union_ptr: ptr::null_mut(),
        p_unk_for_release: ptr::null_mut(),
    };
    if unsafe { ((*vtbl).get_data)(data_obj, &fmt, &mut medium) } != S_OK {
        return Vec::new();
    }
    let mut paths = Vec::new();
    if medium.tymed == TYMED_HGLOBAL && !medium.union_ptr.is_null() {
        let hdrop = unsafe { GlobalLock(medium.union_ptr) };
        if !hdrop.is_null() {
            let count = unsafe { DragQueryFileW(hdrop, u32::MAX, ptr::null_mut(), 0) }
                .min(MAX_INBOUND_DRAG_FILES as u32);
            for i in 0..count {
                let len = unsafe { DragQueryFileW(hdrop, i, ptr::null_mut(), 0) };
                if len == 0 || len > 32_768 {
                    continue;
                }
                let mut buf = vec![0u16; len as usize + 1];
                let copied = unsafe { DragQueryFileW(hdrop, i, buf.as_mut_ptr(), len + 1) };
                if copied > 0 {
                    let s = String::from_utf16_lossy(&buf[..copied as usize]);
                    if !s.is_empty() {
                        paths.push(PathBuf::from(s));
                    }
                }
            }
            unsafe { GlobalUnlock(medium.union_ptr) };
        }
    }
    unsafe { ReleaseStgMedium(&mut medium) };
    paths
}

fn decode_utf16_z(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 {
        return None;
    }
    let mut units = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let u = u16::from_le_bytes([chunk[0], chunk[1]]);
        if u == 0 {
            break;
        }
        units.push(u);
    }
    if units.is_empty() {
        None
    } else {
        Some(String::from_utf16_lossy(&units))
    }
}

pub(crate) fn extract_inbound_payload(data_obj: *mut c_void) -> Option<InboundDragPayload> {
    let cf_html = unsafe { RegisterClipboardFormatW(CFSTR_HTML.as_ptr()) } as u16;
    let cf_url_w = unsafe { RegisterClipboardFormatW(CFSTR_URL_W.as_ptr()) } as u16;
    let cf_url_a = unsafe { RegisterClipboardFormatW(CFSTR_URL_A.as_ptr()) } as u16;

    let files = unsafe { query_hdrop_files(data_obj) };
    let url = unsafe { query_hglobal_bytes(data_obj, cf_url_w, MAX_INBOUND_URL_BYTES * 2) }
        .and_then(|b| decode_utf16_z(&b))
        .or_else(|| {
            unsafe { query_hglobal_bytes(data_obj, cf_url_a, MAX_INBOUND_URL_BYTES) }.and_then(
                |b| {
                    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
                    let s = String::from_utf8_lossy(&b[..end]).trim().to_owned();
                    (!s.is_empty()).then_some(s)
                },
            )
        })
        .filter(|u| u.len() <= MAX_INBOUND_URL_BYTES && !u.contains('\0'));

    let text = unsafe { query_hglobal_bytes(data_obj, CF_UNICODETEXT, MAX_INBOUND_TEXT_BYTES * 2) }
        .and_then(|b| decode_utf16_z(&b))
        .filter(|t| t.len() <= MAX_INBOUND_TEXT_BYTES && !t.contains('\0'));

    let (html, html_base_url) =
        unsafe { query_hglobal_bytes(data_obj, cf_html, MAX_INBOUND_HTML_BYTES) }
            .and_then(|b| parse_cf_html_fragment(&b))
            .map(|(h, base)| (Some(h), base))
            .unwrap_or((None, None));

    let payload = InboundDragPayload {
        files,
        url,
        url_title: None,
        text,
        html,
        html_base_url,
    };
    if payload.validate().is_ok() {
        Some(payload)
    } else {
        None
    }
}

struct AttachedRoute {
    hwnd: Handle,
    window_id: u64,
    com_target: *mut ComDropTarget,
}

/// Host-owned Win32 `IDropTarget` + `IDropTargetHelper` service replacing GPUI's
/// file-only drop target on attached `CefBrowser` windows so files, URLs, plain
/// text, and HTML fragments can be dragged in from external apps OR intra-page.
pub struct NativeDropTargetServices {
    capacity: NonZeroUsize,
    thread: u32,
    routes: Vec<AttachedRoute>,
    retired: bool,
}

impl NativeDropTargetServices {
    pub fn new(capacity: NonZeroUsize) -> Result<Self, NativeDropTargetError> {
        if capacity.get() > MAX_ROUTES {
            return Err(NativeDropTargetError::Capacity);
        }
        Ok(Self {
            capacity,
            thread: unsafe { GetCurrentThreadId() },
            routes: Vec::new(),
            retired: false,
        })
    }

    pub fn attach(
        &mut self,
        view: &Entity<CefBrowser>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), NativeDropTargetError> {
        if self.retired {
            return Err(NativeDropTargetError::Retired);
        }
        if unsafe { GetCurrentThreadId() } != self.thread {
            return Err(NativeDropTargetError::WrongThread);
        }
        if self.routes.len() >= self.capacity.get() {
            return Err(NativeDropTargetError::Capacity);
        }
        let hwnd = win32_hwnd(window).ok_or(NativeDropTargetError::InvalidWindow)?;
        let window_id = window.window_handle().window_id().as_u64();
        if self
            .routes
            .iter()
            .any(|r| r.hwnd == hwnd || r.window_id == window_id)
        {
            return Err(NativeDropTargetError::DuplicateWindow);
        }
        let (token, expected_window, handle, geometry, pump) =
            view.read(cx).native_drop_target_attachment();
        if expected_window != window.window_handle() || !token.is_live() {
            return Err(NativeDropTargetError::BindingRetired);
        }

        let fallback = unsafe { GetPropW(hwnd, OLE_DROP_TARGET_PROP.as_ptr()) };
        if !fallback.is_null() {
            unsafe { unk_add_ref(fallback) };
        }
        let _ = unsafe { RevokeDragDrop(hwnd) };

        let mut helper: *mut c_void = ptr::null_mut();
        let _ = unsafe {
            CoCreateInstance(
                &CLSID_DRAGDROPHELPER,
                ptr::null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_IDROPTARGETHELPER,
                &mut helper,
            )
        };

        let com_box = Box::new(ComDropTarget {
            vtbl: &DROP_TARGET_VTBL,
            refs: AtomicU32::new(1),
            hwnd,
            token,
            handle,
            geometry,
            pump,
            helper,
            fallback,
            active_target: Cell::new(ActiveTarget::None),
            last_payload: RefCell::new(None),
            last_allowed: Cell::new(DragOperationMask::NONE),
            retired: Cell::new(false),
            drag_enters: Cell::new(0),
            drag_overs: Cell::new(0),
            drag_leaves: Cell::new(0),
            drops: Cell::new(0),
        });
        let raw = Box::into_raw(com_box);
        let hr = unsafe { RegisterDragDrop(hwnd, raw as *mut c_void) };
        if hr != S_OK {
            unsafe { unk_release(raw as *mut c_void) };
            if !fallback.is_null() && unsafe { IsWindow(hwnd) } != 0 {
                let _ = unsafe { RegisterDragDrop(hwnd, fallback) };
                unsafe { unk_release(fallback) };
            }
            return Err(NativeDropTargetError::RegisterFailed(hr));
        }

        self.routes.push(AttachedRoute {
            hwnd,
            window_id,
            com_target: raw,
        });
        Ok(())
    }

    pub fn diagnostics(&self) -> NativeDropTargetDiagnostics {
        let mut diag = NativeDropTargetDiagnostics {
            attached_windows: self.routes.len(),
            retired: self.retired,
            ..Default::default()
        };
        for r in &self.routes {
            let t = unsafe { &*r.com_target };
            diag.drag_enters += t.drag_enters.get();
            diag.drag_overs += t.drag_overs.get();
            diag.drag_leaves += t.drag_leaves.get();
            diag.drops += t.drops.get();
        }
        diag
    }

    pub fn retire(&mut self) -> NativeDropTargetDiagnostics {
        if self.retired {
            return self.diagnostics();
        }
        self.retired = true;
        let routes = std::mem::take(&mut self.routes);
        for route in routes {
            let t = unsafe { &*route.com_target };
            t.retired.set(true);
            let fallback = t.fallback;
            if unsafe { IsWindow(route.hwnd) } != 0 {
                let _ = unsafe { RevokeDragDrop(route.hwnd) };
                if !fallback.is_null() {
                    let _ = unsafe { RegisterDragDrop(route.hwnd, fallback) };
                }
            }
            unsafe { unk_release(route.com_target as *mut c_void) };
        }
        NativeDropTargetDiagnostics {
            attached_windows: 0,
            retired: true,
            ..Default::default()
        }
    }
}

impl Drop for NativeDropTargetServices {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}

fn win32_hwnd(window: &Window) -> Option<Handle> {
    let raw = raw_window_handle::HasWindowHandle::window_handle(window).ok()?;
    match raw.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get() as Handle),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_and_modifier_mapping_roundtrips() {
        assert_eq!(
            win32_to_core_effect(DROPEFFECT_COPY | DROPEFFECT_LINK),
            DragOperationMask(DRAG_OP_COPY | DRAG_OP_LINK)
        );
        assert_eq!(
            core_to_win32_effect(DragOperationMask::EVERY),
            DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK
        );
        let mods = key_state_to_modifiers(MK_SHIFT | MK_CONTROL);
        assert!(mods.shift && mods.control && !mods.alt);
    }

    #[test]
    fn com_data_object_extraction_reads_text_and_url() {
        #[repr(C)]
        struct FakeDataObjVtbl {
            query_interface:
                unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
            add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
            release: unsafe extern "system" fn(*mut c_void) -> u32,
            get_data:
                unsafe extern "system" fn(*mut c_void, *const FormatEtc, *mut StgMedium) -> i32,
            get_data_here:
                unsafe extern "system" fn(*mut c_void, *const FormatEtc, *mut StgMedium) -> i32,
            query_get_data: unsafe extern "system" fn(*mut c_void, *const FormatEtc) -> i32,
        }
        #[repr(C)]
        struct FakeDataObj {
            vtbl: *const FakeDataObjVtbl,
        }
        const GMEM_MOVEABLE: u32 = 0x0002;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
        }
        unsafe extern "system" fn qi(_: *mut c_void, _: *const Guid, out: *mut *mut c_void) -> i32 {
            if !out.is_null() {
                unsafe { *out = ptr::null_mut() };
            }
            E_NOINTERFACE
        }
        unsafe extern "system" fn add(_: *mut c_void) -> u32 {
            1
        }
        unsafe extern "system" fn rel(_: *mut c_void) -> u32 {
            1
        }
        unsafe extern "system" fn not_impl(
            _: *mut c_void,
            _: *const FormatEtc,
            _: *mut StgMedium,
        ) -> i32 {
            0x8000_4001u32 as i32
        }
        unsafe extern "system" fn qgd(_: *mut c_void, fmt: *const FormatEtc) -> i32 {
            let cf = unsafe { (*fmt).cf_format };
            if cf == CF_UNICODETEXT { S_OK } else { 1 }
        }
        unsafe extern "system" fn gd(
            _: *mut c_void,
            fmt: *const FormatEtc,
            med: *mut StgMedium,
        ) -> i32 {
            if unsafe { (*fmt).cf_format } != CF_UNICODETEXT {
                return 0x8004_0064u32 as i32;
            }
            let wide: Vec<u16> = "OsrDropPayload\0".encode_utf16().collect();
            let bytes = wide.len() * 2;
            let h = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
            let p = unsafe { GlobalLock(h) };
            unsafe { ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, p as *mut u8, bytes) };
            unsafe { GlobalUnlock(h) };
            unsafe {
                (*med).tymed = TYMED_HGLOBAL;
                (*med).union_ptr = h;
                (*med).p_unk_for_release = ptr::null_mut();
            }
            S_OK
        }
        const VTBL: FakeDataObjVtbl = FakeDataObjVtbl {
            query_interface: qi,
            add_ref: add,
            release: rel,
            get_data: gd,
            get_data_here: not_impl,
            query_get_data: qgd,
        };
        let mut obj = FakeDataObj { vtbl: &VTBL };
        let payload = extract_inbound_payload(&mut obj as *mut FakeDataObj as *mut c_void)
            .expect("extracted payload");
        assert_eq!(payload.text.as_deref(), Some("OsrDropPayload"));
    }
}
