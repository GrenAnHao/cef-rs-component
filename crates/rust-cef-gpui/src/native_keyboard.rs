//! UI-thread Win32 keyboard ingress for explicitly host-owned GPUI views.
//!
//! A thread-local hook would have no safe way to find the particular host service;
//! the private window property identifies an attached route instead. This is not
//! a service locator: the host owns `NativeKeyboardServices`, the only hook and
//! its bounded route allocations, and dropping it removes the properties first.
//! The hook never queues, posts or replays input. When GPUI is already borrowed
//! (notably a Runtime pump inside an App update), it refuses the key and records
//! cleanup rather than aliasing App through an unsafe pointer. The host must
//! pump at a UI-thread boundary where `AsyncApp::update_window` can borrow App.

use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    marker::PhantomData,
    num::NonZeroUsize,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    rc::{Rc, Weak},
};

use gpui::{
    App, AppContext as _, AsyncApp, Entity, FocusHandle, KeyDownEvent, KeyUpEvent, Keystroke,
    Modifiers, PlatformInput, Subscription, WeakEntity, Window,
};
use raw_window_handle::RawWindowHandle;
use rust_cef_core::{
    contracts::SessionState,
    input::{InputKind, InputResetReason},
    ports::{BrowserCommand, BrowserHandle, EngineError, MouseModifiers},
};

use crate::{binding::WindowToken, input::InputLane, view::CefBrowser};

#[cfg(test)]
mod tests;
mod window_routes;

type Handle = *mut c_void;
const MAX_ROUTES: usize = 256;
const WH_GETMESSAGE: i32 = 3;
const PM_REMOVE: usize = 1;
const WM_NULL: u32 = 0;
const WM_QUIT: u32 = 0x0012;
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;
const VK_SHIFT: u32 = 0x10;
const VK_CONTROL: u32 = 0x11;
const VK_MENU: u32 = 0x12;
const VK_LWIN: u32 = 0x5b;
const VK_RWIN: u32 = 0x5c;
/// With an IME open the queued `WM_KEYDOWN` replaces the physical key with
/// this sentinel; the real key is only available through IMM, while the
/// matching `WM_KEYUP` still carries the real code. Indexing the ledger with
/// the raw sentinel therefore leaves every UP unmatched (`orphan_up`, leaked
/// pressed key, no text commit).
const VK_PROCESSKEY: usize = 0xe5;
const WAIT_OBJECT_0: u32 = 0;
const PROPERTY: &[u16] = &[
    0x47, 0x70, 0x75, 0x69, 0x43, 0x65, 0x66, 0x4b, 0x65, 0x79, 0x62, 0x6f, 0x61, 0x72, 0x64, 0x52,
    0x6f, 0x75, 0x74, 0x65, 0x31, 0,
];

