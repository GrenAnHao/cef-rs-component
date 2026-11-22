//! UI-framework-agnostic Win32 native popup menu presenter.
//!
//! Runs `CreatePopupMenu` / `InsertMenuItemW` / `TrackPopupMenuEx` directly on
//! the host window's Win32 UI thread via a private message-only helper window
//! (`HWND_MESSAGE`). Scheduling through `PostMessageW` guarantees that the
//! Win32 modal menu loop starts from the top-level message pump — outside any
//! GPUI, Rune, or host framework borrows — and can be cancelled cleanly on
//! dismissal, replacement, or shutdown via `EndMenu` + `WM_CANCELMODE`.
use rust_cef_core::menu_model::{MenuChild, MenuCommand, MenuItem, MenuModel};
use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    ptr,
    rc::Rc,
    sync::Once,
};

type Handle = isize;

const HWND_MESSAGE: Handle = -3;
const GWLP_USERDATA: i32 = -21;

const WM_DESTROY: u32 = 0x0002;
const WM_CANCELMODE: u32 = 0x001F;
const WM_TIMER: u32 = 0x0113;
const WM_APP_RUN_MENU: u32 = 0x8000 + 0x41;
const MN_GETHMENU: u32 = 0x01E1;

const TIMER_OBSERVE_ID: usize = 1;
const TIMER_OBSERVE_MS: u32 = 25;

const MIIM_STATE: u32 = 0x0001;
const MIIM_ID: u32 = 0x0002;
const MIIM_SUBMENU: u32 = 0x0004;
const MIIM_STRING: u32 = 0x0040;
const MIIM_FTYPE: u32 = 0x0100;

const MFT_STRING: u32 = 0x0000;
const MFT_RADIOCHECK: u32 = 0x0200;
const MFT_SEPARATOR: u32 = 0x0800;

const MFS_ENABLED: u32 = 0x0000;
const MFS_GRAYED: u32 = 0x0003;
const MFS_CHECKED: u32 = 0x0008;
const MFS_UNCHECKED: u32 = 0x0000;

const TPM_LEFTBUTTON: u32 = 0x0000;
const TPM_LEFTALIGN: u32 = 0x0000;
const TPM_TOPALIGN: u32 = 0x0000;
const TPM_NONOTIFY: u32 = 0x0080;
const TPM_RETURNCMD: u32 = 0x0100;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct MenuItemInfoW {
    cb_size: u32,
    f_mask: u32,
    f_type: u32,
    f_state: u32,
    w_id: u32,
    h_sub_menu: Handle,
    hbmp_checked: Handle,
    hbmp_unchecked: Handle,
    dw_item_data: usize,
    dw_type_data: *mut u16,
    cch: u32,
    hbmp_item: Handle,
}

#[repr(C)]
struct WndClassExW {
    cb_size: u32,
    style: u32,
    lpfn_wnd_proc: Option<unsafe extern "system" fn(Handle, u32, usize, isize) -> isize>,
    cb_cls_extra: i32,
    cb_wnd_extra: i32,
    h_instance: Handle,
    h_icon: Handle,
    h_cursor: Handle,
    hbr_background: Handle,
    lpsz_menu_name: *const u16,
    lpsz_class_name: *const u16,
    h_icon_sm: Handle,
}

const _: () = assert!(std::mem::size_of::<MenuItemInfoW>() == 80);
const _: () = assert!(std::mem::size_of::<WndClassExW>() == 80);

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
    fn GetModuleHandleW(lp_module_name: *const u16) -> Handle;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn IsWindow(hwnd: Handle) -> i32;
    fn IsWindowVisible(hwnd: Handle) -> i32;
    fn GetWindowThreadProcessId(hwnd: Handle, process_id: *mut u32) -> u32;
    fn ClientToScreen(hwnd: Handle, point: *mut Point) -> i32;
    fn ScreenToClient(hwnd: Handle, point: *mut Point) -> i32;
    fn GetWindowRect(hwnd: Handle, rect: *mut Rect) -> i32;
    fn GetClassNameW(hwnd: Handle, class_name: *mut u16, max_count: i32) -> i32;
    fn EnumThreadWindows(
        thread_id: u32,
        callback: unsafe extern "system" fn(Handle, isize) -> i32,
        lparam: isize,
    ) -> i32;
    fn CreatePopupMenu() -> Handle;
    fn DestroyMenu(hmenu: Handle) -> i32;
    fn InsertMenuItemW(
        hmenu: Handle,
        item: u32,
        by_position: i32,
        info: *const MenuItemInfoW,
    ) -> i32;
    fn GetMenuItemRect(hwnd: Handle, hmenu: Handle, item: u32, rect: *mut Rect) -> i32;
    fn TrackPopupMenuEx(
        hmenu: Handle,
        flags: u32,
        x: i32,
        y: i32,
        hwnd: Handle,
        lptpm: *const c_void,
    ) -> i32;
    fn EndMenu() -> i32;
    fn PostMessageW(hwnd: Handle, msg: u32, wparam: usize, lparam: isize) -> i32;
    fn SendMessageW(hwnd: Handle, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn RegisterClassExW(wc: *const WndClassExW) -> u16;
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
    fn DestroyWindow(hwnd: Handle) -> i32;
    fn DefWindowProcW(hwnd: Handle, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn SetWindowLongPtrW(hwnd: Handle, index: i32, new_long: isize) -> isize;
    fn GetWindowLongPtrW(hwnd: Handle, index: i32) -> isize;
    fn SetTimer(
        hwnd: Handle,
        event_id: usize,
        elapse: u32,
        timer_func: Option<unsafe extern "system" fn(Handle, u32, usize, u32)>,
    ) -> usize;
    fn KillTimer(hwnd: Handle, event_id: usize) -> i32;
}

/// Errors produced by the Win32 native popup menu presenter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Win32MenuError {
    InvalidOwnerWindow,
    WrongThread,
    InvalidGeometry,
    EmptyMenu,
    Win32ApiFailed(&'static str),
}

/// Screen-space physical pixel anchor bound to an owning Win32 top-level window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Win32MenuAnchor {
    pub owner_hwnd: isize,
    pub screen_x: i32,
    pub screen_y: i32,
}

