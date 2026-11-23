//! Thin GPUI layout-slot adapter for one opt-in Win32 Alloy child `HWND` browser.
//!
//! Unlike [`crate::CefBrowser`] (Offscreen compositor surface), [`NativeCefBrowser`]
//! does not allocate OSR frames or GPUI `RenderImage`s: CEF renders directly into a
//! Win32 child `HWND` parented to the host GPUI window via `SetAsChild`.
//!
//! Because Win32 child `HWND`s participate in native GDI/DirectComposition window
//! clipping ("Win32 airspace"), GPUI elements cannot overlay on top of the child
//! `HWND` area; hosts must dock toolbars/menus/bars adjacent to the slot and call
//! [`NativeCefBrowser::set_visible`] when switching tabs.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    num::NonZeroUsize,
    rc::Rc,
    sync::OnceLock,
};

use gpui::{
    AnyWindowHandle, App, AppContext as _, Context, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, Styled as _, Subscription,
    Window, canvas, div,
};
use raw_window_handle::RawWindowHandle;
use rust_cef_core::{
    contracts::{
        BrowserCursor, BrowserId, BrowserSurfaceMode, NavigationSnapshot, PixelRect, ShortcutId,
    },
    cursor::{NativeCursorData, StandardCursor},
    js_bridge::{EvalOutcome, EvalRequestId, JsFrameScope},
    ports::{BrowserCommand, BrowserEvent, BrowserHandle, CloseMode, CommandId},
};
use rust_cef_ui::native_slot::{NativeSlotController, NativeSlotError};

use crate::{
    BrowserAttachment,
    view::{ViewError, ViewUxState, bounds_dip, checked_scale, window_identity},
};

#[repr(C)]
#[derive(Clone, Copy)]
struct Win32Point {
    x: i32,
    y: i32,
}

type SubclassProc = unsafe extern "system" fn(isize, u32, usize, isize, usize, usize) -> isize;

#[repr(C)]
struct CursorIconInfo {
    f_icon: i32,
    x_hotspot: u32,
    y_hotspot: u32,
    hbm_mask: isize,
    hbm_color: isize,
}

#[repr(C)]
#[derive(Default)]
struct CursorBitmapInfoHeader {
    bi_size: u32,
    bi_width: i32,
    bi_height: i32,
    bi_planes: u16,
    bi_bit_count: u16,
    bi_compression: u32,
    bi_size_image: u32,
    bi_x_pels_per_meter: i32,
    bi_y_pels_per_meter: i32,
    bi_clr_used: u32,
    bi_clr_important: u32,
}

#[allow(clashing_extern_declarations)]
#[link(name = "user32")]
unsafe extern "system" {
    fn IsIconic(hwnd: isize) -> i32;
    fn IsWindow(hwnd: isize) -> i32;
    fn IsWindowVisible(hwnd: isize) -> i32;
    fn IsWindowEnabled(hwnd: isize) -> i32;
    fn GetActiveWindow() -> isize;
    fn GetForegroundWindow() -> isize;
    fn ClientToScreen(hwnd: isize, point: *mut Win32Point) -> i32;
    fn ScreenToClient(hwnd: isize, point: *mut Win32Point) -> i32;
    fn GetCursorPos(point: *mut Win32Point) -> i32;
    fn WindowFromPoint(point: Win32Point) -> isize;
    fn InvalidateRect(hwnd: isize, rect: *const u8, erase: i32) -> i32;
    fn RedrawWindow(hwnd: isize, lprc_update: *const u8, hrgn_update: isize, flags: u32) -> i32;
    fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
    fn SendMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn SetTimer(hwnd: isize, id_event: usize, elapse_ms: u32, timer_func: *const u8) -> usize;
    fn KillTimer(hwnd: isize, id_event: usize) -> i32;
    fn LoadImageW(h_inst: isize, name: usize, type_: u32, cx: i32, cy: i32, fu_load: u32) -> isize;
    fn SetCursor(h_cursor: isize) -> isize;
    fn GetCursor() -> isize;
    fn GetDC(hwnd: isize) -> isize;
    fn ReleaseDC(hwnd: isize, hdc: isize) -> i32;
    fn CreateIconIndirect(piconinfo: *const CursorIconInfo) -> isize;
    fn DestroyIcon(h_icon: isize) -> i32;
}

#[allow(clashing_extern_declarations)]
#[link(name = "gdi32")]
unsafe extern "system" {
    fn GetDeviceCaps(hdc: isize, index: i32) -> i32;
    fn CreateBitmap(
        n_width: i32,
        n_height: i32,
        n_planes: u32,
        n_bit_count: u32,
        lp_bits: *const c_void,
    ) -> isize;
    fn CreateDIBSection(
        hdc: isize,
        pbmi: *const CursorBitmapInfoHeader,
        usage: u32,
        ppv_bits: *mut *mut c_void,
        h_section: isize,
        offset: u32,
    ) -> isize;
    fn DeleteObject(ho: isize) -> i32;
}