#[repr(C)]
#[derive(Clone, Copy)]
struct Point {
    x: i32,
    y: i32,
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
    private: u32, // MSG.lPrivate, part of the Windows ABI.
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
    fn GetKeyState(key: i32) -> i16;
    fn GetFocus() -> Handle;
    fn GetForegroundWindow() -> Handle;
    fn GetWindowThreadProcessId(hwnd: Handle, pid: *mut u32) -> u32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
}
#[link(name = "imm32")]
unsafe extern "system" {
    fn ImmGetVirtualKey(hwnd: Handle) -> u32;
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
pub enum NativeKeyboardError {
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
pub struct NativeKeyboardDiagnostics {
    pub intercepted_down: u64,
    pub intercepted_up: u64,
    pub routed_down: u64,
    pub routed_up: u64,
    pub host_shortcuts: u64,
    pub refused: u64,
    pub cleanup_attempts: u64,
    pub cleanup_failures: u64,
    pub last_cleanup_error: Option<EngineError>,
    pub unclaimed: u64,
    pub system_text_skipped: u64,
    pub unrepresented_lock_state: u64,
    pub orphan_up: u64,
    pub panics: u64,
    pub last_failure: Option<NativeKeyboardFailure>,
    pub hook_live: bool,
    pub properties_live: usize,
    pub lease_live: bool,
    pub retired: bool,
    pub cancelled_keys: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeKeyboardFailure {
    BorrowedApp,
    WindowUnavailable,
    FocusLost,
    BindingRetired,
    BrowserUnavailable,
    InterceptorUnclaimed,
    KeyUpUnclaimed,
    Reentrant,
    NonceExhausted,
    InvalidVirtualKey,
    OrphanUp,
    Panic,
}

/// Explicit host shortcut handler. Return `true` only when the host itself
/// handled the key. A handled MSG is consumed without translation, so neither
/// GPUI's text handler nor Chromium receives an exempt shortcut. The callback
/// must not dispatch/repost this same MSG. Its `up` call is paired to a handled
/// down even when the modifiers changed between the two physical messages.
pub type HostShortcutPolicy = Rc<dyn Fn(u32, MouseModifiers, bool, &mut Window, &mut App) -> bool>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeDownAdmission {
    Admitted(u64),
    Deferred(u64),
    Refused,
}

#[derive(Clone, Copy)]
pub(crate) struct NativeKey {
    pub vk: u32,
    pub lparam: isize,
    pub system: bool,
    pub up: bool,
    pub modifiers: MouseModifiers,
    pub expected_epoch: Option<u64>,
}

impl NativeKey {
    pub(crate) fn kind(self) -> InputKind {
        InputKind::Key {
            windows_key_code: self.vk as i32,
            native_key_code: self.lparam as i32,
            up: self.up,
            repeat: !self.up && (self.lparam as u32 & (1 << 30)) != 0,
            system: self.system,
            modifiers: self.modifiers,
        }
    }
}

#[derive(Clone, Copy)]
struct Envelope {
    hwnd: Handle,
    route_id: u64,
    nonce: u64,
    key: NativeKey,
    claimed: bool,
    text_allowed: bool,
    epoch: Option<u64>,
}

pub(crate) struct ServiceState {
    thread: u32,
    active: Cell<bool>,
    current: Cell<Option<Envelope>>,
    dispatching: Cell<bool>,
    attempt_key: Cell<Option<u32>>,
    attempt_route: Cell<Option<u64>>,
    aborted: Cell<bool>,
    next_nonce: Cell<u64>,
    diagnostics: RefCell<NativeKeyboardDiagnostics>,
    policy: RefCell<Option<HostShortcutPolicy>>,
    routes: RefCell<Vec<Weak<Route>>>,
}

impl ServiceState {
    pub(crate) fn is_active(&self) -> bool {
        self.active.get()
    }

    // The callback resolves a strong Route and calls Route::fail in production.
    // No route-list borrow or packet borrow may survive into that cleanup.
    fn abort_dispatch(
        &self,
        reason: NativeKeyboardFailure,
        mut fail_route: impl FnMut(u64, NativeKeyboardFailure) -> bool,
    ) {
        let route_id = self
            .attempt_route
            .get()
            .or_else(|| self.current.get().map(|packet| packet.route_id));
        self.aborted.set(true);
        self.current.set(None);
        if !route_id.is_some_and(|id| fail_route(id, reason)) {
            self.failure(reason, None);
        }
    }

    fn recover_reentrant(
        &self,
        vk: u32,
        origins: impl IntoIterator<Item = u64>,
        consume: impl FnOnce(),
        mut fail_route: impl FnMut(u64, NativeKeyboardFailure) -> bool,
    ) {
        let origins: Vec<_> = origins.into_iter().collect();
        if !origins.is_empty() || self.attempt_key.get() == Some(vk) {
            consume();
        }
        // A nested UP can release B while A's DOWN has not reached its ledger.
        // Both independent routes must be cleaned, before the outer guard exits.
        for id in origins {
            let _ = fail_route(id, NativeKeyboardFailure::Reentrant);
        }
        self.abort_dispatch(NativeKeyboardFailure::Reentrant, fail_route);
    }

    fn run_dispatch(
        &self,
        vk: u32,
        process: impl FnOnce(),
        fail_route: impl FnMut(u64, NativeKeyboardFailure) -> bool,
    ) {
        self.dispatching.set(true);
        self.attempt_key.set(Some(vk));
        self.attempt_route.set(None);
        self.aborted.set(false);
        let _guard = DispatchGuard(self);
        if catch_unwind(AssertUnwindSafe(process)).is_err() {
            {
                let mut diagnostics = self.diagnostics.borrow_mut();
                diagnostics.panics = diagnostics.panics.saturating_add(1);
            }
            self.abort_dispatch(NativeKeyboardFailure::Panic, fail_route);
        }
    }

    fn failure(&self, failure: NativeKeyboardFailure, handle: Option<&BrowserHandle>) {
        {
            let mut diag = self.diagnostics.borrow_mut();
            diag.refused = diag.refused.saturating_add(1);
            diag.last_failure = Some(failure);
            if handle.is_some() {
                diag.cleanup_attempts = diag.cleanup_attempts.saturating_add(1);
            }
        }
        if let Some(handle) = handle {
            // Existing independent coalesced cleanup lane; never auto retry.
            if let Err(error) =
                handle.submit(BrowserCommand::ResetInput(InputResetReason::Rejected))
            {
                let mut diag = self.diagnostics.borrow_mut();
                diag.cleanup_failures = diag.cleanup_failures.saturating_add(1);
                diag.last_cleanup_error = Some(error);
            }
        }
    }
}

struct WindowRoutes {
    hwnd: Handle,
    window_handle: gpui::AnyWindowHandle,
    app: AsyncApp,
    service: Rc<ServiceState>,
}

struct Route {
    id: u64,
    hwnd: Handle,
    host_exceptions: Cell<[bool; 256]>,
    owned_keys: Cell<[bool; 256]>,
    owned_epochs: Cell<[u64; 256]>,

    window_id: u64,
    window_handle: gpui::AnyWindowHandle,
    app: AsyncApp,
    view: WeakEntity<CefBrowser>,
    focus: FocusHandle,
    token: WindowToken,
    browser: BrowserHandle,
    input: Rc<RefCell<InputLane>>,
    cef_focus_accepted: Rc<Cell<bool>>,
    service: Rc<ServiceState>,
}

/// Explicitly owned by the host, not by App globals or a thread-local slot.
/// Capacity is explicit and at most 256 views, including multiple views in
/// the same window. There is no input queue or cached focused-view selection.
/// Never move or drop on another thread.
pub struct NativeKeyboardServices {
    service: Rc<ServiceState>,
    routes: Vec<Rc<Route>>,
    windows: Vec<Rc<WindowRoutes>>,
    capacity: usize,
    hook: Handle,
    lease: Handle,
    lease_owned: bool,
    interceptor: Option<Subscription>,
    _ui_thread: PhantomData<Rc<()>>,
}

impl NativeKeyboardServices {
    pub fn new(capacity: NonZeroUsize, cx: &mut App) -> Result<Self, NativeKeyboardError> {
        if capacity.get() > MAX_ROUTES {
            return Err(NativeKeyboardError::RouteCapacity);
        }
        let thread = unsafe { GetCurrentThreadId() };
        let name: Vec<u16> = format!(
            "Local\\GpuiCefKeyboardV1-{}-{}",
            unsafe { GetCurrentProcessId() },
            thread
        )
        .encode_utf16()
        .chain(Some(0))
        .collect();
        let lease = unsafe { CreateMutexW(ptr::null_mut(), 0, name.as_ptr()) };
        if lease.is_null() {
            return Err(NativeKeyboardError::LeaseUnavailable);
        }
        let existed = unsafe { GetLastError() } == 183; // ERROR_ALREADY_EXISTS
        if existed || unsafe { WaitForSingleObject(lease, 0) } != WAIT_OBJECT_0 {
            unsafe {
                CloseHandle(lease);
            }
            return Err(NativeKeyboardError::LeaseUnavailable);
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
            return Err(NativeKeyboardError::HookUnavailable);
        }
        let service = Rc::new(ServiceState {
            thread,
            active: Cell::new(true),
            current: Cell::new(None),
            dispatching: Cell::new(false),
            attempt_key: Cell::new(None),
            attempt_route: Cell::new(None),
            aborted: Cell::new(false),
            next_nonce: Cell::new(1),
            diagnostics: RefCell::new(NativeKeyboardDiagnostics {
                hook_live: true,
                lease_live: true,
                ..NativeKeyboardDiagnostics::default()
            }),
            policy: RefCell::new(None),
            routes: RefCell::new(Vec::with_capacity(capacity.get())),
        });
        let intercept_state = service.clone();
        let interceptor = cx.intercept_keystrokes(move |event, window, app| {
            let Some(envelope) = intercept_state.current.get().filter(|envelope| {
                intercept_state.dispatching.get()
                    && matches_envelope(*envelope, &event.keystroke, false)
            }) else {
                // Browser ownership is explicit. Do not let SDK-derived
                // standalone modifiers or an old host replay enter its keymap.
                let routes: Vec<_> = intercept_state
                    .routes
                    .borrow()
                    .iter()
                    .filter_map(Weak::upgrade)
                    .collect();
                if intercept_state.active.get()
                    && routes.iter().any(|route| {
                        route.window_handle == window.window_handle()
                            && route.token.matches_window(route.window_id)
                            && route.focus.is_focused(window)
                    })
                {
                    app.stop_propagation();
                }
                return;
            };
            if let Some(route) = route_for_envelope(envelope) {
                if !Rc::ptr_eq(&route.service, &intercept_state)
                    || route.window_handle != window.window_handle()
                    || !route.token.matches_window(route.window_id)
                {
                    return;
                }
                // Before GPUI's multi-key shortcut cache: no replay can later
                // acquire the native packet. Ownership is confirmed *after*
                // the production view's actual InputLane admission, not merely
                // by observing propagation=false.
                if let Some(view) = route.view.upgrade() {
                    app.stop_propagation();
                    let admission =
                        view.update(app, |view, _| view.native_key_down(envelope.key, window));
                    let (epoch, text_allowed) = match admission {
                        NativeDownAdmission::Admitted(epoch) => (Some(epoch), true),
                        NativeDownAdmission::Deferred(epoch) => (Some(epoch), false),
                        NativeDownAdmission::Refused => (None, false),
                    };
                    if epoch.is_some() {
                        let _ = claim_nonce(
                            &intercept_state.current,
                            &event.keystroke,
                            false,
                            text_allowed,
                            epoch,
                        );
                    }
                }
            }
        });
        Ok(Self {
            service,
            routes: Vec::with_capacity(capacity.get()),
            windows: Vec::with_capacity(capacity.get()),
            capacity: capacity.get(),
            hook,
            lease,
            lease_owned: true,
            interceptor: Some(interceptor),
            _ui_thread: PhantomData,
        })
    }

    /// Explicit host callback for reserved shortcuts. The callback must handle
    /// the action itself; returning true consumes the original MSG. Without a
    /// callback all keys belong to the actually focused browser.
    pub fn set_host_shortcut_policy(&self, policy: Option<HostShortcutPolicy>) {
        *self.service.policy.borrow_mut() = policy;
    }

    pub fn diagnostics(&self) -> NativeKeyboardDiagnostics {
        self.service.diagnostics.borrow().clone()
    }

    pub fn attach(
        &mut self,
        view: &Entity<CefBrowser>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), NativeKeyboardError> {
        if unsafe { GetCurrentThreadId() } != self.service.thread {
            return Err(NativeKeyboardError::WrongThread);
        }
        if !self.service.active.get() {
            return Err(NativeKeyboardError::Retired);
        }
        if self.routes.len() == self.capacity {
            return Err(NativeKeyboardError::RouteCapacity);
        }
        let RawWindowHandle::Win32(raw) = raw_window_handle::HasWindowHandle::window_handle(window)
            .map_err(|_| NativeKeyboardError::InvalidWindow)?
            .as_raw()
        else {
            return Err(NativeKeyboardError::InvalidWindow);
        };
        let hwnd = raw.hwnd.get() as Handle;
        let mut pid = 0;
        if hwnd.is_null()
            || unsafe { GetWindowThreadProcessId(hwnd, &mut pid) } != self.service.thread
            || pid != unsafe { GetCurrentProcessId() }
        {
            return Err(NativeKeyboardError::InvalidWindow);
        }
        let (token, focus, browser, bound_window, input, cef_focus_accepted) =
            view.read(cx).native_keyboard_attachment();
        if bound_window != window.window_handle()
            || !token.matches_window(window.window_handle().window_id().as_u64())
        {
            return Err(NativeKeyboardError::BindingRetired);
        }
        if self
            .routes
            .iter()
            .any(|route| route.token.binding().browser == token.binding().browser)
        {
            return Err(NativeKeyboardError::DuplicateWindow);
        }
        let existing = self
            .windows
            .iter()
            .find(|group| group.hwnd == hwnd)
            .cloned();
        if let Some(group) = &existing {
            if unsafe { GetPropW(hwnd, PROPERTY.as_ptr()) } != Rc::as_ptr(group) as Handle {
                return Err(NativeKeyboardError::PropertyUnavailable);
            }
        } else if !unsafe { GetPropW(hwnd, PROPERTY.as_ptr()) }.is_null() {
            return Err(NativeKeyboardError::DuplicateWindow);
        }
        let route = Rc::new(Route {
            id: self.routes.len() as u64 + 1,
            hwnd,
            host_exceptions: Cell::new([false; 256]),
            owned_keys: Cell::new([false; 256]),
            owned_epochs: Cell::new([0; 256]),
            window_id: window.window_handle().window_id().as_u64(),
            window_handle: window.window_handle(),
            app: cx.to_async(),
            view: view.downgrade(),
            focus,
            token,
            browser,
            input,
            cef_focus_accepted,
            service: self.service.clone(),
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
                return Err(NativeKeyboardError::PropertyUnavailable);
            }
            // Keep the property owner alive even if a host callback below panics.
            self.windows.push(group);
            self.service.diagnostics.borrow_mut().properties_live += 1;
            if unsafe { GetPropW(hwnd, PROPERTY.as_ptr()) } != pointer {
                return Err(NativeKeyboardError::PropertyUnavailable);
            }
        }
        self.service.routes.borrow_mut().push(Rc::downgrade(&route));
        self.routes.push(route);
        view.update(cx, |view, _| view.native_keyboard_attached(&self.service));
        Ok(())
    }

    /// Explicit, observable retirement. Call outside a native dispatch, after
    /// removing the views. Failure retains ownership; it is never fake success.
    pub fn retire(&mut self) -> Result<NativeKeyboardDiagnostics, NativeKeyboardError> {
        if unsafe { GetCurrentThreadId() } != self.service.thread {
            return Err(NativeKeyboardError::WrongThread);
        }
        if self.service.dispatching.get() {
            return Err(NativeKeyboardError::Busy);
        }
        self.service.active.set(false);
        for route in &self.routes {
            route.cancel(InputResetReason::FocusLost);
        }
        for group in &self.windows {
            let pointer = Rc::as_ptr(group) as Handle;
            if unsafe { GetPropW(group.hwnd, PROPERTY.as_ptr()) } == pointer {
                unsafe {
                    RemovePropW(group.hwnd, PROPERTY.as_ptr());
                }
                if unsafe { GetPropW(group.hwnd, PROPERTY.as_ptr()) } == pointer {
                    return Err(NativeKeyboardError::RetirementUnconfirmed);
                }
            }
        }
        self.service.diagnostics.borrow_mut().properties_live = 0;
        if !self.hook.is_null() {
            if unsafe { UnhookWindowsHookEx(self.hook) } == 0 {
                return Err(NativeKeyboardError::RetirementUnconfirmed);
            }
            self.hook = ptr::null_mut();
            self.service.diagnostics.borrow_mut().hook_live = false;
        }
        if !self.lease.is_null() {
            if self.lease_owned {
                if unsafe { ReleaseMutex(self.lease) } == 0 {
                    return Err(NativeKeyboardError::RetirementUnconfirmed);
                }
                self.lease_owned = false;
            }
            if unsafe { CloseHandle(self.lease) } == 0 {
                return Err(NativeKeyboardError::RetirementUnconfirmed);
            }
            self.lease = ptr::null_mut();
            self.service.diagnostics.borrow_mut().lease_live = false;
        }
        self.interceptor.take();
        self.routes.clear();
        self.windows.clear();
        self.service.routes.borrow_mut().clear();
        self.service.diagnostics.borrow_mut().retired = true;
        Ok(self.diagnostics())
    }
}

impl Drop for NativeKeyboardServices {
    fn drop(&mut self) {
        if self.retire().is_err() {
            self.service.active.set(false);
            self.interceptor.take();
            // Unconfirmed properties/hooks must not outlive their Rc targets.
            // Preserve the lease as well, preventing a second producer. This
            // fail-closed leak is not reported as a successful retirement.
            for group in self.windows.drain(..) {
                let _ = Rc::into_raw(group);
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
    // A service-owned property is removed before its strong group can be freed.
    unsafe {
        Rc::increment_strong_count(raw);
        Some(Rc::from_raw(raw))
    }
}

fn route_for_envelope(envelope: Envelope) -> Option<Rc<Route>> {
    let group = window_from_property(envelope.hwnd)?;
    let route = group
        .service
        .routes
        .borrow()
        .iter()
        .filter_map(Weak::upgrade)
        .find(|route| route.id == envelope.route_id && route.hwnd == envelope.hwnd);
    route
}

unsafe extern "system" fn get_message_hook(code: i32, wparam: usize, lparam: isize) -> isize {
    // CallNext first: later hooks may replace the MSG; do not consume a
    // PM_NOREMOVE peek or a WM_QUIT. No panic may cross this FFI boundary.
    let next = unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) };
    if code != 0 || wparam != PM_REMOVE || lparam == 0 {
        return next;
    }
    let msg = lparam as *mut Msg;
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let original = unsafe { ptr::read(msg) };
        if !selected_keyboard(code, wparam, original.message) || original.hwnd.is_null() {
            return;
        }
        let (up, system) = match original.message {
            WM_KEYDOWN => (false, false),
            WM_SYSKEYDOWN => (false, true),
            WM_KEYUP => (true, false),
            WM_SYSKEYUP => (true, true),
            _ => return,
        };
        let Some(group) = window_from_property(original.hwnd) else {
            return;
        };
        if unsafe { GetCurrentThreadId() } != group.service.thread || group.hwnd != original.hwnd {
            return;
        }
        let vk = message_virtual_key(original.wparam, || unsafe {
            ImmGetVirtualKey(original.hwnd)
        });
        if vk > 255 {
            group
                .service
                .failure(NativeKeyboardFailure::InvalidVirtualKey, None);
            return;
        }
        let key = NativeKey {
            vk: vk as u32,
            lparam: original.lparam,
            system,
            up,
            modifiers: current_modifiers(),
            expected_epoch: None,
        };
        if catch_unwind(AssertUnwindSafe(|| group.process(msg, original, key))).is_err() {
            {
                let mut diagnostics = group.service.diagnostics.borrow_mut();
                diagnostics.panics = diagnostics.panics.saturating_add(1);
            }
            group.service.failure(NativeKeyboardFailure::Panic, None);
        }
    }));
    if outcome.is_err() {
        // We cannot safely inspect a potentially corrupted property or take a
        // route borrow after unwind; the MSG remains the caller's responsibility.
        // `Route::process` guards its own mutable state with RAII.
    }
    next
}

fn neutralize(msg: *mut Msg) {
    // The original MSG has already been copied; preserve its time, point,
    // private metadata and the thread's GetMessageExtraInfo untouched.
    unsafe {
        (*msg).message = WM_NULL;
        (*msg).wparam = 0;
        (*msg).lparam = 0;
    }
}

/// Resolve the physical virtual key of a queued message. `VK_PROCESSKEY` is a
/// sentinel, never a key: it is replaced by the owner IME's real key code so a
/// DOWN and its UP index the same ledger slot. An unresolvable answer keeps the
/// sentinel explicit instead of pairing keys by guesswork.
fn message_virtual_key(wparam: usize, owner: impl FnOnce() -> u32) -> usize {
    if wparam != VK_PROCESSKEY {
        return wparam;
    }
    let resolved = owner();
    if (1..=255).contains(&resolved) {
        resolved as usize
    } else {
        wparam
    }
}

fn selected_keyboard(code: i32, removed: usize, msg: u32) -> bool {
    code == 0
        && removed == PM_REMOVE
        && msg != WM_QUIT
        && matches!(msg, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP)
}

fn matches_envelope(envelope: Envelope, stroke: &Keystroke, up: bool) -> bool {
    !envelope.claimed
        && envelope.key.up == up
        && stroke.key == native_name(envelope.nonce)
        && stroke.modifiers == gpui_modifiers(envelope.key.modifiers)
}

fn claim_nonce(
    slot: &Cell<Option<Envelope>>,
    stroke: &Keystroke,
    up: bool,
    text_allowed: bool,
    epoch: Option<u64>,
) -> Option<NativeKey> {
    let mut envelope = slot.get()?;
    if !matches_envelope(envelope, stroke, up) {
        return None;
    }
    envelope.claimed = true;
    envelope.text_allowed = text_allowed;
    envelope.epoch = epoch;
    slot.set(Some(envelope));
    Some(envelope.key)
}

fn current_modifiers() -> MouseModifiers {
    let pressed = |vk| unsafe { GetKeyState(vk) } < 0;
    MouseModifiers {
        shift: pressed(VK_SHIFT as i32),
        control: pressed(VK_CONTROL as i32),
        alt: pressed(VK_MENU as i32),
        platform: pressed(VK_LWIN as i32) || pressed(VK_RWIN as i32),
    }
}

fn gpui_modifiers(modifiers: MouseModifiers) -> Modifiers {
    Modifiers {
        shift: modifiers.shift,
        control: modifiers.control,
        alt: modifiers.alt,
        platform: modifiers.platform,
        ..Modifiers::default()
    }
}

fn native_name(nonce: u64) -> String {
    format!("gpui-cef-native-{nonce:016x}")
}

fn keystroke(key: NativeKey, nonce: u64) -> Keystroke {
    // Never infer VK from this GPUI key. It is an opaque one-dispatch proof,
    // not an ASCII approximation of the physical layout or a replay token.
    Keystroke {
        key: native_name(nonce),
        key_char: None,
        modifiers: gpui_modifiers(key.modifiers),
    }
}

struct DispatchGuard<'a>(&'a ServiceState);
impl Drop for DispatchGuard<'_> {
    fn drop(&mut self) {
        self.0.current.set(None);
        self.0.attempt_key.set(None);
        self.0.attempt_route.set(None);
        self.0.aborted.set(false);
        self.0.dispatching.set(false);
    }
}