impl Win32MenuAnchor {
    /// Converts a client-area DIP point on `owner_hwnd` into physical screen
    /// coordinates via `ClientToScreen`.
    pub fn from_client_dip(
        owner_hwnd: isize,
        client_dip_x: f32,
        client_dip_y: f32,
        scale: f32,
    ) -> Result<Self, Win32MenuError> {
        validate_owner_hwnd(owner_hwnd)?;
        if !client_dip_x.is_finite()
            || !client_dip_y.is_finite()
            || !scale.is_finite()
            || scale <= 0.0
        {
            return Err(Win32MenuError::InvalidGeometry);
        }
        let px_x = (client_dip_x * scale).round();
        let px_y = (client_dip_y * scale).round();
        if !px_x.is_finite()
            || !px_y.is_finite()
            || px_x < i32::MIN as f32
            || px_x > i32::MAX as f32
            || px_y < i32::MIN as f32
            || px_y > i32::MAX as f32
        {
            return Err(Win32MenuError::InvalidGeometry);
        }
        let mut pt = Point {
            x: px_x as i32,
            y: px_y as i32,
        };
        if unsafe { ClientToScreen(owner_hwnd as Handle, &mut pt) } == 0 {
            return Err(Win32MenuError::Win32ApiFailed("ClientToScreen"));
        }
        Ok(Self {
            owner_hwnd,
            screen_x: pt.x,
            screen_y: pt.y,
        })
    }
}

/// Screen-space physical pixel rectangle of a native Win32 popup menu or item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Win32MenuRect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

impl Win32MenuRect {
    fn from_win32(rect: Rect) -> Option<Self> {
        let width = rect.right.checked_sub(rect.left)?;
        let height = rect.bottom.checked_sub(rect.top)?;
        (width > 0 && height > 0).then_some(Self {
            left: rect.left,
            top: rect.top,
            width,
            height,
        })
    }

    /// Converts this screen-pixel rectangle into `owner_hwnd` client DIPs.
    pub fn to_client_dip(self, owner_hwnd: isize, scale: f32) -> Option<(f32, f32, f32, f32)> {
        if owner_hwnd == 0 || !scale.is_finite() || scale <= 0.0 {
            return None;
        }
        let mut origin = Point {
            x: self.left,
            y: self.top,
        };
        if unsafe { ScreenToClient(owner_hwnd as Handle, &mut origin) } == 0 {
            return None;
        }
        let left = origin.x as f32 / scale;
        let top = origin.y as f32 / scale;
        let width = self.width as f32 / scale;
        let height = self.height as f32 / scale;
        ([left, top, width, height].iter().all(|v| v.is_finite()) && width > 0.0 && height > 0.0)
            .then_some((left, top, width, height))
    }
}

/// One observed visible command row inside an active Win32 popup menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Win32MenuObservedItem {
    pub command_id: i32,
    pub enabled: bool,
    pub rect: Option<Win32MenuRect>,
}

/// Snapshot of the live Win32 `#32768` popup menu bounds and item rects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Win32MenuObservation {
    pub menu_rect: Option<Win32MenuRect>,
    pub items: Vec<Win32MenuObservedItem>,
}

/// Terminal outcome of one Win32 native popup menu session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Win32MenuOutcome {
    Selected { command_id: i32 },
    Cancelled,
}

/// Monotonic token identifying one scheduled Win32 menu opening.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Win32MenuToken(u64);

/// Input payload for opening one Win32 native popup menu.
#[derive(Clone, Debug)]
pub struct Win32MenuRequest {
    pub anchor: Win32MenuAnchor,
    pub model: MenuModel,
}

fn validate_owner_hwnd(owner_hwnd: isize) -> Result<(), Win32MenuError> {
    if owner_hwnd == 0 {
        return Err(Win32MenuError::InvalidOwnerWindow);
    }
    let hwnd = owner_hwnd as Handle;
    if unsafe { IsWindow(hwnd) } == 0 {
        return Err(Win32MenuError::InvalidOwnerWindow);
    }
    let mut pid = 0u32;
    let owner_thread = unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    let current_thread = unsafe { GetCurrentThreadId() };
    if owner_thread == 0 || owner_thread != current_thread {
        return Err(Win32MenuError::WrongThread);
    }
    Ok(())
}

fn utf16_nul(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

struct OwnedHmenu(Handle);

impl OwnedHmenu {
    fn new_popup() -> Result<Self, Win32MenuError> {
        let h = unsafe { CreatePopupMenu() };
        if h == 0 {
            Err(Win32MenuError::Win32ApiFailed("CreatePopupMenu"))
        } else {
            Ok(Self(h))
        }
    }

    fn raw(&self) -> Handle {
        self.0
    }

    fn into_raw(self) -> Handle {
        let h = self.0;
        std::mem::forget(self);
        h
    }
}

impl Drop for OwnedHmenu {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe {
                let _ = DestroyMenu(self.0);
            }
        }
    }
}

struct ObservedSlot {
    hmenu: Handle,
    position: u32,
    command_id: i32,
    enabled: bool,
    is_submenu: bool,
}

struct BuiltMenu {
    root: OwnedHmenu,
    slot_to_command: Vec<(u32, i32)>,
    observed_slots: Vec<ObservedSlot>,
}