#[link(name = "comctl32")]
unsafe extern "system" {
    fn SetWindowSubclass(
        hwnd: isize,
        pfn_subclass: SubclassProc,
        u_id_subclass: usize,
        dw_ref_data: usize,
    ) -> i32;
    fn DefSubclassProc(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn RemoveWindowSubclass(hwnd: isize, pfn_subclass: SubclassProc, u_id_subclass: usize) -> i32;
}

const SMOOTH_RESIZE_SUBCLASS_ID: usize = 0x4345_465F_5253_5A31;
const SMOOTH_RESIZE_TIMER_ID: usize = 0x4345_4652;
const WM_CEF_SIZEMOVE_PRESENT: u32 = 0x8043;
const MOVE_COALESCE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(32);
const SIZE_COALESCE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(4);

/// Returns an OSR `windowless_frame_rate` aligned to the primary display's
/// native refresh rate within `120..=240` Hz (falling back to 120 Hz on
/// 60 Hz panels or unknown queries to minimize phase beat against DWM VSync).
pub fn recommended_windowless_frame_rate() -> u16 {
    use rust_cef_core::contracts::{DEFAULT_WINDOWLESS_FRAME_RATE, MAX_WINDOWLESS_FRAME_RATE};
    const VREFRESH: i32 = 116;
    let hz = unsafe {
        let hdc = GetDC(0);
        if hdc == 0 {
            0
        } else {
            let v = GetDeviceCaps(hdc, VREFRESH);
            let _ = ReleaseDC(0, hdc);
            v
        }
    };
    if hz >= 30 {
        (hz as u16).clamp(DEFAULT_WINDOWLESS_FRAME_RATE, MAX_WINDOWLESS_FRAME_RATE)
    } else {
        DEFAULT_WINDOWLESS_FRAME_RATE
    }
}

struct OwnedGdiHandle(isize);

impl Drop for OwnedGdiHandle {
    fn drop(&mut self) {
        if self.0 != 0 {
            let _ = unsafe { DeleteObject(self.0) };
        }
    }
}

enum OwnedWinCursor {
    Standard(isize),
    Hidden,
    Custom(std::num::NonZeroIsize),
}

impl OwnedWinCursor {
    fn raw(&self) -> isize {
        match *self {
            Self::Standard(h) => h,
            Self::Hidden => 0,
            Self::Custom(h) => h.get(),
        }
    }
}

impl Drop for OwnedWinCursor {
    fn drop(&mut self) {
        if let Self::Custom(h) = *self {
            let _ = unsafe { DestroyIcon(h.get()) };
        }
    }
}

fn standard_hcursor(kind: StandardCursor) -> isize {
    const IMAGE_CURSOR: u32 = 2;
    const LR_DEFAULTSIZE: u32 = 0x0040;
    const LR_SHARED: u32 = 0x8000;
    const IDC_ARROW: usize = 32512;
    const IDC_IBEAM: usize = 32513;
    const IDC_WAIT: usize = 32514;
    const IDC_CROSS: usize = 32515;
    const IDC_SIZENWSE: usize = 32642;
    const IDC_SIZENESW: usize = 32643;
    const IDC_SIZEWE: usize = 32644;
    const IDC_SIZENS: usize = 32645;
    const IDC_SIZEALL: usize = 32646;
    const IDC_NO: usize = 32648;
    const IDC_HAND: usize = 32649;
    const IDC_APPSTARTING: usize = 32650;
    const IDC_HELP: usize = 32651;

    static CACHE: [OnceLock<isize>; 13] = [const { OnceLock::new() }; 13];
    let (idx, idc) = match kind {
        StandardCursor::Arrow => (0, IDC_ARROW),
        StandardCursor::IBeam => (1, IDC_IBEAM),
        StandardCursor::Wait => (2, IDC_WAIT),
        StandardCursor::Cross => (3, IDC_CROSS),
        StandardCursor::SizeNwse => (4, IDC_SIZENWSE),
        StandardCursor::SizeNesw => (5, IDC_SIZENESW),
        StandardCursor::SizeWe => (6, IDC_SIZEWE),
        StandardCursor::SizeNs => (7, IDC_SIZENS),
        StandardCursor::SizeAll => (8, IDC_SIZEALL),
        StandardCursor::No => (9, IDC_NO),
        StandardCursor::Hand => (10, IDC_HAND),
        StandardCursor::AppStarting => (11, IDC_APPSTARTING),
        StandardCursor::Help => (12, IDC_HELP),
    };
    *CACHE[idx].get_or_init(|| unsafe {
        LoadImageW(0, idc, IMAGE_CURSOR, 0, 0, LR_DEFAULTSIZE | LR_SHARED)
    })
}

fn build_win32_hcursor(data: &NativeCursorData) -> OwnedWinCursor {
    match data {
        NativeCursorData::Standard(kind) => OwnedWinCursor::Standard(standard_hcursor(*kind)),
        NativeCursorData::Hidden => OwnedWinCursor::Hidden,
        NativeCursorData::BgraBitmap {
            width,
            height,
            hotspot_x,
            hotspot_y,
            pixels,
        } => {
            let bih = CursorBitmapInfoHeader {
                bi_size: std::mem::size_of::<CursorBitmapInfoHeader>() as u32,
                bi_width: *width as i32,
                bi_height: -(*height as i32),
                bi_planes: 1,
                bi_bit_count: 32,
                bi_compression: 0,
                ..Default::default()
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let color = OwnedGdiHandle(unsafe { CreateDIBSection(0, &bih, 0, &mut bits, 0, 0) });
            if color.0 == 0 || bits.is_null() {
                return OwnedWinCursor::Standard(standard_hcursor(StandardCursor::Arrow));
            }
            unsafe {
                std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast(), pixels.len());
            }
            let w = *width as usize;
            let h = *height as usize;
            let mask_stride = ((w + 15) / 16) * 2;
            let mut mask_bits = vec![0u8; mask_stride * h];
            for y in 0..h {
                for x in 0..w {
                    if pixels[(y * w + x) * 4 + 3] == 0 {
                        mask_bits[y * mask_stride + (x >> 3)] |= 0x80 >> (x & 7);
                    }
                }
            }
            let mask = OwnedGdiHandle(unsafe {
                CreateBitmap(
                    *width as i32,
                    *height as i32,
                    1,
                    1,
                    mask_bits.as_ptr().cast(),
                )
            });
            if mask.0 == 0 {
                return OwnedWinCursor::Standard(standard_hcursor(StandardCursor::Arrow));
            }
            let icon = unsafe {
                CreateIconIndirect(&CursorIconInfo {
                    f_icon: 0,
                    x_hotspot: *hotspot_x as u32,
                    y_hotspot: *hotspot_y as u32,
                    hbm_mask: mask.0,
                    hbm_color: color.0,
                })
            };
            std::num::NonZeroIsize::new(icon).map_or_else(
                || OwnedWinCursor::Standard(standard_hcursor(StandardCursor::Arrow)),
                OwnedWinCursor::Custom,
            )
        }
        NativeCursorData::MonochromeMask {
            width,
            height,
            hotspot_x,
            hotspot_y,
            and_mask,
            xor_mask,
        } => {
            let mut combined = Vec::with_capacity(and_mask.len() + xor_mask.len());
            combined.extend_from_slice(and_mask);
            combined.extend_from_slice(xor_mask);
            let mask = OwnedGdiHandle(unsafe {
                CreateBitmap(
                    *width as i32,
                    (*height * 2) as i32,
                    1,
                    1,
                    combined.as_ptr().cast(),
                )
            });
            if mask.0 == 0 {
                return OwnedWinCursor::Standard(standard_hcursor(StandardCursor::Arrow));
            }
            let icon = unsafe {
                CreateIconIndirect(&CursorIconInfo {
                    f_icon: 0,
                    x_hotspot: *hotspot_x as u32,
                    y_hotspot: *hotspot_y as u32,
                    hbm_mask: mask.0,
                    hbm_color: 0,
                })
            };
            std::num::NonZeroIsize::new(icon).map_or_else(
                || OwnedWinCursor::Standard(standard_hcursor(StandardCursor::Arrow)),
                OwnedWinCursor::Custom,
            )
        }
    }
}

struct SurfaceCursorEntry {
    browser_key: u64,
    rect_px: (i32, i32, i32, i32),
    hovered: bool,
    cursor: OwnedWinCursor,
}

fn point_in_rect_px(pt: Win32Point, rect: (i32, i32, i32, i32)) -> bool {
    rect.2 > rect.0
        && rect.3 > rect.1
        && pt.x >= rect.0
        && pt.x < rect.2
        && pt.y >= rect.1
        && pt.y < rect.3
}

unsafe fn apply_cursor_if_over_surface(hwnd: isize, rect_px: (i32, i32, i32, i32), hcursor: isize) {
    if unsafe { IsWindowVisible(hwnd) == 0 || IsWindowEnabled(hwnd) == 0 } {
        return;
    }
    let mut screen_pt = Win32Point { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut screen_pt) } == 0 {
        return;
    }
    if unsafe { WindowFromPoint(screen_pt) } != hwnd {
        return;
    }
    let mut client_pt = screen_pt;
    if unsafe { ScreenToClient(hwnd, &mut client_pt) } == 0 {
        return;
    }
    if point_in_rect_px(client_pt, rect_px) {
        let _ = unsafe { SetCursor(hcursor) };
    }
}