impl Route {
    fn cancel_owned_up(&self, msg: *mut Msg, key: NativeKey, index: usize) {
        if key.up && (self.owned_keys.get()[index] || self.host_exceptions.get()[index]) {
            neutralize(msg);
        }
    }

    fn forget_key(&self, index: usize) {
        let mut keys = self.owned_keys.get();
        let mut epochs = self.owned_epochs.get();
        keys[index] = false;
        epochs[index] = 0;
        self.owned_keys.set(keys);
        self.owned_epochs.set(epochs);
        self.cef_focus_accepted.set(false);
        let mut diagnostics = self.service.diagnostics.borrow_mut();
        diagnostics.cancelled_keys = diagnostics.cancelled_keys.saturating_add(1);
    }

    fn cancel(&self, reason: InputResetReason) {
        let count = self.owned_keys.get().iter().filter(|held| **held).count() as u64;
        self.owned_keys.set([false; 256]);
        self.owned_epochs.set([0; 256]);
        self.cef_focus_accepted.set(false);
        if count == 0 {
            return;
        }
        {
            let mut diagnostics = self.service.diagnostics.borrow_mut();
            diagnostics.cancelled_keys = diagnostics.cancelled_keys.saturating_add(count);
        }
        if let Ok(mut lane) = self.input.try_borrow_mut() {
            lane.abandon(&self.browser, reason);
        } else {
            self.service
                .failure(NativeKeyboardFailure::Reentrant, Some(&self.browser));
        }
    }