impl BuiltMenu {
    fn build(model: &MenuModel) -> Result<Self, Win32MenuError> {
        let root = OwnedHmenu::new_popup()?;
        let mut slot_to_command = Vec::new();
        let mut observed_slots = Vec::new();
        let mut next_slot: u32 = 1;
        let mut root_pos: u32 = 0;

        for item in &model.entries {
            match item {
                MenuItem::Command(cmd) if cmd.visible => {
                    let slot = next_slot;
                    next_slot = next_slot
                        .checked_add(1)
                        .ok_or(Win32MenuError::Win32ApiFailed("slot overflow"))?;
                    insert_command(root.raw(), root_pos, slot, cmd, true)?;
                    slot_to_command.push((slot, cmd.command_id));
                    observed_slots.push(ObservedSlot {
                        hmenu: root.raw(),
                        position: root_pos,
                        command_id: cmd.command_id,
                        enabled: cmd.enabled,
                        is_submenu: false,
                    });
                    root_pos += 1;
                }
                MenuItem::Separator { visible: true } => {
                    insert_separator(root.raw(), root_pos)?;
                    root_pos += 1;
                }
                MenuItem::Submenu(sub) if sub.visible => {
                    let child_menu = OwnedHmenu::new_popup()?;
                    let mut child_pos: u32 = 0;
                    for child in &sub.entries {
                        match child {
                            MenuChild::Command(cmd) if cmd.visible => {
                                let slot = next_slot;
                                next_slot = next_slot
                                    .checked_add(1)
                                    .ok_or(Win32MenuError::Win32ApiFailed("slot overflow"))?;
                                let effective_enabled = sub.enabled && cmd.enabled;
                                insert_command(
                                    child_menu.raw(),
                                    child_pos,
                                    slot,
                                    cmd,
                                    sub.enabled,
                                )?;
                                slot_to_command.push((slot, cmd.command_id));
                                observed_slots.push(ObservedSlot {
                                    hmenu: child_menu.raw(),
                                    position: child_pos,
                                    command_id: cmd.command_id,
                                    enabled: effective_enabled,
                                    is_submenu: true,
                                });
                                child_pos += 1;
                            }
                            MenuChild::Separator { visible: true } => {
                                insert_separator(child_menu.raw(), child_pos)?;
                                child_pos += 1;
                            }
                            _ => {}
                        }
                    }
                    let mut label = utf16_nul(&sub.label);
                    let info = MenuItemInfoW {
                        cb_size: std::mem::size_of::<MenuItemInfoW>() as u32,
                        f_mask: MIIM_SUBMENU | MIIM_FTYPE | MIIM_STATE | MIIM_STRING,
                        f_type: MFT_STRING,
                        f_state: if sub.enabled { MFS_ENABLED } else { MFS_GRAYED },
                        w_id: 0,
                        h_sub_menu: child_menu.raw(),
                        hbmp_checked: 0,
                        hbmp_unchecked: 0,
                        dw_item_data: 0,
                        dw_type_data: label.as_mut_ptr(),
                        cch: 0,
                        hbmp_item: 0,
                    };
                    if unsafe { InsertMenuItemW(root.raw(), root_pos, 1, &info) } == 0 {
                        return Err(Win32MenuError::Win32ApiFailed("InsertMenuItemW(submenu)"));
                    }
                    // Ownership of child_menu transfers to root upon successful insert.
                    let _ = child_menu.into_raw();
                    root_pos += 1;
                }
                _ => {}
            }
        }

        if root_pos == 0 {
            return Err(Win32MenuError::EmptyMenu);
        }

        Ok(Self {
            root,
            slot_to_command,
            observed_slots,
        })
    }

    fn command_for_slot(&self, slot: u32) -> Option<i32> {
        self.slot_to_command
            .iter()
            .find_map(|&(s, cmd)| (s == slot).then_some(cmd))
    }

    fn observe(&self, _owner_hwnd: Handle) -> Win32MenuObservation {
        let popup_windows = active_popup_windows();
        let menu_rect = popup_windows
            .iter()
            .find(|&&(_, hmenu)| hmenu == self.root.raw())
            .or_else(|| popup_windows.first())
            .and_then(|&(hwnd, _)| {
                let mut r = Rect::default();
                (unsafe { GetWindowRect(hwnd, &mut r) } != 0)
                    .then(|| Win32MenuRect::from_win32(r))
                    .flatten()
            });

        let items = self
            .observed_slots
            .iter()
            .map(|slot| {
                // Only query submenu rects when that submenu HMENU is actually visible.
                let matching_popup = popup_windows
                    .iter()
                    .find(|&&(_, hmenu)| hmenu == slot.hmenu)
                    .map(|&(hwnd, _)| hwnd);
                let query = !slot.is_submenu || matching_popup.is_some();
                let rect = if query {
                    let mut r = Rect::default();
                    // Win32 GetMenuItemRect requires hWnd == NULL (0) for popup menus
                    // tracked via TrackPopupMenuEx so Windows locates the #32768 window.
                    let ok = unsafe { GetMenuItemRect(0, slot.hmenu, slot.position, &mut r) } != 0
                        || matching_popup.is_some_and(|popup_hwnd| {
                            (unsafe {
                                GetMenuItemRect(popup_hwnd, slot.hmenu, slot.position, &mut r)
                            }) != 0
                        });
                    if ok {
                        Win32MenuRect::from_win32(r)
                    } else {
                        None
                    }
                } else {
                    None
                };
                Win32MenuObservedItem {
                    command_id: slot.command_id,
                    enabled: slot.enabled,
                    rect,
                }
            })
            .collect();

        Win32MenuObservation { menu_rect, items }
    }
}

fn insert_command(
    hmenu: Handle,
    position: u32,
    slot_id: u32,
    cmd: &MenuCommand,
    parent_enabled: bool,
) -> Result<(), Win32MenuError> {
    let mut label = utf16_nul(&cmd.label);
    let enabled = parent_enabled && cmd.enabled;
    let f_type = MFT_STRING
        | if cmd.radio_group.is_some() {
            MFT_RADIOCHECK
        } else {
            0
        };
    let f_state = if enabled { MFS_ENABLED } else { MFS_GRAYED }
        | if cmd.checked == Some(true) {
            MFS_CHECKED
        } else {
            MFS_UNCHECKED
        };
    let info = MenuItemInfoW {
        cb_size: std::mem::size_of::<MenuItemInfoW>() as u32,
        f_mask: MIIM_ID | MIIM_FTYPE | MIIM_STATE | MIIM_STRING,
        f_type,
        f_state,
        w_id: slot_id,
        h_sub_menu: 0,
        hbmp_checked: 0,
        hbmp_unchecked: 0,
        dw_item_data: 0,
        dw_type_data: label.as_mut_ptr(),
        cch: 0,
        hbmp_item: 0,
    };
    if unsafe { InsertMenuItemW(hmenu, position, 1, &info) } == 0 {
        return Err(Win32MenuError::Win32ApiFailed("InsertMenuItemW(command)"));
    }
    Ok(())
}

fn insert_separator(hmenu: Handle, position: u32) -> Result<(), Win32MenuError> {
    let info = MenuItemInfoW {
        cb_size: std::mem::size_of::<MenuItemInfoW>() as u32,
        f_mask: MIIM_FTYPE,
        f_type: MFT_SEPARATOR,
        f_state: 0,
        w_id: 0,
        h_sub_menu: 0,
        hbmp_checked: 0,
        hbmp_unchecked: 0,
        dw_item_data: 0,
        dw_type_data: ptr::null_mut(),
        cch: 0,
        hbmp_item: 0,
    };
    if unsafe { InsertMenuItemW(hmenu, position, 1, &info) } == 0 {
        return Err(Win32MenuError::Win32ApiFailed("InsertMenuItemW(separator)"));
    }
    Ok(())
}