#[derive(Default)]
struct SmoothResizeState {
    in_sizemove: bool,
    sync_painting: bool,
    present_queued: bool,
    last_move_commit: Option<std::time::Instant>,
    pending_move_lparam: Option<isize>,
    last_size_commit: Option<std::time::Instant>,
    pending_size: Option<(usize, isize)>,
    surface_cursors: Vec<SurfaceCursorEntry>,
}

std::thread_local! {
    static SIZEMOVE_STATE: RefCell<std::collections::BTreeMap<isize, SmoothResizeState>> =
        const { RefCell::new(std::collections::BTreeMap::new()) };
}

unsafe fn commit_resize_and_paint(
    hwnd: isize,
    wparam: usize,
    lparam: isize,
    sync_paint: bool,
) -> isize {
    const WM_SIZE: u32 = 0x0005;
    const RDW_INVALIDATE: u32 = 0x0001;
    const RDW_UPDATENOW: u32 = 0x0100;

    SIZEMOVE_STATE.with(|m| {
        let mut map = m.borrow_mut();
        let st = map.entry(hwnd).or_default();
        st.last_size_commit = Some(std::time::Instant::now());
        st.pending_size = None;
    });
    let res = unsafe { DefSubclassProc(hwnd, WM_SIZE, wparam, lparam) };
    if sync_paint {
        let entered = SIZEMOVE_STATE.with(|m| {
            let mut map = m.borrow_mut();
            let st = map.entry(hwnd).or_default();
            if st.sync_painting {
                false
            } else {
                st.sync_painting = true;
                true
            }
        });
        if entered {
            if unsafe { IsWindowVisible(hwnd) != 0 } {
                let _ = unsafe {
                    RedrawWindow(hwnd, std::ptr::null(), 0, RDW_INVALIDATE | RDW_UPDATENOW)
                };
            }
            SIZEMOVE_STATE.with(|m| {
                if let Some(st) = m.borrow_mut().get_mut(&hwnd) {
                    st.sync_painting = false;
                    st.present_queued = false;
                }
            });
        }
    }
    res
}

