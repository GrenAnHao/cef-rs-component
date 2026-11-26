//! Win32 Alloy child `HWND` (`SetAsChild`) helpers for `rust-cef-runtime`.
//!
//! Reference CefSharp anchors (`D:\System Dir\Downloads\CefSharp`, v146.0.100):
//! - `CefSharp.Wpf\HwndHost\ChromiumWebBrowser.cs:1514-1546`: `CreateBrowserWindowInfo`
//!   uses Alloy style (`SetAsChild`), disables windowless rendering, and adds
//!   `WS_EX_NOACTIVATE` during initial creation so embedding does not steal host focus.
//! - `CefSharp.Wpf\HwndHost\ChromiumWebBrowser.cs:64-85`: `RemoveExNoActivateStyle` clears
//!   `WS_EX_NOACTIVATE` via `GetWindowLong`/`SetWindowLong` after `OnAfterCreated`.
//! - `CefSharp.Wpf\HwndHost\ChromiumWebBrowser.cs:1571-1604` &
//!   `CefSharp.Core.Runtime\ManagedCefBrowserAdapter.cpp:194-210`: `0x0` collapses and hides
//!   the child `HWND`; non-empty bounds resize via `SetWindowPos` (`SWP_NOZORDER | SWP_NOACTIVATE`).
//! - `CefSharp.Wpf\HwndHost\Internals\NoCloseLifespanHandler.cs:12-25`,
//!   `CefSharp.WinForms\Handler\LifeSpanHandler.cs:69-70,131-132`,
//!   `CefSharp.Wpf\HwndHost\ChromiumWebBrowser.cs:783-786`, and
//!   `cef_life_span_handler.h:189-209`: `DoClose` returns `true` (`1`) for Alloy
//!   child `HWND` browsers so CEF does not post `WM_CLOSE` to the host top-level
//!   window; CEF's child `HWND` is explicitly destroyed on its owning CEF UI thread.
//!
//! Threading: all Win32 positioning/visibility calls (`SetWindowPos`, `ShowWindow`,
//! `SetWindowLongPtrW`) execute on the owning CEF UI thread, never from the GPUI thread.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::{ffi::c_void, num::NonZeroUsize};

use cef::{Rect as CefRect, RuntimeStyle, WindowInfo};
use rust_cef_core::contracts::{NativeChildHwndConfig, PixelRect};

type Handle = isize;

const GWL_EXSTYLE: i32 = -20;
const WS_EX_NOACTIVATE: isize = 0x0800_0000;
const SW_HIDE: i32 = 0;
const SW_SHOWNOACTIVATE: i32 = 4;
const SWP_NOMOVE: u32 = 0x0002;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_NOACTIVATE: u32 = 0x0010;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct WinPoint {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct WinRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[allow(clashing_extern_declarations)]
#[link(name = "user32")]
unsafe extern "system" {
    fn IsWindow(hwnd: Handle) -> i32;
    fn IsWindowVisible(hwnd: Handle) -> i32;
    fn GetParent(hwnd: Handle) -> Handle;
    fn GetWindowLongPtrW(hwnd: Handle, index: i32) -> isize;
    fn SetWindowLongPtrW(hwnd: Handle, index: i32, new_long: isize) -> isize;
    fn SetWindowPos(
        hwnd: Handle,
        insert_after: Handle,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        flags: u32,
    ) -> i32;
    fn ShowWindow(hwnd: Handle, cmd_show: i32) -> i32;
    fn GetWindowRect(hwnd: Handle, rect: *mut WinRect) -> i32;
    fn ClientToScreen(hwnd: Handle, point: *mut WinPoint) -> i32;
    fn ScreenToClient(hwnd: Handle, point: *mut WinPoint) -> i32;
    fn DestroyWindow(hwnd: Handle) -> i32;
}

/// Owned per-browser native child `HWND` state tracked inside `UiRegistry`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChildHwndState {
    pub(crate) parent_hwnd: NonZeroUsize,
    pub(crate) browser_hwnd: Option<NonZeroUsize>,
    pub(crate) last_bounds: PixelRect,
    pub(crate) visible: bool,
    pub(crate) strip_noactivate: bool,
    pub(crate) noactivate_stripped: bool,
}