fn active_popup_windows() -> Vec<(Handle, Handle)> {
    unsafe extern "system" fn enum_proc(hwnd: Handle, lparam: isize) -> i32 {
        if unsafe { IsWindowVisible(hwnd) } == 0 {
            return 1;
        }
        let mut class_buf = [0u16; 16];
        let len = unsafe { GetClassNameW(hwnd, class_buf.as_mut_ptr(), class_buf.len() as i32) };
        if len == 6 && &class_buf[..6] == ['#', '3', '2', '7', '6', '8'].map(|c| c as u16) {
            let hmenu = unsafe { SendMessageW(hwnd, MN_GETHMENU, 0, 0) } as Handle;
            let out = unsafe { &mut *(lparam as *mut Vec<(Handle, Handle)>) };
            out.push((hwnd, hmenu));
        }
        1
    }
    let mut found = Vec::new();
    let tid = unsafe { GetCurrentThreadId() };
    unsafe {
        let _ = EnumThreadWindows(tid, enum_proc, (&mut found as *mut Vec<_>) as isize);
    }
    found
}

struct PendingOpen {
    token: Win32MenuToken,
    request: Win32MenuRequest,
    on_observe: Box<dyn FnMut(Win32MenuObservation)>,
    on_complete: Box<dyn FnOnce(Win32MenuOutcome)>,
}

struct ActiveModal {
    token: Win32MenuToken,
    owner_hwnd: Handle,
    built: BuiltMenu,
    on_observe: Option<Box<dyn FnMut(Win32MenuObservation)>>,
    cancelled_externally: bool,
}

struct HostInner {
    thread_id: u32,
    helper_hwnd: Cell<Handle>,
    next_token: Cell<u64>,
    in_modal_loop: Cell<bool>,
    pending: RefCell<Option<PendingOpen>>,
    active: RefCell<Option<ActiveModal>>,
}

impl HostInner {
    fn ensure_thread(&self) -> Result<(), Win32MenuError> {
        if unsafe { GetCurrentThreadId() } != self.thread_id {
            Err(Win32MenuError::WrongThread)
        } else {
            Ok(())
        }
    }

    fn cancel_active_modal(&self) {
        let owner = {
            let mut active = self.active.borrow_mut();
            if let Some(modal) = active.as_mut() {
                modal.cancelled_externally = true;
                Some(modal.owner_hwnd)
            } else {
                None
            }
        };
        if let Some(owner_hwnd) = owner {
            unsafe {
                // EndMenu() terminates the active modal menu loop directly. Only
                // fall back to posting WM_CANCELMODE when EndMenu() reports no
                // active menu, so a leftover WM_CANCELMODE cannot linger in
                // owner_hwnd's queue and prematurely kill a queued replacement.
                if EndMenu() == 0 {
                    let _ = PostMessageW(owner_hwnd, WM_CANCELMODE, 0, 0);
                }
            }
        }
    }

    fn run_pending(self: &Rc<Self>) {
        if self.in_modal_loop.get() {
            // Only unwind the active modal if a replacement is actually queued;
            // a duplicate WM_APP_RUN_MENU must never kill a freshly opened menu.
            if self.pending.borrow().is_some() {
                self.cancel_active_modal();
            }
            return;
        }
        let Some(pending) = self.pending.borrow_mut().take() else {
            return;
        };
        if validate_owner_hwnd(pending.request.anchor.owner_hwnd).is_err() {
            (pending.on_complete)(Win32MenuOutcome::Cancelled);
            return;
        }
        let built = match BuiltMenu::build(&pending.request.model) {
            Ok(built) => built,
            Err(_) => {
                (pending.on_complete)(Win32MenuOutcome::Cancelled);
                return;
            }
        };

        let owner_hwnd = pending.request.anchor.owner_hwnd as Handle;
        let root_hmenu = built.root.raw();
        let helper_hwnd = self.helper_hwnd.get();

        *self.active.borrow_mut() = Some(ActiveModal {
            token: pending.token,
            owner_hwnd,
            built,
            on_observe: Some(pending.on_observe),
            cancelled_externally: false,
        });
        self.in_modal_loop.set(true);

        if helper_hwnd != 0 {
            unsafe {
                let _ = SetTimer(helper_hwnd, TIMER_OBSERVE_ID, TIMER_OBSERVE_MS, None);
            }
        }

        let chosen = unsafe {
            TrackPopupMenuEx(
                root_hmenu,
                TPM_LEFTBUTTON | TPM_LEFTALIGN | TPM_TOPALIGN | TPM_NONOTIFY | TPM_RETURNCMD,
                pending.request.anchor.screen_x,
                pending.request.anchor.screen_y,
                owner_hwnd,
                ptr::null(),
            )
        };

        if helper_hwnd != 0 {
            unsafe {
                let _ = KillTimer(helper_hwnd, TIMER_OBSERVE_ID);
            }
        }
        self.in_modal_loop.set(false);

        let finished = self.active.borrow_mut().take();
        let outcome = match finished {
            Some(modal) if !modal.cancelled_externally && chosen > 0 => modal
                .built
                .command_for_slot(chosen as u32)
                .map_or(Win32MenuOutcome::Cancelled, |command_id| {
                    Win32MenuOutcome::Selected { command_id }
                }),
            _ => Win32MenuOutcome::Cancelled,
        };

        // Invoke completion outside any internal RefCell borrow.
        (pending.on_complete)(outcome);

        // If a replacement menu was scheduled while we were inside the modal
        // loop, dispatch it on the next top-level message turn.
        if self.pending.borrow().is_some() && helper_hwnd != 0 {
            unsafe {
                let _ = PostMessageW(helper_hwnd, WM_APP_RUN_MENU, 0, 0);
            }
        }
    }

    fn observe_active(&self) {
        let taken = {
            let mut active = self.active.borrow_mut();
            active.as_mut().and_then(|modal| {
                let cb = modal.on_observe.take()?;
                let obs = modal.built.observe(modal.owner_hwnd);
                Some((modal.token, obs, cb))
            })
        };
        if let Some((token, obs, mut cb)) = taken {
            // Invoke callback with zero internal RefCell borrows held so
            // callers may safely call cancel() re-entrantly.
            cb(obs);
            let mut active = self.active.borrow_mut();
            if let Some(modal) = active.as_mut() {
                if modal.token == token && modal.on_observe.is_none() {
                    modal.on_observe = Some(cb);
                }
            }
        }
    }
}

static REGISTER_CLASS: Once = Once::new();
const HELPER_CLASS_NAME: &str = "GpuiCefWin32MenuHost";