    fn fail(&self, reason: NativeKeyboardFailure) {
        self.owned_epochs.set([0; 256]);
        self.host_exceptions.set([false; 256]);
        self.owned_keys.set([false; 256]);
        self.cef_focus_accepted.set(false);
        if let Ok(mut lane) = self.input.try_borrow_mut() {
            {
                let mut diag = self.service.diagnostics.borrow_mut();
                diag.refused = diag.refused.saturating_add(1);
                diag.last_failure = Some(reason);
                diag.cleanup_attempts = diag.cleanup_attempts.saturating_add(1);
            }
            lane.abandon(&self.browser, InputResetReason::Rejected);
        } else {
            self.service.failure(reason, Some(&self.browser));
        }
    }

    fn process(&self, msg: *mut Msg, original: Msg, key: NativeKey) {
        let state = &self.service;
        if !state.active.get() || state.aborted.get() {
            return;
        }
        let Some(index) = usize::try_from(key.vk).ok().filter(|index| *index < 256) else {
            self.fail(NativeKeyboardFailure::InvalidVirtualKey);
            return;
        };
        if key.up && !self.owned_keys.get()[index] {
            neutralize(msg);
            return;
        }
        let live = self.token.matches_window(self.window_id)
            && self.browser.snapshot().state == SessionState::Alive
            && self
                .browser
                .input_snapshot()
                .is_ok_and(|s| !s.cleanup_pending && !s.cleanup_failed);
        if !live {
            self.cancel_owned_up(msg, key, index);
            self.fail(if self.token.is_live() {
                NativeKeyboardFailure::BrowserUnavailable
            } else {
                NativeKeyboardFailure::BindingRetired
            });
            return;
        }
        if self.view.upgrade().is_none()
            || unsafe { GetForegroundWindow() } != self.hwnd
            || unsafe { GetFocus() } != self.hwnd
        {
            self.cancel_owned_up(msg, key, index);
            self.fail(NativeKeyboardFailure::FocusLost);
            return;
        }
        let mut app = self.app.clone();
        // This is a *preflight*, not a borrowed GPUI reference retained across
        // the dispatch. A borrowed App or window is an observable, fail-closed
        // rejection rather than a second raw-message queue.
        let eligible = app.update_window(self.window_handle, |_, window, _| {
            self.token
                .matches_window(window.window_handle().window_id().as_u64())
                && window.is_window_active()
                && self.focus.is_focused(window)
        });
        if state.aborted.get() {
            return;
        }
        match eligible {
            Ok(true) => {}
            Ok(false) => {
                self.cancel_owned_up(msg, key, index);
                self.fail(NativeKeyboardFailure::FocusLost);
                return;
            }
            Err(_) => {
                self.cancel_owned_up(msg, key, index);
                self.fail(NativeKeyboardFailure::BorrowedApp);
                return;
            }
        }
        // The host exception is a *real* callback in a legal GPUI window/App
        // update, not a bool that silently swallows the shortcut. No borrow of
        // the policy RefCell survives its callback or the App update.
        let paired_up = key.up && self.host_exceptions.get()[index];
        let policy = state.policy.borrow().clone();
        if paired_up || (!key.up && !self.owned_keys.get()[index] && policy.is_some()) {
            let outcome = app.update_window(self.window_handle, |_, window, cx| {
                let handled = policy
                    .as_ref()
                    .is_some_and(|callback| callback(key.vk, key.modifiers, key.up, window, cx));
                (
                    handled,
                    self.token.matches_window(self.window_id)
                        && window.is_window_active()
                        && self.focus.is_focused(window),
                )
            });
            if state.aborted.get() {
                return;
            }
            let (handled, still_focused) = match outcome {
                Ok(pair) => pair,
                Err(_) => {
                    self.cancel_owned_up(msg, key, index);
                    self.fail(NativeKeyboardFailure::BorrowedApp);
                    return;
                }
            };
            if paired_up {
                let mut exceptions = self.host_exceptions.get();
                exceptions[index] = false;
                self.host_exceptions.set(exceptions);
            }
            if handled || paired_up {
                if !key.up {
                    let mut exceptions = self.host_exceptions.get();
                    exceptions[index] = true;
                    self.host_exceptions.set(exceptions);
                }
                neutralize(msg);
                let mut diagnostics = state.diagnostics.borrow_mut();
                diagnostics.host_shortcuts = diagnostics.host_shortcuts.saturating_add(1);
                return;
            }
            if !still_focused {
                self.cancel_owned_up(msg, key, index);
                self.fail(NativeKeyboardFailure::FocusLost);
                return;
            }
        }
        let nonce = state.next_nonce.get();
        let Some(next) = nonce.checked_add(1) else {
            self.fail(NativeKeyboardFailure::NonceExhausted);
            return;
        };
        state.next_nonce.set(next);
        if !state.active.get() || !self.token.matches_window(self.window_id) {
            self.cancel_owned_up(msg, key, index);
            self.fail(NativeKeyboardFailure::BindingRetired);
            return;
        }
        let stroke = keystroke(key, nonce);
        state.current.set(Some(Envelope {
            hwnd: self.hwnd,
            route_id: self.id,
            nonce,
            key,
            claimed: false,
            text_allowed: false,
            epoch: None,
        }));
        // From here we own the removed MSG. Never let the outer main loop (or
        // CEF nested pump) translate or dispatch the same key a second time.
        // Commit before entering GPUI: a nested Windows loop must never see
        // this original key twice. The original value remains intact here for
        // optional translation *after* confirmed browser ownership.
        neutralize(msg);
        let dispatched = app.update_window(self.window_handle, |_, window, cx| {
            if key.up {
                window.dispatch_event(PlatformInput::KeyUp(KeyUpEvent { keystroke: stroke }), cx);
            } else {
                window.dispatch_event(
                    PlatformInput::KeyDown(KeyDownEvent {
                        keystroke: stroke,
                        is_held: key.lparam as u32 & (1 << 30) != 0,
                        prefer_character_input: false,
                    }),
                    cx,
                );
            }
        });
        if state.aborted.get() {
            return;
        }
        let result = state.current.get();
        let claimed = result.is_some_and(|envelope| envelope.claimed);
        if claimed {
            let mut owned = self.owned_keys.get();
            owned[index] = !key.up;
            self.owned_keys.set(owned);
            let mut epochs = self.owned_epochs.get();
            epochs[index] = if key.up {
                0
            } else {
                result.and_then(|e| e.epoch).unwrap_or(0)
            };
            self.owned_epochs.set(epochs);
            if unsafe { GetKeyState(0x14) & 1 != 0 || GetKeyState(0x90) & 1 != 0 } {
                let mut diagnostics = state.diagnostics.borrow_mut();
                diagnostics.unrepresented_lock_state =
                    diagnostics.unrepresented_lock_state.saturating_add(1);
            }
        }
        let text_epoch_matches = result.is_some_and(|envelope| {
            self.browser.input_snapshot().is_ok_and(|snapshot| {
                Some(snapshot.epoch) == envelope.epoch
                    && !snapshot.cleanup_pending
                    && !snapshot.cleanup_failed
            })
        });
        let same_window = window_from_property(self.hwnd).is_some_and(|group| {
            group.window_handle == self.window_handle && Rc::ptr_eq(&group.service, state)
        });
        if result.is_some_and(|envelope| envelope.text_allowed)
            && !key.up
            && text_epoch_matches
            && state.active.get()
            && self.token.matches_window(self.window_id)
            && same_window
        {
            if key.system {
                // SYSKEY/Alt text semantics are not guessed here. WM_SYSCHAR
                // cannot be fabricated from a raw keystroke; record the limit.
                let mut diagnostics = state.diagnostics.borrow_mut();
                diagnostics.system_text_skipped = diagnostics.system_text_skipped.saturating_add(1);
            } else {
                // Only a browser-owned down produces WM_CHAR. No GPUI keydown
                // replay can retranslate it, and original MSG metadata is kept.
                unsafe {
                    TranslateMessage(&original);
                }
            }
        }
        let mut d = state.diagnostics.borrow_mut();
        if key.up {
            d.intercepted_up = d.intercepted_up.saturating_add(1);
        } else {
            d.intercepted_down = d.intercepted_down.saturating_add(1);
        }
        if dispatched.is_err() || !claimed {
            d.unclaimed = d.unclaimed.saturating_add(1);
            drop(d);
            self.fail(if key.up {
                NativeKeyboardFailure::KeyUpUnclaimed
            } else {
                NativeKeyboardFailure::InterceptorUnclaimed
            });
        } else if key.up {
            d.routed_up = d.routed_up.saturating_add(1);
        } else {
            d.routed_down = d.routed_down.saturating_add(1);
        }
    }
}

/// Called only by the production GPUI `on_key_up` listener. No matching by
/// value against an ambient native scope, and no fabricated origin for replay.
pub(crate) fn claim_up(
    state: &ServiceState,
    browser: rust_cef_core::contracts::BrowserId,
    window: &Window,
    event: &KeyUpEvent,
) -> Option<NativeKey> {
    let envelope = state.current.get()?;
    if !state.active.get()
        || !state.dispatching.get()
        || !matches_envelope(envelope, &event.keystroke, true)
    {
        return None;
    }
    let route = route_for_envelope(envelope)?;
    if !ptr::eq(Rc::as_ptr(&route.service), state as *const ServiceState)
        || route.window_handle != window.window_handle()
        || route.token.binding().browser != browser
        || !route.focus.is_focused(window)
        || !route.token.matches_window(route.window_id)
    {
        return None;
    }
    claim_nonce(&state.current, &event.keystroke, true, false, None)
}