impl ChildHwndState {
    pub(crate) fn new(config: NativeChildHwndConfig) -> Self {
        Self {
            parent_hwnd: config.parent_hwnd,
            browser_hwnd: None,
            last_bounds: config.initial_bounds,
            visible: !config.initial_bounds.is_empty(),
            strip_noactivate: !config.activate_on_creation,
            noactivate_stripped: config.activate_on_creation,
        }
    }
}

pub(crate) fn hwnd_from_cef(handle: cef::sys::cef_window_handle_t) -> Option<NonZeroUsize> {
    NonZeroUsize::new(handle.0 as usize)
}

pub(crate) fn is_valid_window(hwnd: NonZeroUsize) -> bool {
    let raw = hwnd.get() as Handle;
    // SAFETY: Win32 `IsWindow` safely probes arbitrary handle values.
    unsafe { IsWindow(raw) != 0 }
}

/// Converts a client-area physical pixel point on `hwnd` to screen physical pixels.
///
/// Cross-thread safe: `ClientToScreen` reads win32k geometry without sending window messages.
pub(crate) fn client_to_screen(hwnd: NonZeroUsize, x_px: i32, y_px: i32) -> Option<(i32, i32)> {
    if !is_valid_window(hwnd) {
        return None;
    }
    let mut pt = WinPoint { x: x_px, y: y_px };
    // SAFETY: `pt` is valid stack storage and `hwnd` passed `IsWindow`.
    if unsafe { ClientToScreen(hwnd.get() as Handle, &mut pt) } == 0 {
        return None;
    }
    Some((pt.x, pt.y))
}