fn ensure_helper_class(instance: Handle) -> Vec<u16> {
    let class_name = utf16_nul(HELPER_CLASS_NAME);
    REGISTER_CLASS.call_once(|| {
        let wc = WndClassExW {
            cb_size: std::mem::size_of::<WndClassExW>() as u32,
            style: 0,
            lpfn_wnd_proc: Some(helper_wnd_proc),
            cb_cls_extra: 0,
            cb_wnd_extra: 0,
            h_instance: instance,
            h_icon: 0,
            h_cursor: 0,
            hbr_background: 0,
            lpsz_menu_name: ptr::null(),
            lpsz_class_name: class_name.as_ptr(),
            h_icon_sm: 0,
        };
        unsafe {
            let _ = RegisterClassExW(&wc);
        }
    });
    class_name
}

unsafe extern "system" fn helper_wnd_proc(
    hwnd: Handle,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const HostInner;
    if !ptr.is_null() {
        // Borrow the Rc without consuming the window's stored raw count.
        unsafe {
            Rc::increment_strong_count(ptr);
        }
        let inner = unsafe { Rc::from_raw(ptr) };
        match msg {
            WM_APP_RUN_MENU => {
                inner.run_pending();
                return 0;
            }
            WM_TIMER if wparam == TIMER_OBSERVE_ID => {
                inner.observe_active();
                return 0;
            }
            WM_DESTROY => {
                unsafe {
                    let _ = KillTimer(hwnd, TIMER_OBSERVE_ID);
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    // Release the Rc count owned by GWLP_USERDATA.
                    Rc::decrement_strong_count(ptr);
                }
                inner.helper_hwnd.set(0);
                return 0;
            }
            _ => {}
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// UI-framework-agnostic single-UI-thread host for Win32 native popup menus.
///
/// Any Windows UI framework (GPUI, Rune, raw Win32) can own one of these on its
/// window thread, call [`schedule_open`] when a context menu arrives, and call
/// [`cancel`] on dismiss/close/shutdown.
pub struct Win32ContextMenuHost {
    inner: Rc<HostInner>,
}

impl Win32ContextMenuHost {
    /// Creates a new native menu host bound to the current UI thread.
    pub fn new() -> Result<Self, Win32MenuError> {
        let thread_id = unsafe { GetCurrentThreadId() };
        let instance = unsafe { GetModuleHandleW(ptr::null()) };
        let class_name = ensure_helper_class(instance);
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                0,
                instance,
                ptr::null_mut(),
            )
        };
        if hwnd == 0 {
            return Err(Win32MenuError::Win32ApiFailed("CreateWindowExW"));
        }
        let inner = Rc::new(HostInner {
            thread_id,
            helper_hwnd: Cell::new(hwnd),
            next_token: Cell::new(1),
            in_modal_loop: Cell::new(false),
            pending: RefCell::new(None),
            active: RefCell::new(None),
        });
        let raw = Rc::into_raw(inner.clone());
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
        }
        Ok(Self { inner })
    }

    /// Schedules a Win32 native popup menu to open on the next top-level Win32
    /// message loop turn, outside any caller framework borrows.
    ///
    /// Any previously queued or active menu is cleanly cancelled first.
    pub fn schedule_open(
        &self,
        request: Win32MenuRequest,
        on_observe: Box<dyn FnMut(Win32MenuObservation)>,
        on_complete: Box<dyn FnOnce(Win32MenuOutcome)>,
    ) -> Result<Win32MenuToken, Win32MenuError> {
        self.inner.ensure_thread()?;
        validate_owner_hwnd(request.anchor.owner_hwnd)?;
        let helper = self.inner.helper_hwnd.get();
        if helper == 0 {
            return Err(Win32MenuError::Win32ApiFailed("helper window closed"));
        }

        // Cancel any queued menu before replacing it.
        if let Some(old) = self.inner.pending.borrow_mut().take() {
            (old.on_complete)(Win32MenuOutcome::Cancelled);
        }
        self.inner.cancel_active_modal();

        let token = Win32MenuToken(self.inner.next_token.get());
        self.inner
            .next_token
            .set(self.inner.next_token.get().saturating_add(1));

        *self.inner.pending.borrow_mut() = Some(PendingOpen {
            token,
            request,
            on_observe,
            on_complete,
        });

        if !self.inner.in_modal_loop.get()
            && unsafe { PostMessageW(helper, WM_APP_RUN_MENU, 0, 0) } == 0
        {
            let _ = self.inner.pending.borrow_mut().take();
            return Err(Win32MenuError::Win32ApiFailed("PostMessageW"));
        }
        Ok(token)
    }

    /// Cancels a matching queued or active menu (or any open menu when `token`
    /// is `None`). Safe to call during dismissal, window close, or shutdown.
    pub fn cancel(&self, token: Option<Win32MenuToken>) {
        if self.inner.ensure_thread().is_err() {
            return;
        }
        let cancelled_pending = {
            let mut slot = self.inner.pending.borrow_mut();
            let matches = slot
                .as_ref()
                .is_some_and(|p| token.is_none_or(|t| t == p.token));
            if matches { slot.take() } else { None }
        };
        if let Some(old) = cancelled_pending {
            (old.on_complete)(Win32MenuOutcome::Cancelled);
        }
        let active_matches = self
            .inner
            .active
            .borrow()
            .as_ref()
            .is_some_and(|m| token.is_none_or(|t| t == m.token));
        if active_matches {
            self.inner.cancel_active_modal();
        }
    }

    /// Returns whether a Win32 modal popup menu loop is currently running.
    pub fn is_modal_active(&self) -> bool {
        self.inner.in_modal_loop.get()
    }
}