unsafe extern "system" fn smooth_resize_subclass_proc(
    hwnd: isize,
    msg: u32,
    wparam: usize,
    lparam: isize,
    uid: usize,
    _ref_data: usize,
) -> isize {
    const WM_MOVE: u32 = 0x0003;
    const WM_SIZE: u32 = 0x0005;
    const WM_ERASEBKGND: u32 = 0x0014;
    const WM_SETCURSOR: u32 = 0x0020;
    const WM_NCDESTROY: u32 = 0x0082;
    const WM_TIMER: u32 = 0x0113;
    const WM_ENTERSIZEMOVE: u32 = 0x0231;
    const WM_EXITSIZEMOVE: u32 = 0x0232;
    const HTCLIENT: usize = 1;
    const SIZE_MINIMIZED: usize = 1;
    const SIZE_MAXIMIZED: usize = 2;
    const RDW_INVALIDATE: u32 = 0x0001;
    const RDW_UPDATENOW: u32 = 0x0100;

    match msg {
        WM_ERASEBKGND => 1,
        WM_SETCURSOR => {
            let hit_code = (lparam as usize) & 0xFFFF;
            if hit_code == HTCLIENT && unsafe { IsWindowEnabled(hwnd) != 0 } {
                let mut pt = Win32Point { x: 0, y: 0 };
                if unsafe { GetCursorPos(&mut pt) != 0 && ScreenToClient(hwnd, &mut pt) != 0 } {
                    let matched = SIZEMOVE_STATE.with(|m| {
                        let map = m.borrow();
                        let st = map.get(&hwnd)?;
                        st.surface_cursors
                            .iter()
                            .rev()
                            .find(|e| e.hovered && point_in_rect_px(pt, e.rect_px))
                            .map(|e| e.cursor.raw())
                    });
                    if let Some(hcursor) = matched {
                        let _ = unsafe { SetCursor(hcursor) };
                        return 1;
                    }
                }
            }
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
        WM_ENTERSIZEMOVE => {
            SIZEMOVE_STATE.with(|m| {
                let mut map = m.borrow_mut();
                let st = map.entry(hwnd).or_default();
                st.in_sizemove = true;
                st.present_queued = false;
                st.last_move_commit = None;
                st.pending_move_lparam = None;
                st.last_size_commit = None;
                st.pending_size = None;
            });
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
        WM_EXITSIZEMOVE => {
            let _ = unsafe { KillTimer(hwnd, SMOOTH_RESIZE_TIMER_ID) };
            let (pending_move, pending_size) = SIZEMOVE_STATE.with(|m| {
                let mut map = m.borrow_mut();
                let st = map.entry(hwnd).or_default();
                st.in_sizemove = false;
                st.present_queued = false;
                st.last_move_commit = None;
                st.last_size_commit = None;
                (st.pending_move_lparam.take(), st.pending_size.take())
            });
            if let Some(lp) = pending_move {
                let _ = unsafe { DefSubclassProc(hwnd, WM_MOVE, 0, lp) };
            }
            if let Some((sw, sl)) = pending_size {
                let _ = unsafe { commit_resize_and_paint(hwnd, sw, sl, true) };
            }
            if unsafe { IsWindowVisible(hwnd) != 0 } {
                let _ = unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
        WM_MOVE => {
            let defer = SIZEMOVE_STATE.with(|m| {
                let mut map = m.borrow_mut();
                let st = map.entry(hwnd).or_default();
                if !st.in_sizemove {
                    return false;
                }
                if st
                    .last_move_commit
                    .is_some_and(|t| t.elapsed() < MOVE_COALESCE_INTERVAL)
                {
                    st.pending_move_lparam = Some(lparam);
                    true
                } else {
                    st.last_move_commit = Some(std::time::Instant::now());
                    st.pending_move_lparam = None;
                    false
                }
            });
            if defer {
                0
            } else {
                unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
            }
        }
        WM_SIZE if wparam == SIZE_MINIMIZED => {
            let _ = unsafe { KillTimer(hwnd, SMOOTH_RESIZE_TIMER_ID) };
            SIZEMOVE_STATE.with(|m| {
                if let Some(st) = m.borrow_mut().get_mut(&hwnd) {
                    st.pending_size = None;
                }
            });
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
        WM_SIZE => {
            enum SizeAction {
                Defer,
                CommitSync,
                PassThrough,
            }
            let action = SIZEMOVE_STATE.with(|m| {
                let mut map = m.borrow_mut();
                let st = map.entry(hwnd).or_default();
                if st.in_sizemove && !st.sync_painting {
                    if st
                        .last_size_commit
                        .is_some_and(|t| t.elapsed() < SIZE_COALESCE_INTERVAL)
                    {
                        st.pending_size = Some((wparam, lparam));
                        SizeAction::Defer
                    } else {
                        st.pending_size = None;
                        SizeAction::CommitSync
                    }
                } else if wparam == SIZE_MAXIMIZED && !st.sync_painting {
                    st.pending_size = None;
                    SizeAction::CommitSync
                } else {
                    SizeAction::PassThrough
                }
            });
            match action {
                SizeAction::Defer => {
                    let _ = unsafe { SetTimer(hwnd, SMOOTH_RESIZE_TIMER_ID, 4, std::ptr::null()) };
                    0
                }
                SizeAction::CommitSync => {
                    let _ = unsafe { KillTimer(hwnd, SMOOTH_RESIZE_TIMER_ID) };
                    unsafe { commit_resize_and_paint(hwnd, wparam, lparam, true) }
                }
                SizeAction::PassThrough => unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) },
            }
        }
        WM_TIMER if wparam == SMOOTH_RESIZE_TIMER_ID => {
            let _ = unsafe { KillTimer(hwnd, SMOOTH_RESIZE_TIMER_ID) };
            let pending = SIZEMOVE_STATE.with(|m| {
                let mut map = m.borrow_mut();
                let st = map.get_mut(&hwnd)?;
                if st.in_sizemove && !st.sync_painting {
                    st.pending_size.take()
                } else {
                    st.pending_size = None;
                    None
                }
            });
            if let Some((sw, sl)) = pending {
                let _ = unsafe { commit_resize_and_paint(hwnd, sw, sl, true) };
            }
            0
        }
        WM_CEF_SIZEMOVE_PRESENT => {
            enum PresentAction {
                Skip,
                CommitPendingSize(usize, isize),
                RedrawNow,
            }
            let action = SIZEMOVE_STATE.with(|m| {
                let mut map = m.borrow_mut();
                let Some(st) = map.get_mut(&hwnd) else {
                    return PresentAction::Skip;
                };
                let was_queued = std::mem::replace(&mut st.present_queued, false);
                if !was_queued || !st.in_sizemove || st.sync_painting {
                    return PresentAction::Skip;
                }
                if let Some((sw, sl)) = st.pending_size.take() {
                    PresentAction::CommitPendingSize(sw, sl)
                } else {
                    st.sync_painting = true;
                    PresentAction::RedrawNow
                }
            });
            match action {
                PresentAction::Skip => {}
                PresentAction::CommitPendingSize(sw, sl) => {
                    let _ = unsafe { KillTimer(hwnd, SMOOTH_RESIZE_TIMER_ID) };
                    let _ = unsafe { commit_resize_and_paint(hwnd, sw, sl, true) };
                }
                PresentAction::RedrawNow => {
                    if unsafe { IsWindowVisible(hwnd) != 0 } {
                        let _ = unsafe {
                            RedrawWindow(hwnd, std::ptr::null(), 0, RDW_INVALIDATE | RDW_UPDATENOW)
                        };
                    }
                    SIZEMOVE_STATE.with(|m| {
                        if let Some(st) = m.borrow_mut().get_mut(&hwnd) {
                            st.sync_painting = false;
                        }
                    });
                }
            }
            0
        }
        WM_NCDESTROY => {
            let _ = unsafe { KillTimer(hwnd, SMOOTH_RESIZE_TIMER_ID) };
            let removed = SIZEMOVE_STATE.with(|m| m.borrow_mut().remove(&hwnd));
            if let Some(state) = removed {
                let active = unsafe { GetCursor() };
                if state
                    .surface_cursors
                    .iter()
                    .any(|e| matches!(e.cursor, OwnedWinCursor::Custom(h) if h.get() == active))
                {
                    let _ = unsafe { SetCursor(standard_hcursor(StandardCursor::Arrow)) };
                }
            }
            let _ = unsafe { RemoveWindowSubclass(hwnd, smooth_resize_subclass_proc, uid) };
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
        _ => unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) },
    }
}

pub(crate) fn update_surface_cursor(
    window: &Window,
    browser_key: u64,
    data: &NativeCursorData,
    rect_px: (i32, i32, i32, i32),
    hovered: bool,
) {
    let Some(hwnd) = window_hwnd(window) else {
        return;
    };
    let raw = hwnd.get() as isize;
    if unsafe { IsWindow(raw) == 0 } {
        return;
    }
    let new_cursor = build_win32_hcursor(data);
    let new_raw = new_cursor.raw();
    let old_cursor = SIZEMOVE_STATE.with(|m| {
        let mut map = m.borrow_mut();
        let st = map.entry(raw).or_default();
        if let Some(entry) = st
            .surface_cursors
            .iter_mut()
            .find(|e| e.browser_key == browser_key)
        {
            entry.rect_px = rect_px;
            entry.hovered = hovered;
            Some(std::mem::replace(&mut entry.cursor, new_cursor))
        } else {
            st.surface_cursors.push(SurfaceCursorEntry {
                browser_key,
                rect_px,
                hovered,
                cursor: new_cursor,
            });
            None
        }
    });
    if hovered {
        unsafe { apply_cursor_if_over_surface(raw, rect_px, new_raw) };
    }
    if let Some(old) = old_cursor {
        if old.raw() != new_raw && unsafe { GetCursor() } == old.raw() {
            let fallback = if hovered {
                new_raw
            } else {
                standard_hcursor(StandardCursor::Arrow)
            };
            let _ = unsafe { SetCursor(fallback) };
        }
        drop(old);
    }
}

pub(crate) fn update_surface_cursor_bounds(
    window: &Window,
    browser_key: u64,
    rect_px: (i32, i32, i32, i32),
) {
    let Some(hwnd) = window_hwnd(window) else {
        return;
    };
    let raw = hwnd.get() as isize;
    SIZEMOVE_STATE.with(|m| {
        if let Some(st) = m.borrow_mut().get_mut(&raw)
            && let Some(entry) = st
                .surface_cursors
                .iter_mut()
                .find(|e| e.browser_key == browser_key)
        {
            entry.rect_px = rect_px;
        }
    });
}

pub(crate) fn set_surface_cursor_hover(window: &Window, browser_key: u64, hovered: bool) {
    let Some(hwnd) = window_hwnd(window) else {
        return;
    };
    let raw = hwnd.get() as isize;
    let info = SIZEMOVE_STATE.with(|m| {
        let mut map = m.borrow_mut();
        let st = map.get_mut(&raw)?;
        let entry = st
            .surface_cursors
            .iter_mut()
            .find(|e| e.browser_key == browser_key)?;
        let changed = entry.hovered != hovered;
        entry.hovered = hovered;
        Some((changed, entry.rect_px, entry.cursor.raw()))
    });
    let Some((changed, rect_px, hcursor)) = info else {
        return;
    };
    if hovered {
        unsafe { apply_cursor_if_over_surface(raw, rect_px, hcursor) };
    } else if changed && unsafe { IsWindowVisible(raw) != 0 && IsWindowEnabled(raw) != 0 } {
        let mut screen_pt = Win32Point { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut screen_pt) != 0 && WindowFromPoint(screen_pt) == raw } {
            const WM_SETCURSOR: u32 = 0x0020;
            const HTCLIENT: usize = 1;
            const WM_MOUSEMOVE: usize = 0x0200;
            let _ = unsafe {
                SendMessageW(
                    raw,
                    WM_SETCURSOR,
                    raw as usize,
                    (HTCLIENT | (WM_MOUSEMOVE << 16)) as isize,
                )
            };
        }
    }
}

pub(crate) fn clear_surface_cursor(hwnd_raw: Option<isize>, browser_key: u64) {
    let Some(raw) = hwnd_raw else {
        return;
    };
    let removed = SIZEMOVE_STATE.with(|m| {
        let mut map = m.borrow_mut();
        let st = map.get_mut(&raw)?;
        let pos = st
            .surface_cursors
            .iter()
            .position(|e| e.browser_key == browser_key)?;
        Some(st.surface_cursors.remove(pos))
    });
    if let Some(entry) = removed {
        if matches!(entry.cursor, OwnedWinCursor::Custom(h) if h.get() == unsafe { GetCursor() }) {
            let _ = unsafe { SetCursor(standard_hcursor(StandardCursor::Arrow)) };
        }
        drop(entry);
    }
}

pub(crate) fn is_window_in_sizemove(window: &Window) -> bool {
    let Some(hwnd) = window_hwnd(window) else {
        return false;
    };
    let raw = hwnd.get() as isize;
    SIZEMOVE_STATE.with(|m| m.borrow().get(&raw).is_some_and(|st| st.in_sizemove))
}

pub(crate) fn request_sizemove_present(window: &Window) {
    let Some(hwnd) = window_hwnd(window) else {
        return;
    };
    let raw = hwnd.get() as isize;
    let should_post = SIZEMOVE_STATE.with(|m| {
        let mut map = m.borrow_mut();
        let Some(st) = map.get_mut(&raw) else {
            return false;
        };
        if !st.in_sizemove || st.present_queued {
            return false;
        }
        st.present_queued = true;
        true
    });
    if should_post && unsafe { IsWindow(raw) != 0 } {
        let _ = unsafe { PostMessageW(raw, WM_CEF_SIZEMOVE_PRESENT, 0, 0) };
    }
}

/// Ensures interactive Win32 border drag (`WM_ENTERSIZEMOVE`..`WM_EXITSIZEMOVE`)
/// and maximize transitions synchronously draw and `Present` right after GPUI's
/// `swap_chain.ResizeBuffers(...)` inside `WM_SIZE`, preventing DWM from sampling
/// an unpresented swapchain buffer.
pub(crate) fn install_smooth_resize_hook(window: &Window) {
    let Some(hwnd) = window_hwnd(window) else {
        return;
    };
    let raw = hwnd.get() as isize;
    if unsafe { IsWindow(raw) != 0 } {
        let _ = unsafe {
            SetWindowSubclass(
                raw,
                smooth_resize_subclass_proc,
                SMOOTH_RESIZE_SUBCLASS_ID,
                0,
            )
        };
    }
}

/// Extract the Win32 `HWND` for a GPUI `Window`, if backed by Win32.
pub fn window_hwnd(window: &Window) -> Option<NonZeroUsize> {
    let raw = raw_window_handle::HasWindowHandle::window_handle(window).ok()?;
    match raw.as_raw() {
        RawWindowHandle::Win32(handle) => NonZeroUsize::new(handle.hwnd.get() as usize),
        _ => None,
    }
}

/// Check GPUI activation or immediate Win32 `WM_ACTIVATE(WA_CLICKACTIVE)` state.
/// GPUI updates `is_window_active()` asynchronously after `WM_ACTIVATE`, so the
/// first `WM_LBUTTONDOWN` arriving from another window (e.g. DevTools) still
/// sees `window.is_window_active() == false` even though Win32 already activated it.
pub(crate) fn is_window_effectively_active(window: &Window) -> bool {
    if window.is_window_active() {
        return true;
    }
    let Some(hwnd) = window_hwnd(window) else {
        return false;
    };
    let raw = hwnd.get() as isize;
    unsafe { IsWindow(raw) != 0 && (GetActiveWindow() == raw || GetForegroundWindow() == raw) }
}

pub(crate) fn is_gpui_window_minimized(window: &Window) -> bool {
    let Some(hwnd) = window_hwnd(window) else {
        return false;
    };
    is_window_minimized(hwnd)
}

pub(crate) fn window_client_screen_origin(window: &Window) -> Option<(i32, i32)> {
    let hwnd = window_hwnd(window)?;
    let raw = hwnd.get() as isize;
    if unsafe { IsWindow(raw) == 0 || IsIconic(raw) != 0 } {
        return None;
    }
    let mut pt = Win32Point { x: 0, y: 0 };
    if unsafe { ClientToScreen(raw, &mut pt) } == 0 {
        return None;
    }
    Some((pt.x, pt.y))
}

/// Compute physical-pixel `PixelRect` for a GPUI layout slot (`origin * scale_factor`).
pub fn compute_slot_pixel_rect(
    origin_x: f32,
    origin_y: f32,
    width: f32,
    height: f32,
    scale_factor: f32,
) -> Result<PixelRect, ViewError> {
    let scale = checked_scale(scale_factor)?;
    if ![origin_x, origin_y, width, height]
        .into_iter()
        .all(f32::is_finite)
    {
        return Err(ViewError::InvalidGeometry);
    }
    let px_x = (origin_x * scale).round();
    let px_y = (origin_y * scale).round();
    let px_w = (width.max(0.0) * scale).round();
    let px_h = (height.max(0.0) * scale).round();
    if !px_x.is_finite()
        || !px_y.is_finite()
        || !px_w.is_finite()
        || !px_h.is_finite()
        || px_x < i32::MIN as f32
        || px_x > i32::MAX as f32
        || px_y < i32::MIN as f32
        || px_y > i32::MAX as f32
        || px_w < 0.0
        || px_w > u32::MAX as f32
        || px_h < 0.0
        || px_h > u32::MAX as f32
    {
        return Err(ViewError::InvalidGeometry);
    }
    let rect = PixelRect {
        x: px_x as i32,
        y: px_y as i32,
        width: px_w as u32,
        height: px_h as u32,
    };
    rect.validate().map_err(|_| ViewError::InvalidGeometry)?;
    Ok(rect)
}

fn is_window_minimized(hwnd: NonZeroUsize) -> bool {
    // SAFETY: Win32 `IsIconic` safely probes any handle value without mutating window state.
    unsafe { IsIconic(hwnd.get() as isize) != 0 }
}

fn map_slot_error(err: NativeSlotError) -> ViewError {
    match err {
        NativeSlotError::BrowserMismatch { expected, actual } => {
            ViewError::BrowserMismatch { expected, actual }
        }
        NativeSlotError::NotNativeChildHwnd => ViewError::SurfaceModeMismatch,
        NativeSlotError::ParentHwndMismatch { expected, actual } => {
            ViewError::NativeWindowMismatch {
                expected,
                actual: Some(actual),
            }
        }
        NativeSlotError::Retired => ViewError::RetiredBinding,
        NativeSlotError::InvalidRect => ViewError::InvalidGeometry,
    }
}

pub type NativeTakeFocusCallback =
    Box<dyn FnMut(bool, &mut NativeCefBrowser, &mut Window, &mut Context<NativeCefBrowser>)>;
pub type NativeShortcutCallback =
    Box<dyn FnMut(ShortcutId, &mut NativeCefBrowser, &mut Window, &mut Context<NativeCefBrowser>)>;
pub type NativeJsMessageCallback = Box<
    dyn FnMut(String, String, &mut NativeCefBrowser, &mut Window, &mut Context<NativeCefBrowser>),
>;
pub type NativeEvalCompletedCallback = Box<
    dyn FnMut(
        EvalRequestId,
        EvalOutcome,
        &mut NativeCefBrowser,
        &mut Window,
        &mut Context<NativeCefBrowser>,
    ),
>;

#[derive(Clone, Debug, PartialEq)]
pub struct NativeViewDiagnostics {
    pub parent_hwnd: NonZeroUsize,
    pub last_bounds: Option<PixelRect>,
    pub visible: bool,
    pub window_minimized: bool,
    pub bounds_commands_sent: u64,
    pub visibility_commands_sent: u64,
    pub move_notifications_sent: u64,
    pub focus_commands_sent: u64,
    pub retired: bool,
    pub last_error: Option<String>,
    pub scale_factor: f32,
}

/// Thin GPUI layout-slot entity for a `BrowserSurfaceMode::NativeChildHwnd` Browser.
pub struct NativeCefBrowser {
    browser: BrowserId,
    handle: BrowserHandle,
    parent_hwnd: NonZeroUsize,
    surface_mode: BrowserSurfaceMode,
    window_handle: AnyWindowHandle,
    focus: FocusHandle,
    slot: Rc<RefCell<NativeSlotController>>,
    host_visible: Rc<Cell<bool>>,
    minimized: Rc<Cell<bool>>,
    scale_factor: Rc<Cell<f32>>,
    last_error: Rc<RefCell<Option<String>>>,
    ux: ViewUxState,
    subscriptions: [Option<Subscription>; 4],
    on_take_focus: Option<NativeTakeFocusCallback>,
    on_shortcut: Option<NativeShortcutCallback>,
    on_js_message: Option<NativeJsMessageCallback>,
    on_eval_completed: Option<NativeEvalCompletedCallback>,
}

impl NativeCefBrowser {
    /// Bind a `NativeChildHwnd` attachment to the owning GPUI window.
    pub fn new(
        attachment: BrowserAttachment,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<gpui::Entity<Self>, ViewError> {
        let browser = attachment.browser();
        let handle_browser = attachment.handle().snapshot().browser;
        if handle_browser != browser {
            return Err(ViewError::BrowserMismatch {
                expected: browser,
                actual: handle_browser,
            });
        }
        let surface_mode = attachment.surface_mode();
        let BrowserSurfaceMode::NativeChildHwnd(cfg) = surface_mode else {
            return Err(ViewError::SurfaceModeMismatch);
        };
        let actual_hwnd = window_hwnd(window);
        if actual_hwnd != Some(cfg.parent_hwnd) {
            return Err(ViewError::NativeWindowMismatch {
                expected: cfg.parent_hwnd,
                actual: actual_hwnd,
            });
        }
        let scale_factor = checked_scale(window.scale_factor())?;
        let window_handle = window.window_handle();
        let window_id = window_handle.window_id().as_u64();
        let handle = attachment.handle().clone();
        let slot_controller =
            NativeSlotController::from_attachment(attachment, cfg.parent_hwnd, window_id)
                .map_err(map_slot_error)?;
        let close_token = slot_controller.token().clone();
        let close_handle = handle.clone();
        let slot = Rc::new(RefCell::new(slot_controller));
        let host_visible = Rc::new(Cell::new(true));
        let minimized = Rc::new(Cell::new(is_window_minimized(cfg.parent_hwnd)));
        let scale_cell = Rc::new(Cell::new(scale_factor));
        let last_error = Rc::new(RefCell::new(None));

        let view = cx.new(|cx| Self {
            browser,
            handle,
            parent_hwnd: cfg.parent_hwnd,
            surface_mode,
            window_handle,
            focus: cx.focus_handle(),
            slot,
            host_visible,
            minimized,
            scale_factor: scale_cell,
            last_error,
            ux: ViewUxState::default(),
            subscriptions: [None, None, None, None],
            on_take_focus: None,
            on_shortcut: None,
            on_js_message: None,
            on_eval_completed: None,
        });

        let release_subscription = cx.observe_release(&view, |view, _app| {
            view.retire();
        });
        let close_view = view.downgrade();
        let close_subscription = cx.on_window_closed(move |app, closed_window| {
            if closed_window.as_u64() != window_id {
                return;
            }
            if close_view.update(app, |view, _| view.retire()).is_err() {
                close_token.retire();
                let _ = close_handle.submit(BrowserCommand::SetVisible(false));
                close_handle.request_close(CloseMode::Graceful);
            }
        });

        view.update(cx, move |view, cx| {
            let activation = cx.observe_window_activation(window, |view, _window, cx| {
                let minimized = is_window_minimized(view.parent_hwnd);
                view.minimized.set(minimized);
                view.reconcile_visibility();
                cx.notify();
            });
            let bounds = cx.observe_window_bounds(window, |view, window, cx| {
                if let Ok(scale) = checked_scale(window.scale_factor()) {
                    view.scale_factor.set(scale);
                }
                let minimized = is_window_minimized(view.parent_hwnd);
                view.minimized.set(minimized);
                view.reconcile_visibility();
                view.notify_parent_moved();
                cx.notify();
            });
            view.subscriptions = [
                Some(release_subscription),
                Some(close_subscription),
                Some(activation),
                Some(bounds),
            ];
        });

        Ok(view)
    }

    pub fn browser(&self) -> BrowserId {
        self.browser
    }

    pub fn handle(&self) -> &BrowserHandle {
        &self.handle
    }

    pub fn parent_hwnd(&self) -> NonZeroUsize {
        self.parent_hwnd
    }

    pub fn surface_mode(&self) -> BrowserSurfaceMode {
        self.surface_mode
    }

    fn check_window(&self, window: &Window) -> Result<(), ViewError> {
        let actual = window_identity(window);
        let expected = self.window_handle.window_id().as_u64();
        if actual != expected {
            return Err(ViewError::WrongWindow { expected, actual });
        }
        Ok(())
    }

    fn check_live(&self) -> Result<(), ViewError> {
        if self.slot.borrow().is_live() {
            Ok(())
        } else {
            Err(ViewError::RetiredBinding)
        }
    }

    fn reconcile_visibility(&self) {
        let visible = self.host_visible.get() && !self.minimized.get();
        let mut slot = self.slot.borrow_mut();
        if !slot.is_live() {
            return;
        }
        if let Err(err) = slot.set_visible(visible) {
            *self.last_error.borrow_mut() = Some(err.to_string());
        }
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        self.host_visible.set(visible);
        let minimized = is_window_minimized(self.parent_hwnd);
        self.minimized.set(minimized);
        self.reconcile_visibility();
        cx.notify();
    }

    pub fn set_focus(&mut self, focused: bool) {
        let mut slot = self.slot.borrow_mut();
        if !slot.is_live() {
            return;
        }
        if let Err(err) = slot.set_focus(focused) {
            *self.last_error.borrow_mut() = Some(err.to_string());
        }
    }

    pub fn notify_parent_moved(&mut self) {
        let mut slot = self.slot.borrow_mut();
        if !slot.is_live() {
            return;
        }
        if let Err(err) = slot.notify_parent_moved() {
            *self.last_error.borrow_mut() = Some(err.to_string());
        }
    }

    /// Synchronize slot geometry/visibility and drain owned UX/JS events for the bound window.
    pub fn consume(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), ViewError> {
        self.check_window(window)?;
        self.check_live()?;
        if let Ok(scale) = checked_scale(window.scale_factor()) {
            self.scale_factor.set(scale);
        }
        let minimized = is_window_minimized(self.parent_hwnd);
        self.minimized.set(minimized);
        {
            let mut slot = self.slot.borrow_mut();
            if let Err(err) = slot.set_visible(self.host_visible.get() && !minimized) {
                *self.last_error.borrow_mut() = Some(err.to_string());
            } else if let Err(err) = slot.sync() {
                *self.last_error.borrow_mut() = Some(err.to_string());
            }
        }
        self.sync_ux(window, cx);
        self.sync_js(window, cx);
        cx.notify();
        Ok(())
    }

    fn sync_ux(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let nav = self.handle.snapshot().navigation;
        self.ux.apply_snapshot(&nav);
        let events = self.handle.take_ux_events();
        let mut focus_taken = Vec::new();
        let mut shortcuts = Vec::new();
        for event in events {
            let (focus, shortcut) = self.ux.apply_event(event);
            if let Some(next) = focus {
                focus_taken.push(next);
            }
            if let Some(id) = shortcut {
                shortcuts.push(id);
            }
        }
        if !focus_taken.is_empty()
            && let Some(mut cb) = self.on_take_focus.take()
        {
            for next in focus_taken {
                cb(next, self, window, cx);
            }
            if self.on_take_focus.is_none() {
                self.on_take_focus = Some(cb);
            }
        }
        if !shortcuts.is_empty()
            && let Some(mut cb) = self.on_shortcut.take()
        {
            for id in shortcuts {
                cb(id, self, window, cx);
            }
            if self.on_shortcut.is_none() {
                self.on_shortcut = Some(cb);
            }
        }
    }

    fn sync_js(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let events = self.handle.take_js_events();
        for event in events {
            match event {
                BrowserEvent::JsMessageReceived {
                    origin,
                    payload_json,
                } => {
                    if let Some(mut cb) = self.on_js_message.take() {
                        cb(origin, payload_json, self, window, cx);
                        if self.on_js_message.is_none() {
                            self.on_js_message = Some(cb);
                        }
                    }
                }
                BrowserEvent::EvalCompleted { id, outcome } => {
                    if let Some(mut cb) = self.on_eval_completed.take() {
                        cb(id, outcome, self, window, cx);
                        if self.on_eval_completed.is_none() {
                            self.on_eval_completed = Some(cb);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    pub fn retire(&mut self) {
        self.slot.borrow_mut().retire();
    }

    pub fn cursor(&self) -> BrowserCursor {
        self.ux.cursor
    }

    pub fn tooltip(&self) -> &str {
        &self.ux.tooltip
    }

    pub fn status_text(&self) -> &str {
        &self.ux.status_text
    }

    pub fn zoom_level(&self) -> f64 {
        f64::from(self.ux.zoom_level_milli) / 1000.0
    }

    pub fn audio_muted(&self) -> bool {
        self.ux.audio_muted
    }

    pub fn navigation_snapshot(&self) -> NavigationSnapshot {
        self.handle.snapshot().navigation
    }

    pub fn set_on_take_focus(&mut self, handler: Option<NativeTakeFocusCallback>) {
        self.on_take_focus = handler;
    }

    pub fn set_on_shortcut(&mut self, handler: Option<NativeShortcutCallback>) {
        self.on_shortcut = handler;
    }

    pub fn set_on_js_message(&mut self, handler: Option<NativeJsMessageCallback>) {
        self.on_js_message = handler;
    }

    pub fn set_on_eval_completed(&mut self, handler: Option<NativeEvalCompletedCallback>) {
        self.on_eval_completed = handler;
    }

    pub fn set_zoom_level(&mut self, level: f64) -> Result<CommandId, ViewError> {
        self.check_live()?;
        self.handle
            .submit(BrowserCommand::SetZoomLevel(level))
            .map_err(ViewError::Input)
    }

    pub fn reset_zoom(&mut self) -> Result<CommandId, ViewError> {
        self.set_zoom_level(0.0)
    }

    pub fn set_audio_muted(&mut self, muted: bool) -> Result<CommandId, ViewError> {
        self.check_live()?;
        self.handle
            .submit(BrowserCommand::SetAudioMuted(muted))
            .map_err(ViewError::Input)
    }

    pub fn execute_javascript(
        &self,
        script: impl Into<String>,
        frame_scope: JsFrameScope,
    ) -> Result<CommandId, ViewError> {
        self.check_live()?;
        self.handle
            .submit(BrowserCommand::ExecuteJavaScript {
                script: script.into(),
                frame_scope,
            })
            .map_err(ViewError::Input)
    }

    pub fn evaluate_script(
        &self,
        script: impl Into<String>,
        frame_scope: JsFrameScope,
        timeout_ms: u64,
    ) -> Result<EvalRequestId, ViewError> {
        self.check_live()?;
        self.handle
            .evaluate_script(script.into(), frame_scope, timeout_ms)
            .map_err(ViewError::Input)
    }

    pub fn post_js_message(&self, payload_json: impl Into<String>) -> Result<CommandId, ViewError> {
        self.check_live()?;
        self.handle
            .submit(BrowserCommand::PostJsMessage {
                payload_json: payload_json.into(),
            })
            .map_err(ViewError::Input)
    }

    pub fn diagnostics(&self) -> NativeViewDiagnostics {
        let slot = self.slot.borrow().diagnostics();
        let error = self
            .last_error
            .borrow()
            .clone()
            .or_else(|| slot.last_error.map(|e| format!("{e:?}")));
        NativeViewDiagnostics {
            parent_hwnd: slot.parent_hwnd,
            last_bounds: slot.last_bounds,
            visible: slot.visible,
            window_minimized: self.minimized.get(),
            bounds_commands_sent: slot.bounds_commands_sent,
            visibility_commands_sent: slot.visibility_commands_sent,
            move_notifications_sent: slot.move_notifications_sent,
            focus_commands_sent: slot.focus_commands_sent,
            retired: slot.retired,
            last_error: error,
            scale_factor: self.scale_factor.get(),
        }
    }
}

impl Drop for NativeCefBrowser {
    fn drop(&mut self) {
        self.slot.borrow_mut().retire();
    }
}

impl Focusable for NativeCefBrowser {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for NativeCefBrowser {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let slot = self.slot.clone();
        let host_visible = self.host_visible.clone();
        let minimized_cell = self.minimized.clone();
        let scale_cell = self.scale_factor.clone();
        let last_error = self.last_error.clone();
        let parent_hwnd = self.parent_hwnd;
        let expected_window = self.window_handle.window_id().as_u64();

        div().size_full().track_focus(&self.focus).child(
            canvas(
                move |bounds, window, _app| {
                    let actual_window = window_identity(window);
                    if actual_window != expected_window {
                        return;
                    }
                    let scale = window.scale_factor();
                    if let Ok(validated) = checked_scale(scale) {
                        scale_cell.set(validated);
                    }
                    let (x, y, w, h) = bounds_dip(bounds);
                    let rect = match compute_slot_pixel_rect(x, y, w, h, scale) {
                        Ok(rect) => rect,
                        Err(err) => {
                            *last_error.borrow_mut() = Some(err.to_string());
                            return;
                        }
                    };
                    let minimized = is_window_minimized(parent_hwnd);
                    minimized_cell.set(minimized);
                    let mut slot = slot.borrow_mut();
                    if !slot.is_live() {
                        return;
                    }
                    let want_visible = host_visible.get() && !minimized;
                    if let Err(err) = slot.set_visible(want_visible) {
                        *last_error.borrow_mut() = Some(err.to_string());
                        return;
                    }
                    if let Err(err) = slot.set_bounds(rect) {
                        *last_error.borrow_mut() = Some(err.to_string());
                    }
                },
                |_bounds, _prepaint: (), _window, _app| {},
            )
            .size_full(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::contracts::MAX_NATIVE_BOUND;

    #[test]
    fn compute_slot_pixel_rect_handles_dpi_scales_and_empty_bounds() {
        assert_eq!(
            compute_slot_pixel_rect(10.0, 20.0, 800.0, 600.0, 1.0),
            Ok(PixelRect {
                x: 10,
                y: 20,
                width: 800,
                height: 600,
            })
        );
        assert_eq!(
            compute_slot_pixel_rect(10.0, 86.0, 1000.0, 600.0, 1.25),
            Ok(PixelRect {
                x: 13,
                y: 108,
                width: 1250,
                height: 750,
            })
        );
        assert_eq!(
            compute_slot_pixel_rect(0.0, 86.0, 800.0, 500.0, 1.5),
            Ok(PixelRect {
                x: 0,
                y: 129,
                width: 1200,
                height: 750,
            })
        );
        assert_eq!(
            compute_slot_pixel_rect(4.0, 8.0, 320.0, 240.0, 2.0),
            Ok(PixelRect {
                x: 8,
                y: 16,
                width: 640,
                height: 480,
            })
        );
        assert_eq!(
            compute_slot_pixel_rect(0.0, 0.0, 0.0, 0.0, 1.5),
            Ok(PixelRect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            })
        );
    }

    #[test]
    fn compute_slot_pixel_rect_rejects_invalid_and_oversized_inputs() {
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                compute_slot_pixel_rect(0.0, 0.0, 100.0, 100.0, bad),
                Err(ViewError::InvalidGeometry)
            );
        }
        assert_eq!(
            compute_slot_pixel_rect(f32::NAN, 0.0, 100.0, 100.0, 1.0),
            Err(ViewError::InvalidGeometry)
        );
        assert_eq!(
            compute_slot_pixel_rect(0.0, 0.0, (MAX_NATIVE_BOUND + 1) as f32, 100.0, 1.0),
            Err(ViewError::InvalidGeometry)
        );
    }

    #[test]
    fn view_error_formats_native_hwnd_variants() {
        assert!(
            ViewError::SurfaceModeMismatch
                .to_string()
                .contains("surface mode")
        );
        let err = ViewError::NativeWindowMismatch {
            expected: NonZeroUsize::new(0x1234).unwrap(),
            actual: Some(NonZeroUsize::new(0x5678).unwrap()),
        };
        let rendered = err.to_string();
        assert!(rendered.contains("0x1234"));
        assert!(rendered.contains("0x5678"));
    }

    #[test]
    fn builds_win32_hcursors_for_standard_hidden_bgra_and_monochrome() {
        #[allow(clashing_extern_declarations)]
        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetIconInfo(h_icon: isize, piconinfo: *mut CursorIconInfo) -> i32;
        }

        for kind in [
            StandardCursor::Arrow,
            StandardCursor::IBeam,
            StandardCursor::Wait,
            StandardCursor::Cross,
            StandardCursor::SizeNwse,
            StandardCursor::SizeNesw,
            StandardCursor::SizeWe,
            StandardCursor::SizeNs,
            StandardCursor::SizeAll,
            StandardCursor::No,
            StandardCursor::Hand,
            StandardCursor::AppStarting,
            StandardCursor::Help,
        ] {
            let cur = build_win32_hcursor(&NativeCursorData::Standard(kind));
            assert_ne!(cur.raw(), 0, "missing standard Win32 cursor for {kind:?}");
        }

        let hidden = build_win32_hcursor(&NativeCursorData::Hidden);
        assert_eq!(hidden.raw(), 0);

        let bgra = NativeCursorData::from_bgra(4, 4, 2, 3, vec![0xFFu8; 4 * 4 * 4]).unwrap();
        let custom_bgra = build_win32_hcursor(&bgra);
        assert_ne!(custom_bgra.raw(), 0);
        let mut info = CursorIconInfo {
            f_icon: 1,
            x_hotspot: 0,
            y_hotspot: 0,
            hbm_mask: 0,
            hbm_color: 0,
        };
        assert_ne!(unsafe { GetIconInfo(custom_bgra.raw(), &mut info) }, 0);
        assert_eq!((info.f_icon, info.x_hotspot, info.y_hotspot), (0, 2, 3));
        assert_ne!(info.hbm_color, 0);
        let _mask_guard = OwnedGdiHandle(info.hbm_mask);
        let _color_guard = OwnedGdiHandle(info.hbm_color);

        let mono = NativeCursorData::from_monochrome(
            2,
            2,
            1,
            1,
            vec![0xC0, 0x00, 0x40, 0x00],
            vec![0x80, 0x00, 0x00, 0x00],
        )
        .unwrap();
        let custom_mono = build_win32_hcursor(&mono);
        assert_ne!(custom_mono.raw(), 0);
        let mut mono_info = CursorIconInfo {
            f_icon: 1,
            x_hotspot: 0,
            y_hotspot: 0,
            hbm_mask: 0,
            hbm_color: 0,
        };
        assert_ne!(unsafe { GetIconInfo(custom_mono.raw(), &mut mono_info) }, 0);
        assert_eq!(
            (mono_info.f_icon, mono_info.x_hotspot, mono_info.y_hotspot),
            (0, 1, 1)
        );
        assert_ne!(mono_info.hbm_mask, 0);
        assert_eq!(mono_info.hbm_color, 0);
        let _mono_mask = OwnedGdiHandle(mono_info.hbm_mask);
    }
}