/// Constructs OSR `WindowInfo` matching CEF `156.0.31` (`cef_win.h:126-130`) and
/// CefSharp `156.0.15` (`ChromiumWebBrowser.cs:2051-2056` / `LifespanHandler.cs:203-216`).
pub(crate) fn build_offscreen_window_info(
    parent_hwnd: Option<NonZeroUsize>,
    shared_texture_enabled: bool,
) -> WindowInfo {
    let parent_handle = match parent_hwnd {
        Some(hwnd) => cef::sys::HWND(hwnd.get() as *mut c_void as _),
        None => cef::sys::HWND(std::ptr::null_mut()),
    };
    let mut info = WindowInfo::default().set_as_windowless(parent_handle);
    info.shared_texture_enabled = i32::from(shared_texture_enabled);
    info
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn is_window_visible(hwnd: NonZeroUsize) -> bool {
    if !is_valid_window(hwnd) {
        return false;
    }
    let raw = hwnd.get() as Handle;
    // SAFETY: Validated `IsWindow` immediately above on current sequence.
    unsafe { IsWindowVisible(raw) != 0 }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn parent_window(hwnd: NonZeroUsize) -> Option<NonZeroUsize> {
    if !is_valid_window(hwnd) {
        return None;
    }
    let raw = hwnd.get() as Handle;
    // SAFETY: Validated `IsWindow` above.
    let parent = unsafe { GetParent(raw) };
    usize::try_from(parent).ok().and_then(NonZeroUsize::new)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn window_ex_style(hwnd: NonZeroUsize) -> isize {
    if !is_valid_window(hwnd) {
        return 0;
    }
    let raw = hwnd.get() as Handle;
    // SAFETY: Validated `IsWindow` above.
    unsafe { GetWindowLongPtrW(raw, GWL_EXSTYLE) }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn window_rect_in_parent(
    hwnd: NonZeroUsize,
    parent_hwnd: NonZeroUsize,
) -> Option<PixelRect> {
    if !is_valid_window(hwnd) || !is_valid_window(parent_hwnd) {
        return None;
    }
    let mut rect = WinRect::default();
    // SAFETY: `rect` points to initialized stack storage; both handles validated.
    if unsafe { GetWindowRect(hwnd.get() as Handle, &mut rect) } == 0 {
        return None;
    }
    let mut tl = WinPoint {
        x: rect.left,
        y: rect.top,
    };
    let mut br = WinPoint {
        x: rect.right,
        y: rect.bottom,
    };
    let parent_raw = parent_hwnd.get() as Handle;
    // SAFETY: Both points are valid stack pointers and `parent_hwnd` was validated.
    if unsafe { ScreenToClient(parent_raw, &mut tl) } == 0
        || unsafe { ScreenToClient(parent_raw, &mut br) } == 0
    {
        return None;
    }
    PixelRect::try_from_i32(tl.x, tl.y, br.x - tl.x, br.y - tl.y).ok()
}

/// Construct CEF `WindowInfo` matching CefSharp's Alloy `SetAsChild` setup.
pub(crate) fn build_child_window_info(config: &NativeChildHwndConfig) -> WindowInfo {
    let parent_handle = cef::sys::HWND(config.parent_hwnd.get() as *mut c_void as _);
    let cef_bounds = CefRect {
        x: config.initial_bounds.x,
        y: config.initial_bounds.y,
        width: i32::try_from(config.initial_bounds.width).unwrap_or(0),
        height: i32::try_from(config.initial_bounds.height).unwrap_or(0),
    };
    let mut info = WindowInfo::default().set_as_child(parent_handle, &cef_bounds);
    info.runtime_style = RuntimeStyle::ALLOY;
    info.windowless_rendering_enabled = 0;
    info.shared_texture_enabled = 0;
    if !config.activate_on_creation {
        info.ex_style |= WS_EX_NOACTIVATE as u32;
    }
    info
}

/// Remove initial `WS_EX_NOACTIVATE` style after `OnAfterCreated` so clicks focus normally.
pub(crate) fn strip_ex_noactivate(hwnd: NonZeroUsize) -> bool {
    if !is_valid_window(hwnd) {
        return false;
    }
    let raw = hwnd.get() as Handle;
    // SAFETY: Validated `IsWindow` on the owning thread.
    let ex = unsafe { GetWindowLongPtrW(raw, GWL_EXSTYLE) };
    if (ex & WS_EX_NOACTIVATE) != 0 {
        // SAFETY: Validated `IsWindow`; clears only `WS_EX_NOACTIVATE`.
        unsafe { SetWindowLongPtrW(raw, GWL_EXSTYLE, ex & !WS_EX_NOACTIVATE) };
    }
    // SAFETY: Read-back verification of effective extended style.
    (unsafe { GetWindowLongPtrW(raw, GWL_EXSTYLE) } & WS_EX_NOACTIVATE) == 0
}

pub(crate) fn hide_native_hwnd(hwnd: NonZeroUsize) -> bool {
    if !is_valid_window(hwnd) {
        return false;
    }
    let raw = hwnd.get() as Handle;
    // SAFETY: Validated `IsWindow`; `SW_HIDE` and `SetWindowPos(..., 0, 0)` collapse without activating.
    unsafe {
        ShowWindow(raw, SW_HIDE);
        SetWindowPos(
            raw,
            0,
            0,
            0,
            0,
            0,
            SWP_NOZORDER | SWP_NOMOVE | SWP_NOACTIVATE,
        ) != 0
    }
}

pub(crate) fn apply_native_bounds(hwnd: NonZeroUsize, rect: PixelRect) -> bool {
    if !is_valid_window(hwnd) {
        return false;
    }
    if rect.is_empty() {
        return hide_native_hwnd(hwnd);
    }
    let Ok(w) = i32::try_from(rect.width) else {
        return false;
    };
    let Ok(h) = i32::try_from(rect.height) else {
        return false;
    };
    let raw = hwnd.get() as Handle;
    // SAFETY: Validated `IsWindow` and bounded dimensions (`PixelRect::validate`).
    unsafe { SetWindowPos(raw, 0, rect.x, rect.y, w, h, SWP_NOZORDER | SWP_NOACTIVATE) != 0 }
}

pub(crate) fn show_native_hwnd(hwnd: NonZeroUsize, rect: PixelRect) -> bool {
    if !is_valid_window(hwnd) {
        return false;
    }
    if rect.is_empty() {
        return hide_native_hwnd(hwnd);
    }
    let Ok(w) = i32::try_from(rect.width) else {
        return false;
    };
    let Ok(h) = i32::try_from(rect.height) else {
        return false;
    };
    let raw = hwnd.get() as Handle;
    // SAFETY: Validated `IsWindow`; moves/sizes first then reveals without stealing focus.
    let ok =
        unsafe { SetWindowPos(raw, 0, rect.x, rect.y, w, h, SWP_NOZORDER | SWP_NOACTIVATE) != 0 };
    if ok {
        unsafe {
            ShowWindow(raw, SW_SHOWNOACTIVATE);
        }
    }
    ok
}

/// Destroy only CEF's Alloy child `HWND` on its owning CEF UI thread while
/// keeping the host parent `HWND` alive. Refuses mismatched or recycled handles.
pub(crate) fn destroy_native_child_hwnd(hwnd: NonZeroUsize, expected_parent: NonZeroUsize) -> bool {
    if !is_valid_window(hwnd) {
        return true;
    }
    if parent_window(hwnd) != Some(expected_parent) {
        return false;
    }
    let raw = hwnd.get() as Handle;
    // SAFETY: Validated `IsWindow` and matching parent on the owning CEF UI thread.
    unsafe {
        ShowWindow(raw, SW_HIDE);
        DestroyWindow(raw) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    const WS_CHILD: u32 = 0x4000_0000;
    const WS_POPUP: u32 = 0x8000_0000;
    const WS_VISIBLE: u32 = 0x1000_0000;
    const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;

    #[allow(clashing_extern_declarations)]
    #[link(name = "user32")]
    unsafe extern "system" {
        fn CreateWindowExW(
            ex_style: u32,
            class_name: *const u16,
            window_name: *const u16,
            style: u32,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            parent: Handle,
            menu: Handle,
            instance: Handle,
            param: *mut c_void,
        ) -> Handle;
    }

    fn static_class_wide() -> Vec<u16> {
        "STATIC".encode_utf16().chain(Some(0)).collect()
    }

    struct ScopedWindow(Handle);

    impl Drop for ScopedWindow {
        fn drop(&mut self) {
            if self.0 != 0 {
                unsafe {
                    DestroyWindow(self.0);
                }
            }
        }
    }

    #[test]
    fn build_child_window_info_matches_cefsharp_alloy_contract() {
        let parent = NonZeroUsize::new(0x1234).unwrap();
        let cfg = NativeChildHwndConfig {
            parent_hwnd: parent,
            initial_bounds: PixelRect {
                x: 12,
                y: 34,
                width: 800,
                height: 600,
            },
            activate_on_creation: false,
        };
        let info = build_child_window_info(&cfg);
        assert_eq!(info.parent_window.0 as usize, parent.get());
        assert_eq!(info.windowless_rendering_enabled, 0);
        assert_eq!(info.shared_texture_enabled, 0);
        assert!(matches!(info.runtime_style, RuntimeStyle::ALLOY));
        assert_eq!(info.bounds.x, 12);
        assert_eq!(info.bounds.y, 34);
        assert_eq!(info.bounds.width, 800);
        assert_eq!(info.bounds.height, 600);
        assert_ne!(info.ex_style & (WS_EX_NOACTIVATE as u32), 0);

        let activate_cfg = NativeChildHwndConfig {
            activate_on_creation: true,
            ..cfg
        };
        let activate_info = build_child_window_info(&activate_cfg);
        assert_eq!(activate_info.ex_style & (WS_EX_NOACTIVATE as u32), 0);
    }

    #[test]
    fn real_win32_child_hwnd_strip_resize_hide_restore_and_destroyed_safety() {
        let class = static_class_wide();
        let parent_raw = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                ptr::null(),
                WS_POPUP,
                100,
                100,
                640,
                480,
                0,
                0,
                0,
                ptr::null_mut(),
            )
        };
        assert_ne!(parent_raw, 0, "hidden test parent window must be created");
        let _parent_guard = ScopedWindow(parent_raw);
        let parent = NonZeroUsize::new(parent_raw as usize).unwrap();

        let child_raw = unsafe {
            CreateWindowExW(
                (WS_EX_NOACTIVATE as u32) | WS_EX_TOOLWINDOW,
                class.as_ptr(),
                ptr::null(),
                WS_CHILD | WS_VISIBLE,
                10,
                20,
                300,
                200,
                parent_raw,
                0,
                0,
                ptr::null_mut(),
            )
        };
        assert_ne!(child_raw, 0, "test child window must be created");
        let mut child_guard = ScopedWindow(child_raw);
        let child = NonZeroUsize::new(child_raw as usize).unwrap();

        assert!(is_valid_window(parent));
        assert!(is_valid_window(child));
        assert_eq!(parent_window(child), Some(parent));
        assert_ne!(window_ex_style(child) & WS_EX_NOACTIVATE, 0);
        assert_ne!(window_ex_style(child) & (WS_EX_TOOLWINDOW as isize), 0);

        assert!(strip_ex_noactivate(child));
        assert_eq!(window_ex_style(child) & WS_EX_NOACTIVATE, 0);
        assert_ne!(window_ex_style(child) & (WS_EX_TOOLWINDOW as isize), 0);

        let moved = PixelRect {
            x: 24,
            y: 48,
            width: 420,
            height: 260,
        };
        assert!(apply_native_bounds(child, moved));
        assert_eq!(window_rect_in_parent(child, parent), Some(moved));

        assert!(hide_native_hwnd(child));
        assert!(!is_window_visible(child));
        let hidden_rect = window_rect_in_parent(child, parent).unwrap();
        assert_eq!(hidden_rect.width, 0);
        assert_eq!(hidden_rect.height, 0);

        let restored = PixelRect {
            x: 8,
            y: 16,
            width: 512,
            height: 320,
        };
        assert!(show_native_hwnd(child, restored));
        assert_eq!(window_rect_in_parent(child, parent), Some(restored));

        let raw_to_destroy = child_guard.0;
        child_guard.0 = 0;
        assert_ne!(unsafe { DestroyWindow(raw_to_destroy) }, 0);
        assert!(!is_valid_window(child));
        assert!(!strip_ex_noactivate(child));
        assert!(!apply_native_bounds(child, restored));
        assert!(!hide_native_hwnd(child));
        assert!(!show_native_hwnd(child, restored));
    }

    #[test]
    fn destroy_native_child_hwnd_checks_parent_preserves_host_and_is_idempotent() {
        let class = static_class_wide();
        let parent_raw = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                ptr::null(),
                WS_POPUP,
                100,
                100,
                400,
                300,
                0,
                0,
                0,
                ptr::null_mut(),
            )
        };
        assert_ne!(parent_raw, 0, "host parent window must be created");
        let _parent_guard = ScopedWindow(parent_raw);
        let parent = NonZeroUsize::new(parent_raw as usize).unwrap();

        let child_raw = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                ptr::null(),
                WS_CHILD | WS_VISIBLE,
                0,
                0,
                200,
                150,
                parent_raw,
                0,
                0,
                ptr::null_mut(),
            )
        };
        assert_ne!(child_raw, 0, "child window must be created");
        let mut child_guard = ScopedWindow(child_raw);
        let child = NonZeroUsize::new(child_raw as usize).unwrap();
        let wrong_parent = NonZeroUsize::new(parent.get().wrapping_add(16).max(1)).unwrap();

        assert!(!destroy_native_child_hwnd(child, wrong_parent));
        assert!(is_valid_window(child));
        assert!(is_valid_window(parent));

        assert!(destroy_native_child_hwnd(child, parent));
        child_guard.0 = 0;
        assert!(!is_valid_window(child));
        assert!(is_valid_window(parent));
        assert!(destroy_native_child_hwnd(child, parent));
    }

    #[test]
    fn build_offscreen_window_info_binds_parent_hwnd_and_shared_texture() {
        let hwnd = NonZeroUsize::new(0x1234_5678).unwrap();
        let with_parent = build_offscreen_window_info(Some(hwnd), true);
        assert_eq!(with_parent.windowless_rendering_enabled, 1);
        assert_eq!(with_parent.shared_texture_enabled, 1);
        assert_eq!(with_parent.runtime_style, RuntimeStyle::ALLOY);
        assert_eq!(with_parent.parent_window.0 as usize, hwnd.get());

        let without_parent = build_offscreen_window_info(None, false);
        assert_eq!(without_parent.windowless_rendering_enabled, 1);
        assert_eq!(without_parent.shared_texture_enabled, 0);
        assert_eq!(without_parent.runtime_style, RuntimeStyle::ALLOY);
        assert!(without_parent.parent_window.0.is_null());
    }

    #[test]
    fn client_to_screen_converts_real_window_client_pixels_and_rejects_invalid_hwnd() {
        let class = static_class_wide();
        let raw = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                ptr::null(),
                WS_POPUP,
                120,
                140,
                400,
                300,
                0,
                0,
                0,
                ptr::null_mut(),
            )
        };
        assert_ne!(raw, 0, "popup window must be created");
        let _guard = ScopedWindow(raw);
        let hwnd = NonZeroUsize::new(raw as usize).unwrap();

        let (x0, y0) = client_to_screen(hwnd, 0, 0).expect("client_to_screen(0, 0)");
        let (x1, y1) = client_to_screen(hwnd, 165, 141).expect("client_to_screen(165, 141)");
        assert_eq!((x1 - x0, y1 - y0), (165, 141));
        assert_eq!(
            client_to_screen(NonZeroUsize::new(0xDEAD).unwrap(), 10, 10),
            None
        );
    }
}