impl Drop for Win32ContextMenuHost {
    fn drop(&mut self) {
        self.cancel(None);
        let hwnd = self.inner.helper_hwnd.replace(0);
        if hwnd != 0 {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::menu_model::MenuSubmenu;

    #[repr(C)]
    struct Msg {
        hwnd: Handle,
        message: u32,
        wparam: usize,
        lparam: isize,
        time: u32,
        pt: Point,
        lprivate: u32,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetMenuItemCount(hmenu: Handle) -> i32;
        fn GetMenuItemInfoW(
            hmenu: Handle,
            item: u32,
            by_position: i32,
            info: *mut MenuItemInfoW,
        ) -> i32;
        fn PeekMessageW(
            msg: *mut Msg,
            hwnd: Handle,
            min_filter: u32,
            max_filter: u32,
            remove: u32,
        ) -> i32;
        fn TranslateMessage(msg: *const Msg) -> i32;
        fn DispatchMessageW(msg: *const Msg) -> isize;
        fn ShowWindow(hwnd: Handle, cmd_show: i32) -> i32;
    }

    fn read_info(hmenu: Handle, pos: u32) -> (u32, u32, u32, Handle, String) {
        let mut buf = [0u16; 128];
        let mut info = MenuItemInfoW {
            cb_size: std::mem::size_of::<MenuItemInfoW>() as u32,
            f_mask: MIIM_ID | MIIM_FTYPE | MIIM_STATE | MIIM_SUBMENU | MIIM_STRING,
            f_type: 0,
            f_state: 0,
            w_id: 0,
            h_sub_menu: 0,
            hbmp_checked: 0,
            hbmp_unchecked: 0,
            dw_item_data: 0,
            dw_type_data: buf.as_mut_ptr(),
            cch: buf.len() as u32,
            hbmp_item: 0,
        };
        assert_ne!(unsafe { GetMenuItemInfoW(hmenu, pos, 1, &mut info) }, 0);
        let text = String::from_utf16_lossy(&buf[..info.cch as usize]);
        (info.w_id, info.f_type, info.f_state, info.h_sub_menu, text)
    }

    #[test]
    fn builds_native_hmenu_with_mnemonics_checks_radios_separators_and_submenus() {
        let model = MenuModel {
            x: 10,
            y: 20,
            entries: vec![
                MenuItem::Command(MenuCommand {
                    command_id: 110,
                    label: "&Undo".into(),
                    enabled: false,
                    visible: true,
                    checked: None,
                    radio_group: None,
                }),
                MenuItem::Command(MenuCommand {
                    command_id: -5,
                    label: "Hidden".into(),
                    enabled: true,
                    visible: false,
                    checked: None,
                    radio_group: None,
                }),
                MenuItem::Separator { visible: true },
                MenuItem::Command(MenuCommand {
                    command_id: 28500,
                    label: "Show bookmarks bar".into(),
                    enabled: true,
                    visible: true,
                    checked: Some(true),
                    radio_group: None,
                }),
                MenuItem::Command(MenuCommand {
                    command_id: 0,
                    label: "Radio option".into(),
                    enabled: true,
                    visible: true,
                    checked: Some(true),
                    radio_group: Some(7),
                }),
                MenuItem::Submenu(MenuSubmenu {
                    label: "More".into(),
                    enabled: true,
                    visible: true,
                    entries: vec![MenuChild::Command(MenuCommand {
                        command_id: 28501,
                        label: "Host submenu item".into(),
                        enabled: true,
                        visible: true,
                        checked: None,
                        radio_group: None,
                    })],
                }),
                MenuItem::Submenu(MenuSubmenu {
                    label: "Disabled Sub".into(),
                    enabled: false,
                    visible: true,
                    entries: vec![MenuChild::Command(MenuCommand {
                        command_id: 28502,
                        label: "Child".into(),
                        enabled: true,
                        visible: true,
                        checked: None,
                        radio_group: None,
                    })],
                }),
            ],
        };

        let built = BuiltMenu::build(&model).expect("native HMENU builds");
        assert_eq!(unsafe { GetMenuItemCount(built.root.raw()) }, 6);

        let (id0, _, st0, sub0, txt0) = read_info(built.root.raw(), 0);
        assert_eq!(txt0, "&Undo");
        assert_eq!(st0 & MFS_GRAYED, MFS_GRAYED);
        assert_eq!(sub0, 0);
        assert_eq!(built.command_for_slot(id0), Some(110));

        let (_, ty1, _, _, _) = read_info(built.root.raw(), 1);
        assert_eq!(ty1 & MFT_SEPARATOR, MFT_SEPARATOR);

        let (id2, _, st2, _, txt2) = read_info(built.root.raw(), 2);
        assert_eq!(txt2, "Show bookmarks bar");
        assert_eq!(st2 & MFS_CHECKED, MFS_CHECKED);
        assert_eq!(built.command_for_slot(id2), Some(28500));

        let (id3, ty3, st3, _, _) = read_info(built.root.raw(), 3);
        assert_eq!(ty3 & MFT_RADIOCHECK, MFT_RADIOCHECK);
        assert_eq!(st3 & MFS_CHECKED, MFS_CHECKED);
        // Zero command ID maps unambiguously through a positive Win32 slot ID.
        assert!(id3 > 0);
        assert_eq!(built.command_for_slot(id3), Some(0));

        let (_, _, st4, sub4, txt4) = read_info(built.root.raw(), 4);
        assert_eq!(txt4, "More");
        assert_eq!(st4 & MFS_GRAYED, 0);
        assert_ne!(sub4, 0);
        assert_eq!(unsafe { GetMenuItemCount(sub4) }, 1);
        let (sub_id0, _, sub_st0, _, sub_txt0) = read_info(sub4, 0);
        assert_eq!(sub_txt0, "Host submenu item");
        assert_eq!(sub_st0 & MFS_GRAYED, 0);
        assert_eq!(built.command_for_slot(sub_id0), Some(28501));

        let (_, _, st5, sub5, _) = read_info(built.root.raw(), 5);
        assert_eq!(st5 & MFS_GRAYED, MFS_GRAYED);
        assert_ne!(sub5, 0);
        let (_, _, child_st, _, _) = read_info(sub5, 0);
        assert_eq!(child_st & MFS_GRAYED, MFS_GRAYED);
    }

    #[test]
    fn empty_or_all_hidden_model_is_rejected() {
        let empty = MenuModel::default();
        assert_eq!(
            BuiltMenu::build(&empty).err(),
            Some(Win32MenuError::EmptyMenu)
        );
    }

    #[test]
    fn host_creates_message_window_and_drops_cleanly() {
        let host = Win32ContextMenuHost::new().expect("message window created");
        assert!(!host.is_modal_active());
        host.cancel(None);
        drop(host);
    }

    static LIVE_POPUP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn observes_live_popup_item_rects_and_cancels_cleanly() {
        let _serial = LIVE_POPUP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let instance = unsafe { GetModuleHandleW(ptr::null()) };
        let class_name = utf16_nul("STATIC");
        let owner = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                ptr::null(),
                0x8000_0000, // WS_POPUP
                100,
                100,
                320,
                240,
                0,
                0,
                instance,
                ptr::null_mut(),
            )
        };
        assert_ne!(owner, 0, "owner window must be created");
        unsafe {
            let _ = ShowWindow(owner, 4); // SW_SHOWNOACTIVATE
        }

        let host = Win32ContextMenuHost::new().expect("menu host created");
        let model = MenuModel {
            x: 10,
            y: 10,
            entries: vec![
                MenuItem::Command(MenuCommand {
                    command_id: 110,
                    label: "&Undo".into(),
                    enabled: true,
                    visible: true,
                    checked: None,
                    radio_group: None,
                }),
                MenuItem::Command(MenuCommand {
                    command_id: 28500,
                    label: "Host Toggle".into(),
                    enabled: true,
                    visible: true,
                    checked: Some(true),
                    radio_group: None,
                }),
            ],
        };

        let observed = Rc::new(RefCell::new(None::<Win32MenuObservation>));
        let observed_cb = observed.clone();
        let outcome = Rc::new(RefCell::new(None::<Win32MenuOutcome>));
        let outcome_cb = outcome.clone();
        let inner_cancel = Rc::downgrade(&host.inner);

        let _token = host
            .schedule_open(
                Win32MenuRequest {
                    anchor: Win32MenuAnchor {
                        owner_hwnd: owner,
                        screen_x: 120,
                        screen_y: 120,
                    },
                    model,
                },
                Box::new(move |obs| {
                    let all_have_rects = obs.menu_rect.is_some()
                        && obs.items.len() == 2
                        && obs.items.iter().all(|it| it.rect.is_some());
                    *observed_cb.borrow_mut() = Some(obs);
                    if all_have_rects {
                        if let Some(inner) = inner_cancel.upgrade() {
                            inner.cancel_active_modal();
                        }
                    }
                }),
                Box::new(move |res| {
                    *outcome_cb.borrow_mut() = Some(res);
                }),
            )
            .expect("schedule_open succeeds");

        // Pump the thread message queue so WM_APP_RUN_MENU runs TrackPopupMenuEx.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut msg = Msg {
            hwnd: 0,
            message: 0,
            wparam: 0,
            lparam: 0,
            time: 0,
            pt: Point { x: 0, y: 0 },
            lprivate: 0,
        };
        while outcome.borrow().is_none() && std::time::Instant::now() < deadline {
            while unsafe { PeekMessageW(&mut msg, 0, 0, 0, 1) } != 0 {
                unsafe {
                    let _ = TranslateMessage(&msg);
                    let _ = DispatchMessageW(&msg);
                }
            }
            if outcome.borrow().is_none() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        drop(host);
        unsafe {
            let _ = DestroyWindow(owner);
        }

        let obs = observed
            .borrow()
            .clone()
            .expect("on_observe must fire during live TrackPopupMenuEx");
        assert!(obs.menu_rect.is_some(), "popup menu_rect missing: {obs:?}");
        for item in &obs.items {
            let r = item
                .rect
                .unwrap_or_else(|| panic!("row {} rect missing: {obs:?}", item.command_id));
            assert!(r.width > 0 && r.height > 0, "row rect empty: {r:?}");
        }
        assert_eq!(*outcome.borrow(), Some(Win32MenuOutcome::Cancelled));
    }

    fn create_test_owner_window() -> Handle {
        let instance = unsafe { GetModuleHandleW(ptr::null()) };
        let class_name = utf16_nul("STATIC");
        let owner = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                ptr::null(),
                0x8000_0000, // WS_POPUP
                100,
                100,
                320,
                240,
                0,
                0,
                instance,
                ptr::null_mut(),
            )
        };
        assert_ne!(owner, 0, "owner window must be created");
        unsafe {
            let _ = ShowWindow(owner, 4); // SW_SHOWNOACTIVATE
        }
        owner
    }

    fn pump_thread_until(done: impl Fn() -> bool, timeout: std::time::Duration) {
        let deadline = std::time::Instant::now() + timeout;
        let mut msg = Msg {
            hwnd: 0,
            message: 0,
            wparam: 0,
            lparam: 0,
            time: 0,
            pt: Point { x: 0, y: 0 },
            lprivate: 0,
        };
        while !done() && std::time::Instant::now() < deadline {
            while unsafe { PeekMessageW(&mut msg, 0, 0, 0, 1) } != 0 {
                unsafe {
                    let _ = TranslateMessage(&msg);
                    let _ = DispatchMessageW(&msg);
                }
            }
            if !done() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }

    fn sample_two_row_model(first_id: i32, second_id: i32) -> MenuModel {
        MenuModel {
            x: 10,
            y: 10,
            entries: vec![
                MenuItem::Command(MenuCommand {
                    command_id: first_id,
                    label: "&Undo".into(),
                    enabled: true,
                    visible: true,
                    checked: None,
                    radio_group: None,
                }),
                MenuItem::Command(MenuCommand {
                    command_id: second_id,
                    label: "Host Toggle".into(),
                    enabled: true,
                    visible: true,
                    checked: Some(true),
                    radio_group: None,
                }),
            ],
        }
    }

    #[test]
    fn live_popup_dismisses_on_escape_key() {
        let _serial = LIVE_POPUP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        const WM_KEYDOWN: u32 = 0x0100;
        const WM_KEYUP: u32 = 0x0101;
        const VK_ESCAPE: usize = 0x1B;

        let owner = create_test_owner_window();
        let host = Win32ContextMenuHost::new().expect("menu host created");
        let sent_escape = Rc::new(Cell::new(false));
        let sent_cb = sent_escape.clone();
        let completions = Rc::new(RefCell::new(Vec::<Win32MenuOutcome>::new()));
        let comp_cb = completions.clone();

        let _token = host
            .schedule_open(
                Win32MenuRequest {
                    anchor: Win32MenuAnchor {
                        owner_hwnd: owner,
                        screen_x: 140,
                        screen_y: 140,
                    },
                    model: sample_two_row_model(110, 28500),
                },
                Box::new(move |obs| {
                    if !sent_cb.get()
                        && obs.menu_rect.is_some()
                        && obs.items.iter().all(|it| it.rect.is_some())
                    {
                        sent_cb.set(true);
                        let target = active_popup_windows()
                            .first()
                            .map(|&(hwnd, _)| hwnd)
                            .unwrap_or(owner);
                        unsafe {
                            let _ = PostMessageW(target, WM_KEYDOWN, VK_ESCAPE, 0);
                            let _ = PostMessageW(target, WM_KEYUP, VK_ESCAPE, 0);
                        }
                    }
                }),
                Box::new(move |res| {
                    comp_cb.borrow_mut().push(res);
                }),
            )
            .expect("schedule_open succeeds");

        pump_thread_until(
            || !completions.borrow().is_empty(),
            std::time::Duration::from_secs(3),
        );

        let modal_after = host.is_modal_active();
        drop(host);
        unsafe {
            let _ = DestroyWindow(owner);
        }

        assert!(sent_escape.get(), "live popup must be observed before ESC");
        assert!(!modal_after, "modal loop must unwind after ESC");
        assert_eq!(
            *completions.borrow(),
            vec![Win32MenuOutcome::Cancelled],
            "ESC must produce exactly one Cancelled outcome"
        );
    }

    #[test]
    fn live_popup_dismisses_on_outside_click_and_cancel_mode() {
        let _serial = LIVE_POPUP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        const WM_LBUTTONDOWN: u32 = 0x0201;
        const WM_LBUTTONUP: u32 = 0x0202;

        for use_cancel_mode in [false, true] {
            let owner = create_test_owner_window();
            let host = Win32ContextMenuHost::new().expect("menu host created");
            let triggered = Rc::new(Cell::new(false));
            let trig_cb = triggered.clone();
            let completions = Rc::new(RefCell::new(Vec::<Win32MenuOutcome>::new()));
            let comp_cb = completions.clone();

            let _token = host
                .schedule_open(
                    Win32MenuRequest {
                        anchor: Win32MenuAnchor {
                            owner_hwnd: owner,
                            screen_x: 180,
                            screen_y: 180,
                        },
                        model: sample_two_row_model(110, 28500),
                    },
                    Box::new(move |obs| {
                        if !trig_cb.get()
                            && let Some(menu_rect) = obs.menu_rect
                            && obs.items.iter().all(|it| it.rect.is_some())
                        {
                            trig_cb.set(true);
                            if use_cancel_mode {
                                // Exact Win32 notification emitted on focus loss / deactivation.
                                unsafe {
                                    let _ = PostMessageW(owner, WM_CANCELMODE, 0, 0);
                                }
                            } else {
                                // Click at owner client point strictly outside the #32768 popup rect.
                                let mut outside = Point {
                                    x: menu_rect.left - 30,
                                    y: menu_rect.top - 30,
                                };
                                unsafe {
                                    let _ = ScreenToClient(owner, &mut outside);
                                }
                                let lparam = ((outside.y as u16 as u32) << 16
                                    | (outside.x as u16 as u32))
                                    as isize;
                                unsafe {
                                    let _ = PostMessageW(owner, WM_LBUTTONDOWN, 0x0001, lparam);
                                    let _ = PostMessageW(owner, WM_LBUTTONUP, 0, lparam);
                                }
                            }
                        }
                    }),
                    Box::new(move |res| {
                        comp_cb.borrow_mut().push(res);
                    }),
                )
                .expect("schedule_open succeeds");

            pump_thread_until(
                || !completions.borrow().is_empty(),
                std::time::Duration::from_secs(3),
            );

            let modal_after = host.is_modal_active();
            drop(host);
            unsafe {
                let _ = DestroyWindow(owner);
            }

            assert!(triggered.get(), "popup must be observed before dismissal");
            assert!(!modal_after, "modal loop must unwind after dismissal");
            assert_eq!(
                *completions.borrow(),
                vec![Win32MenuOutcome::Cancelled],
                "outside click / WM_CANCELMODE must emit exactly one Cancelled outcome"
            );
        }
    }

    #[test]
    fn live_popup_replacement_cancels_first_before_opening_second() {
        let _serial = LIVE_POPUP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let owner = create_test_owner_window();
        let host = Rc::new(Win32ContextMenuHost::new().expect("menu host created"));
        let log = Rc::new(RefCell::new(Vec::<String>::new()));

        let first_replaced = Rc::new(Cell::new(false));
        let host_for_first = host.clone();
        let log_for_obs1 = log.clone();
        let log_for_done1 = log.clone();
        let log_for_obs2 = log.clone();
        let log_for_done2 = log.clone();
        let done2 = Rc::new(Cell::new(false));
        let done2_cb = done2.clone();

        let _tok1 = host
            .schedule_open(
                Win32MenuRequest {
                    anchor: Win32MenuAnchor {
                        owner_hwnd: owner,
                        screen_x: 130,
                        screen_y: 130,
                    },
                    model: sample_two_row_model(110, 117),
                },
                Box::new(move |obs| {
                    if !first_replaced.get()
                        && obs.menu_rect.is_some()
                        && obs.items.iter().all(|it| it.rect.is_some())
                    {
                        first_replaced.set(true);
                        log_for_obs1.borrow_mut().push("first-observed".into());
                        let host_cancel = Rc::downgrade(&host_for_first);
                        let log_o2 = log_for_obs2.clone();
                        let log_d2 = log_for_done2.clone();
                        let d2 = done2_cb.clone();
                        let second_cancelled = Cell::new(false);
                        let _tok2 = host_for_first
                            .schedule_open(
                                Win32MenuRequest {
                                    anchor: Win32MenuAnchor {
                                        owner_hwnd: owner,
                                        screen_x: 190,
                                        screen_y: 170,
                                    },
                                    model: sample_two_row_model(28500, 28501),
                                },
                                Box::new(move |obs2| {
                                    if !second_cancelled.get()
                                        && obs2.menu_rect.is_some()
                                        && obs2.items.len() == 2
                                        && obs2.items[0].command_id == 28500
                                        && obs2.items.iter().all(|it| it.rect.is_some())
                                    {
                                        second_cancelled.set(true);
                                        log_o2.borrow_mut().push("second-observed".into());
                                        if let Some(h) = host_cancel.upgrade() {
                                            h.cancel(None);
                                        }
                                    }
                                }),
                                Box::new(move |res2| {
                                    log_d2
                                        .borrow_mut()
                                        .push(format!("second-completed:{res2:?}"));
                                    d2.set(true);
                                }),
                            )
                            .expect("replacement schedule_open succeeds");
                    }
                }),
                Box::new(move |res1| {
                    log_for_done1
                        .borrow_mut()
                        .push(format!("first-completed:{res1:?}"));
                }),
            )
            .expect("initial schedule_open succeeds");

        pump_thread_until(|| done2.get(), std::time::Duration::from_secs(4));

        let modal_after = host.is_modal_active();
        drop(host);
        unsafe {
            let _ = DestroyWindow(owner);
        }

        assert!(
            !modal_after,
            "no modal loop may remain active after replacement"
        );
        assert_eq!(
            *log.borrow(),
            vec![
                "first-observed".to_string(),
                "first-completed:Cancelled".to_string(),
                "second-observed".to_string(),
                "second-completed:Cancelled".to_string(),
            ]
        );
    }
}
