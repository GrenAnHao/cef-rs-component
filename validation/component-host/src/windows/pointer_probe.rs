//! Win32 pointer observation for the real GPUI host window.
//!
//! This is a test-host observation, not CEF input and not a library API. It
//! never calls CEF, GPUI or `SendInput`; the external Python driver performs
//! the real `SendInput`, and this module only subclasses the real GPUI HWND to
//! observe the resulting `WM_LBUTTONDOWN`/`WM_LBUTTONUP` messages. It also does
//! not synthesize `WM_*` messages and does not pump the message queue.
//!
//! What is observed here is actual window-message delivery. That does not prove
//! CEF executed the click; the parent closes that loop separately with GPUI
//! component click counters, a real DOM `onchange` title and popup state.
//!
//! Scope and safety rules:
//!
//! * Windows x64 only, `std` plus exact `extern "system"` declarations.
//! * The GPUI `GWLP_USERDATA` slot is never touched. Probe state lives behind
//!   the probe's own `SetPropW` property, a single `Box<HookState>` pointer.
//!   There is no global map or service locator.
//! * This validation host must be the sole WNDPROC mutator on its GPUI owner
//!   thread while a probe lives. Get/SetWindowLongPtrW has no compare-and-swap;
//!   unexpected writers cause failure and best-effort rollback, not atomic
//!   isolation or a guarantee of no transient effect on another chain.
//! * The original WNDPROC is read first, the subclass is installed second and
//!   the returned previous value is required to match. Every non-observed
//!   message is forwarded unchanged through `CallWindowProcW`.
//! * Message records only happen under short `RefCell` borrows. The borrow is
//!   released before `CallWindowProcW`, so GPUI's own WNDPROC never runs while
//!   a mutable borrow is held.
//! * No panic may cross the FFI boundary. `try_borrow` and explicit branch
//!   checks are used in the WNDPROC; a borrow failure is a sticky observation
//!   error, and the original procedure is still called.
//! * The fixed marker is the full `ULONG_PTR` value `0x4346_4530` (positive
//!   32-bit). Acceptance compares all 64 bits except the system-owned injected
//!   bit 0 (the runtime fixture's real-delivery rule), so the previously failed
//!   wide marker cannot be accepted again. There is no low-word fallback, and
//!   `GetCurrentInputMessageSource` is diagnostic only. An accepted message
//!   keeps the complete received value in `PointerMessage::extra_info`; a
//!   rejection reports only high-32/low-32/zero classification, never a raw
//!   value that could be a foreign pointer.
//! * Ordinary `WM_MOUSEMOVE` is deliberately not judged. Synthetic hover or
//!   system cursor moves must not poison a click observation.
//! * Other mouse buttons, left double-clicks and keyboard messages are sticky
//!   rejections. Left messages that are unarmed, carry a foreign marker, arrive
//!   out of order, carry unexpected coordinates or exceed the fixed 64-record
//!   bound are sticky rejections too.
//! * `WM_NCDESTROY` invalidates the local state, removes the state property and
//!   then chains the destroy to the original procedure. No later frame can
//!   reach a box the owner frees: a stored fallback original, not the state
//!   pointer, is what such a frame would forward through. RAII never restores
//!   the original procedure on a wrong thread, a foreign window or a reused
//!   HWND. The normal path is for the parent to call `retire` before GPUI
//!   removes the window; a destroyed window only clears the local owner and
//!   never touches the old HWND.
//!
//! `PointerProbe` is `!Send`/`!Sync`. Use, snapshot, disarm and retire are all
//! owner-thread operations and return `Err` outside that thread instead of
//! panicking or touching a foreign handle.
//!
//! The records are FIFO and bounded: `Vec::with_capacity` is reserved at
//! install, so the WNDPROC itself never allocates.

#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::{
    cell::{Cell, RefCell},
    marker::PhantomData,
    mem::size_of,
    rc::Rc,
};

/// Positive 32-bit transport marker stored in the full `ULONG_PTR` extra-info
/// field by the external Python `SendInput` driver. Acceptance compares the
/// full width except the system-owned injected bit 0, so polluted high 32 bits
/// are always rejected while real delivery still matches.
const INPUT_MARKER: usize = 0x4346_4530;

/// Fixed FIFO bound for recorded left-button messages.
const MAX_MESSAGES: usize = 64;

/// Stack capacity used while matching the window title. Exact titles longer
/// than this are rejected at install instead of being truncated.
const TITLE_CAP: usize = 1024;

const WM_NCDESTROY: u32 = 0x0082;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_LBUTTONDBLCLK: u32 = 0x0203;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_RBUTTONDBLCLK: u32 = 0x0206;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_MBUTTONDBLCLK: u32 = 0x0209;
const WM_XBUTTONDOWN: u32 = 0x020B;
const WM_XBUTTONUP: u32 = 0x020C;
const WM_XBUTTONDBLCLK: u32 = 0x020D;

const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_CHAR: u32 = 0x0102;
const WM_DEADCHAR: u32 = 0x0103;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;
const WM_SYSCHAR: u32 = 0x0106;
const WM_SYSDEADCHAR: u32 = 0x0107;
const WM_UNICHAR: u32 = 0x0109;

const GWLP_WNDPROC: i32 = -4;

/// Exact Win32 declarations used by this probe. Nothing from the `windows`
/// crate, CEF or GPUI is linked or imported here.
#[rustfmt::skip]
mod raw {
    #![allow(
        dead_code,
        non_camel_case_types,
        non_snake_case,
        clippy::upper_case_acronyms
    )]

    pub type HWND = isize;
    pub type LRESULT = isize;
    pub type LPARAM = isize;
    pub type WPARAM = usize;
    /// Exact Win32 window-procedure type. The stored copy is a `LONG_PTR`, as
    /// returned by `GetWindowLongPtrW(GWLP_WNDPROC)`; on x64 both are 8 bytes.
    pub type WndProc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct POINT {
        pub x: i32,
        pub y: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct RECT {
        pub left: i32,
        pub top: i32,
        pub right: i32,
        pub bottom: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct INPUT_MESSAGE_SOURCE {
        pub deviceType: u32,
        pub originId: u32,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        pub fn GetWindowLongPtrW(hwnd: HWND, index: i32) -> isize;
        pub fn SetWindowLongPtrW(hwnd: HWND, index: i32, value: isize) -> isize;
        pub fn SetPropW(hwnd: HWND, name: *const u16, data: isize) -> i32;
        pub fn GetPropW(hwnd: HWND, name: *const u16) -> isize;
        pub fn RemovePropW(hwnd: HWND, name: *const u16) -> isize;
        pub fn CallWindowProcW(
            previous: WndProc,
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT;
        pub fn DefWindowProcW(
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT;
        pub fn EnumWindows(
            callback: Option<unsafe extern "system" fn(HWND, LPARAM) -> i32>,
            lparam: LPARAM,
        ) -> i32;
        pub fn IsWindow(hwnd: HWND) -> i32;
        pub fn IsWindowVisible(hwnd: HWND) -> i32;
        pub fn GetWindowThreadProcessId(hwnd: HWND, process: *mut u32) -> u32;
        pub fn GetWindowTextLengthW(hwnd: HWND) -> i32;
        pub fn GetWindowTextW(hwnd: HWND, text: *mut u16, max_count: i32) -> i32;
        pub fn GetClientRect(hwnd: HWND, rect: *mut RECT) -> i32;
        pub fn ClientToScreen(hwnd: HWND, point: *mut POINT) -> i32;
        pub fn GetDpiForWindow(hwnd: HWND) -> u32;
        pub fn GetForegroundWindow() -> HWND;
        pub fn SetForegroundWindow(hwnd: HWND) -> i32;
        pub fn GetMessageExtraInfo() -> isize;
        pub fn GetCurrentInputMessageSource(source: *mut INPUT_MESSAGE_SOURCE) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GetCurrentThreadId() -> u32;
        pub fn GetCurrentProcessId() -> u32;
        pub fn GetLastError() -> u32;
    }
}

use raw::*;

const _: () = assert!(INPUT_MARKER == 0x4346_4530);
const _: () = assert!(INPUT_MARKER <= i32::MAX as usize);
// x64 ABI sizes of the exact declarations used here.
const _: () = assert!(size_of::<POINT>() == 8);
const _: () = assert!(size_of::<RECT>() == 16);
const _: () = assert!(size_of::<INPUT_MESSAGE_SOURCE>() == 8);
// GWLP_WNDPROC stores a LONG_PTR; a Win32 procedure pointer is the same width.
const _: () = assert!(size_of::<WndProc>() == size_of::<isize>());

/// Compile-time UTF-8 bytes to a NUL-terminated UTF-16 property name.
const fn ascii_wide<const N: usize>(bytes: &[u8; N]) -> [u16; N] {
    let mut out = [0u16; N];
    let mut index = 0;
    while index < N {
        out[index] = bytes[index] as u16;
        index += 1;
    }
    out
}

// Window-property names are unique to this probe and stay short.
static PROP_NAME: [u16; 16] = ascii_wide(b"GPUI_CEF_PtrPrb\0");
/// Fallback copy of the original WNDPROC, independent of the state box. It lets
/// an orphaned subclass (its state property gone) still forward, never swallow,
/// a GPUI message.
static PROP_ORIGINAL: [u16; 16] = ascii_wide(b"GPUI_CEF_PtrOrg\0");

fn prop_name() -> *const u16 {
    PROP_NAME.as_ptr()
}

fn prop_original() -> *const u16 {
    PROP_ORIGINAL.as_ptr()
}

/// Forward one message to the exact pre-existing GPUI procedure. The value came
/// from `GetWindowLongPtrW(GWLP_WNDPROC)` and was confirmed before subclassing;
/// `LONG_PTR` and a Win32 procedure pointer are both 8 bytes on x64.
fn call_original(
    original: isize,
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if original == 0 {
        // Never call a null procedure.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    // SAFETY: see the type comment above; the bit value is the procedure the
    // window itself returned before this probe installed its subclass.
    let previous: WndProc = unsafe { std::mem::transmute::<isize, WndProc>(original) };
    unsafe { CallWindowProcW(previous, hwnd, message, wparam, lparam) }
}

fn subclass_address() -> isize {
    // Explicit fn-pointer coercion: this is the address installed into
    // GWLP_WNDPROC and compared against after installation and restoration.
    subclass_proc as WndProc as isize
}

fn thread_id() -> u32 {
    // SAFETY: no arguments; reads only the calling thread's id.
    unsafe { GetCurrentThreadId() }
}

fn failed(what: &str) -> String {
    // SAFETY: no arguments; reads the calling thread's last-error value.
    format!("{what} (error {})", unsafe { GetLastError() })
}

#[derive(Clone, Copy, Debug)]
struct SourceDiag {
    available: bool,
    device_type: u32,
    origin_id: u32,
}

/// Diagnostic only. It is recorded with a marker rejection but never used as a
/// substitute for the full-width marker comparison.
fn input_source() -> SourceDiag {
    let mut source = INPUT_MESSAGE_SOURCE::default();
    // SAFETY: `source` is a valid writable out-parameter.
    let available = unsafe { GetCurrentInputMessageSource(&mut source) } != 0;
    SourceDiag {
        available,
        device_type: source.deviceType,
        origin_id: source.originId,
    }
}

/// The public observation records; fields are part of the fixed handoff.
#[derive(Clone, Debug)]
pub(super) struct WindowInfo {
    pub(super) hwnd: u64,
    pub(super) pid: u32,
    pub(super) dpi: u32,
    pub(super) left: i32,
    pub(super) top: i32,
    pub(super) width: u32,
    pub(super) height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PointerMessage {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) up: bool,
    pub(super) extra_info: u64,
}

#[derive(Clone, Debug)]
pub(super) struct PointerSnapshot {
    pub(super) down: u64,
    pub(super) up: u64,
    pub(super) messages: Vec<PointerMessage>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArmPhase {
    /// Waiting for the matching left-button down.
    Down,
    /// Down was accepted; waiting for the matching left-button up.
    Up,
    /// Both messages were accepted; `disarm` may now succeed.
    Complete,
}

#[derive(Clone, Copy, Debug)]
struct Arm {
    x: i32,
    y: i32,
    phase: ArmPhase,
}

#[derive(Clone, Copy, Debug)]
enum Sticky {
    Overflow,
    ForeignButton {
        message: u32,
    },
    Keyboard {
        message: u32,
    },
    Unattributed {
        up: bool,
    },
    Unexpected {
        up: bool,
    },
    OutOfOrder {
        up: bool,
    },
    Coordinate {
        expected_x: i32,
        expected_y: i32,
        actual_x: i32,
        actual_y: i32,
        up: bool,
    },
    Marker {
        /// Diagnostic only: some bit above the low 32 was set.
        high32_set: bool,
        /// Low-32 diagnostic only; never an acceptance fallback.
        low32_match: bool,
        /// Diagnostic only: the received extra-info was exactly zero.
        zero: bool,
        source: SourceDiag,
    },
}

impl Sticky {
    fn describe(self) -> String {
        match self {
            Sticky::Overflow => {
                format!("pointer observation exceeded its fixed bound of {MAX_MESSAGES} messages")
            }
            Sticky::ForeignButton { message } => {
                format!("unexpected non-left mouse button message 0x{message:04x}")
            }
            Sticky::Keyboard { message } => {
                format!("unexpected keyboard message 0x{message:04x}")
            }
            Sticky::Unattributed { up } => {
                let kind = if up { "up" } else { "down" };
                format!("left-button {kind} arrived without an armed expectation")
            }
            Sticky::Unexpected { up } => {
                let kind = if up { "up" } else { "down" };
                format!("left-button {kind} arrived outside the single armed down/up pair")
            }
            Sticky::OutOfOrder { up } => {
                let kind = if up { "up" } else { "down" };
                format!("left-button {kind} arrived out of the required down-then-up order")
            }
            Sticky::Coordinate {
                expected_x,
                expected_y,
                actual_x,
                actual_y,
                up,
            } => {
                let kind = if up { "up" } else { "down" };
                format!(
                    "left-button {kind} at client ({actual_x},{actual_y}) did not match armed client ({expected_x},{expected_y})"
                )
            }
            Sticky::Marker {
                high32_set,
                low32_match,
                zero,
                source,
            } => {
                // The raw received value is not echoed: a foreign injector's
                // extra-info may be a raw pointer. Only classified bits are
                // reported, and none of these classifications admits input.
                let diagnostic = if source.available {
                    format!(
                        "GetCurrentInputMessageSource deviceType={} originId={} (diagnostic only)",
                        source.device_type, source.origin_id
                    )
                } else {
                    "GetCurrentInputMessageSource unavailable (diagnostic only)".to_owned()
                };
                format!(
                    "left message extra_info did not match the full-width INPUT marker; high32_set={high32_set} low32_marker_match={low32_match} extra_info_zero={zero}; {diagnostic}"
                )
            }
        }
    }
}

struct ProbeData {
    sticky: Option<Sticky>,
    messages: Vec<PointerMessage>,
    down: u64,
    up: u64,
    armed: Option<Arm>,
}

impl ProbeData {
    fn new() -> Self {
        Self {
            sticky: None,
            // Reserved up front so recording a message never allocates inside
            // the WNDPROC.
            messages: Vec::with_capacity(MAX_MESSAGES),
            down: 0,
            up: 0,
            armed: None,
        }
    }
}

struct HookState {
    hwnd: HWND,
    owner_thread: u32,
    original_wndproc: Cell<isize>,
    alive: Cell<bool>,
    /// True while this subclass frame is on the stack. Retiring would free the
    /// state that frame still holds, so it is refused instead.
    in_proc: Cell<bool>,
    /// True when a re-entrant borrow or an inconsistent call path was observed.
    reentry: Cell<bool>,
    data: RefCell<ProbeData>,
}

impl HookState {
    fn pointer(&self) -> isize {
        self as *const HookState as isize
    }

    /// Mark a sticky rejection without ever panicking through FFI.
    fn mark_sticky(&self, sticky: Sticky) {
        if self.reentry.get() {
            return;
        }
        match self.data.try_borrow_mut() {
            Ok(mut data) => {
                if data.sticky.is_none() {
                    data.sticky = Some(sticky);
                }
            }
            Err(_) => self.reentry.set(true),
        }
    }

    /// Record one real left-button message. Metadata is captured by the WNDPROC
    /// before this call. The borrow is released before returning to the FFI
    /// caller, which then forwards the message to GPUI's original procedure.
    fn record_left(&self, x: i32, y: i32, up: bool, extra_info: u64, source: SourceDiag) {
        if self.reentry.get() {
            return;
        }
        let mut data = match self.data.try_borrow_mut() {
            Ok(data) => data,
            Err(_) => {
                self.reentry.set(true);
                return;
            }
        };
        if data.sticky.is_some() {
            return;
        }
        // Fixed FIFO bound: nothing beyond 64 recorded messages is accepted,
        // regardless of arm state.
        if data.messages.len() >= MAX_MESSAGES {
            data.sticky = Some(Sticky::Overflow);
            return;
        }

        // A left message is admissible only inside exactly one armed pair.
        let Some(mut arm) = data.armed else {
            data.sticky = Some(Sticky::Unattributed { up });
            return;
        };

        // Full-width marker comparison. Bit 0 is the system-owned injected flag
        // in real delivered extra-info, so it alone is masked; every other bit,
        // including all of the high 32, must equal the marker. No low-word
        // fallback and no GetCurrentInputMessageSource substitute.
        if (extra_info & !1) != (INPUT_MARKER as u64 & !1) {
            data.sticky = Some(Sticky::Marker {
                high32_set: (extra_info >> 32) != 0,
                low32_match: (extra_info as u32 & !1) == (INPUT_MARKER as u32 & !1),
                zero: extra_info == 0,
                source,
            });
            return;
        }

        match (arm.phase, up) {
            (ArmPhase::Down, true) => {
                data.sticky = Some(Sticky::OutOfOrder { up });
                return;
            }
            (ArmPhase::Down, false) => {
                if x != arm.x || y != arm.y {
                    data.sticky = Some(Sticky::Coordinate {
                        expected_x: arm.x,
                        expected_y: arm.y,
                        actual_x: x,
                        actual_y: y,
                        up,
                    });
                    return;
                }
                arm.phase = ArmPhase::Up;
            }
            (ArmPhase::Up, false) => {
                data.sticky = Some(Sticky::OutOfOrder { up });
                return;
            }
            (ArmPhase::Up, true) => {
                if x != arm.x || y != arm.y {
                    data.sticky = Some(Sticky::Coordinate {
                        expected_x: arm.x,
                        expected_y: arm.y,
                        actual_x: x,
                        actual_y: y,
                        up,
                    });
                    return;
                }
                arm.phase = ArmPhase::Complete;
            }
            (ArmPhase::Complete, _) => {
                data.sticky = Some(Sticky::Unexpected { up });
                return;
            }
        }

        // Only an attributed message that belongs to the armed pair is stored.
        // A rejected message instead leaves a sticky error, and the fixed
        // snapshot interface carries no per-message match flag.
        data.armed = Some(arm);
        data.messages.push(PointerMessage {
            x,
            y,
            up,
            extra_info,
        });
        if up {
            data.up += 1;
        } else {
            data.down += 1;
        }
    }
}

fn low_word(value: isize) -> i32 {
    (value as u32 & 0xffff) as u16 as i16 as i32
}

fn high_word(value: isize) -> i32 {
    ((value as u32 >> 16) & 0xffff) as u16 as i16 as i32
}

fn is_other_button(message: u32) -> bool {
    matches!(
        message,
        WM_RBUTTONDOWN
            | WM_RBUTTONUP
            | WM_RBUTTONDBLCLK
            | WM_MBUTTONDOWN
            | WM_MBUTTONUP
            | WM_MBUTTONDBLCLK
            | WM_XBUTTONDOWN
            | WM_XBUTTONUP
            | WM_XBUTTONDBLCLK
    )
}

fn is_keyboard(message: u32) -> bool {
    matches!(
        message,
        WM_KEYDOWN
            | WM_KEYUP
            | WM_CHAR
            | WM_DEADCHAR
            | WM_SYSKEYDOWN
            | WM_SYSKEYUP
            | WM_SYSCHAR
            | WM_SYSDEADCHAR
            | WM_UNICHAR
    )
}

/// Subclass procedure. It never swallows a GPUI message on the normal path:
/// every message ends in `CallWindowProcW` with the original WNDPROC captured
/// before subclass installation.
unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Read message-associated metadata before any other user32 query or any
    // nested window processing can replace the thread's extra-info state.
    let left_button = message == WM_LBUTTONDOWN || message == WM_LBUTTONUP;
    let metadata = if left_button {
        // SAFETY: no arguments; reads this thread's current extra-info value.
        let extra_info = unsafe { GetMessageExtraInfo() } as usize as u64;
        Some((extra_info, input_source()))
    } else {
        None
    };

    let pointer = unsafe { GetPropW(hwnd, prop_name()) };
    if pointer == 0 {
        // Install puts the property in place before the subclass, and
        // `WM_NCDESTROY` removes it only after this frame has been entered. So
        // this is unreachable in the supported lifecycle; if it ever happens,
        // still forward through the saved original instead of swallowing a
        // GPUI message.
        let fallback = unsafe { GetPropW(hwnd, prop_original()) };
        return call_original(fallback, hwnd, message, wparam, lparam);
    }
    // SAFETY: the property stores the live `Box<HookState>` owned by the probe.
    let state = unsafe { &*(pointer as *const HookState) };
    // Mark the active frame first: retirement must not free this state while it
    // is still borrowed by this call. Reset on every exit path without unwind.
    let _in_proc = InProcGuard::new(&state.in_proc);
    let original = state.original_wndproc.get();
    if original == 0 {
        // Re-entry protection: never call a null original procedure.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }

    if message == WM_NCDESTROY {
        // Invalidate the observation, then drop the state property before
        // chaining so no later frame can reach the box the owner frees after the
        // destroy returns. The fallback original property stays available for
        // any re-entrant message during GPUI's destroy path.
        state.alive.set(false);
        if !state.reentry.get() {
            let reset = match state.data.try_borrow_mut() {
                Ok(mut data) => {
                    data.armed = None;
                    true
                }
                Err(_) => false,
            };
            if !reset {
                state.reentry.set(true);
            }
        }
        unsafe {
            RemovePropW(hwnd, prop_name());
        }
        // The fallback original property is intentionally left for the window
        // object to own until destruction. A message re-entered during or after
        // GPUI's destroy chain still finds the original procedure and is
        // forwarded instead of swallowed. It points at code, not the heap box,
        // and the state property is already gone.
        return call_original(original, hwnd, message, wparam, lparam);
    }

    match message {
        WM_LBUTTONDOWN | WM_LBUTTONUP => {
            let x = low_word(lparam);
            let y = high_word(lparam);
            let up = message == WM_LBUTTONUP;
            if let Some((extra_info, source)) = metadata {
                state.record_left(x, y, up, extra_info, source);
            }
        }
        // A second left down in double-click form is outside the single armed
        // down/up pair this probe observes.
        WM_LBUTTONDBLCLK => state.mark_sticky(Sticky::Unexpected { up: false }),
        // Ordinary hover is intentionally not judged: system or synthetic
        // cursor movement must not poison a real click observation.
        WM_MOUSEMOVE => {}
        _ if is_other_button(message) => state.mark_sticky(Sticky::ForeignButton { message }),
        _ if is_keyboard(message) => state.mark_sticky(Sticky::Keyboard { message }),
        _ => {}
    }

    // The state borrow has been released by this point. Forward unchanged.
    call_original(original, hwnd, message, wparam, lparam)
}

/// Marks that a subclass frame is active for this observation.
struct InProcGuard<'a> {
    flag: &'a Cell<bool>,
    previous: bool,
}

impl<'a> InProcGuard<'a> {
    fn new(flag: &'a Cell<bool>) -> Self {
        let previous = flag.replace(true);
        Self { flag, previous }
    }
}

impl Drop for InProcGuard<'_> {
    fn drop(&mut self) {
        self.flag.set(self.previous);
    }
}

struct FindWindow {
    process: u32,
    title: *const u16,
    title_len: usize,
    count: usize,
    found: [HWND; 2],
}

/// Callback used only during install. Matching is exact on pid, visibility and
/// the complete UTF-16 title; no substring or prefix matching.
unsafe extern "system" fn find_window(hwnd: HWND, lparam: LPARAM) -> i32 {
    // SAFETY: EnumWindows hands back the caller's context, live for the enum.
    let context = unsafe { &mut *(lparam as *mut FindWindow) };
    if unsafe { IsWindowVisible(hwnd) } == 0 {
        return 1;
    }
    let mut process = 0u32;
    // SAFETY: `process` is writable; the thread-id result is ignored here.
    unsafe { GetWindowThreadProcessId(hwnd, &mut process) };
    if process != context.process {
        return 1;
    }
    if context.title_len >= TITLE_CAP || context.title_len > i32::MAX as usize {
        return 1;
    }
    let title_len = context.title_len as i32;
    if unsafe { GetWindowTextLengthW(hwnd) } != title_len {
        return 1;
    }
    let mut buffer = [0u16; TITLE_CAP];
    let copied = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), TITLE_CAP as i32) };
    if copied != title_len {
        return 1;
    }
    // SAFETY: the title slice lives in the caller of EnumWindows for the whole
    // enumeration; `title_len` was validated against TITLE_CAP.
    let expected = unsafe { std::slice::from_raw_parts(context.title, context.title_len) };
    // `get` keeps this FFI callback panic-free even if that invariant broke.
    if buffer.get(..context.title_len) != Some(expected) {
        return 1;
    }
    context.count = context.count.saturating_add(1);
    if context.count <= 2 {
        context.found[context.count - 1] = hwnd;
    }
    1
}

/// RAII owner of one subclass on the real GPUI window.
pub(super) struct PointerProbe {
    state: Option<Box<HookState>>,
    /// The probe must stay on the thread that owns the GPUI window.
    _not_send_sync: PhantomData<Rc<()>>,
}

impl PointerProbe {
    pub(super) fn install(title: &str) -> Result<Self, String> {
        let current_thread = thread_id();
        // SAFETY: no arguments; reads this process's id.
        let process = unsafe { GetCurrentProcessId() };

        let expected: Vec<u16> = title.encode_utf16().collect();
        if expected.is_empty() {
            return Err("pointer probe requires a non-empty window title".into());
        }
        if expected.contains(&0) {
            return Err("pointer probe title must not contain an interior NUL".into());
        }
        if expected.len() >= TITLE_CAP {
            return Err(format!(
                "pointer probe title is longer than the supported {TITLE_CAP}-unit bound"
            ));
        }

        let mut find = FindWindow {
            process,
            title: expected.as_ptr(),
            title_len: expected.len(),
            count: 0,
            found: [0; 2],
        };
        // SAFETY: the callback only uses the context during this call.
        let enumerated =
            unsafe { EnumWindows(Some(find_window), &mut find as *mut FindWindow as LPARAM) };
        if enumerated == 0 {
            return Err(failed("EnumWindows failed"));
        }
        if find.count != 1 {
            return Err(format!(
                "expected exactly one visible window with the exact title in this process, found {}",
                find.count
            ));
        }
        let hwnd = find.found[0];
        if hwnd == 0 {
            return Err("EnumWindows reported a null HWND".into());
        }

        // Re-check identity after the enumeration, then require the window to
        // belong to this calling thread, not merely to this process.
        if unsafe { IsWindow(hwnd) } == 0 {
            return Err("GPUI window disappeared during probe install".into());
        }
        if unsafe { IsWindowVisible(hwnd) } == 0 {
            return Err("GPUI window is no longer visible during probe install".into());
        }
        let mut window_process = 0u32;
        // SAFETY: `window_process` is writable.
        let owner_thread = unsafe { GetWindowThreadProcessId(hwnd, &mut window_process) };
        if window_process != process || owner_thread != current_thread {
            return Err(format!(
                "GPUI window owner mismatch: process {window_process} (want {process}), thread {owner_thread} (want {current_thread})"
            ));
        }

        let mut confirm = vec![0u16; expected.len() + 1];
        let copied = unsafe { GetWindowTextW(hwnd, confirm.as_mut_ptr(), confirm.len() as i32) };
        if unsafe { GetWindowTextLengthW(hwnd) } != expected.len() as i32
            || copied != expected.len() as i32
            || confirm.get(..expected.len()) != Some(expected.as_slice())
        {
            return Err("GPUI window title changed during probe install".into());
        }

        let original = unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) };
        if original == 0 {
            return Err(failed("GetWindowLongPtrW(GWLP_WNDPROC) returned null"));
        }
        if unsafe { GetPropW(hwnd, prop_name()) } != 0
            || unsafe { GetPropW(hwnd, prop_original()) } != 0
        {
            return Err("GPUI window already carries a pointer probe property".into());
        }

        let state = Box::new(HookState {
            hwnd,
            owner_thread: current_thread,
            original_wndproc: Cell::new(original),
            alive: Cell::new(true),
            in_proc: Cell::new(false),
            reentry: Cell::new(false),
            data: RefCell::new(ProbeData::new()),
        });
        let state_pointer = state.pointer();

        // Install the property before the subclass so the WNDPROC can always
        // recover its state. This never touches GPUI's GWLP_USERDATA.
        if unsafe { SetPropW(hwnd, prop_name(), state_pointer) } == 0 {
            return Err(failed("SetPropW for pointer probe state failed"));
        }
        if unsafe { GetPropW(hwnd, prop_name()) } != state_pointer {
            unsafe {
                RemovePropW(hwnd, prop_name());
            }
            return Err("SetPropW did not store the pointer probe state handle".into());
        }
        // The original WNDPROC is also stored separately, before the subclass
        // exists, so any orphaned subclass frame can still forward.
        let original_stored = unsafe { SetPropW(hwnd, prop_original(), original) } != 0
            && unsafe { GetPropW(hwnd, prop_original()) } == original;
        if !original_stored {
            unsafe {
                RemovePropW(hwnd, prop_original());
                RemovePropW(hwnd, prop_name());
            }
            return Err("SetPropW for the original WNDPROC was not retained".into());
        }

        let subclass = subclass_address();
        let previous = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, subclass) };
        if previous != original {
            // The cooperative single-writer contract was violated. Preserve the
            // actual displaced chain for any future chained frame if rollback
            // is impossible. This cannot retroactively fix an in-flight frame.
            if previous != 0 {
                state.original_wndproc.set(previous);
                unsafe {
                    SetPropW(hwnd, prop_original(), previous);
                }
            }
            // Another writer changed the WNDPROC between our read and install.
            // Undo our own installation and put back the procedure we actually
            // displaced, never the stale pre-read value.
            let current = unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) };
            if current != subclass {
                // A third party replaced us again and may have saved our
                // procedure in its chain. Keep both properties and leak the
                // state so such a chained call still forwards through
                // `PROP_ORIGINAL` instead of touching freed memory.
                let _ = Box::leak(state);
                return Err(format!(
                    "SetWindowLongPtrW(GWLP_WNDPROC) returned 0x{previous:x}, expected the previously read 0x{original:x}"
                ));
            }
            let restore = if previous != 0 { previous } else { original };
            let restored = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, restore) };
            if restored != subclass || unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) } != restore {
                // The installation could not be undone; our procedure may still
                // be reachable. Keep the properties and leak the state.
                let _ = Box::leak(state);
                return Err(
                    "failed to undo an unexpected pointer probe subclass installation".into(),
                );
            }
            // Our subclass is out of the chain again: remove both properties,
            // then let the state box drop with this scope.
            unsafe {
                RemovePropW(hwnd, prop_original());
                RemovePropW(hwnd, prop_name());
            }
            return Err(format!(
                "SetWindowLongPtrW(GWLP_WNDPROC) returned 0x{previous:x}, expected the previously read 0x{original:x}"
            ));
        }
        if unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) } != subclass {
            // A third party already replaced our subclass and may still chain
            // to it. Never clobber their procedure; keep the properties and
            // leak the state so a chained call forwards to the original GPUI
            // procedure rather than dereferencing freed memory.
            let _ = Box::leak(state);
            return Err("GPUI window did not retain the installed pointer probe subclass".into());
        }

        Ok(Self {
            state: Some(state),
            _not_send_sync: PhantomData,
        })
    }

    fn live(&self) -> Result<&HookState, String> {
        let state = self
            .state
            .as_deref()
            .ok_or_else(|| "pointer probe is already retired".to_owned())?;
        if thread_id() != state.owner_thread {
            return Err("pointer probe must be used from the GPUI owner thread".into());
        }
        if !state.alive.get() || unsafe { IsWindow(state.hwnd) } == 0 {
            return Err("GPUI window was destroyed; pointer observation is not live".into());
        }
        if unsafe { GetPropW(state.hwnd, prop_name()) } != state.pointer()
            || unsafe { GetPropW(state.hwnd, prop_original()) } != state.original_wndproc.get()
        {
            return Err(
                "GPUI window no longer carries this probe property (foreign or reused HWND)".into(),
            );
        }
        if unsafe { GetWindowLongPtrW(state.hwnd, GWLP_WNDPROC) } != subclass_address() {
            return Err("pointer probe subclass is no longer current for the GPUI window".into());
        }
        Ok(state)
    }

    pub(super) fn info(&self) -> Result<WindowInfo, String> {
        let state = self.live()?;
        let mut rect = RECT::default();
        if unsafe { GetClientRect(state.hwnd, &mut rect) } == 0 {
            return Err(failed("GetClientRect failed"));
        }
        let width = rect.right.checked_sub(rect.left).unwrap_or(0);
        let height = rect.bottom.checked_sub(rect.top).unwrap_or(0);
        if width <= 0 || height <= 0 {
            return Err("GPUI window client area is empty".into());
        }
        let mut origin = POINT::default();
        if unsafe { ClientToScreen(state.hwnd, &mut origin) } == 0 {
            return Err(failed("ClientToScreen failed"));
        }
        let dpi = unsafe { GetDpiForWindow(state.hwnd) };
        if dpi == 0 {
            return Err(failed("GetDpiForWindow returned zero"));
        }
        let mut pid = 0u32;
        // SAFETY: `pid` is writable; the owner-thread result was already checked
        // at install and by `live`.
        let owner = unsafe { GetWindowThreadProcessId(state.hwnd, &mut pid) };
        if owner != state.owner_thread || pid != unsafe { GetCurrentProcessId() } {
            return Err("GPUI window identity changed after probe install".into());
        }
        Ok(WindowInfo {
            hwnd: state.hwnd as usize as u64,
            pid,
            dpi,
            left: origin.x,
            top: origin.y,
            width: u32::try_from(width).map_err(|_| "client width is not a u32".to_owned())?,
            height: u32::try_from(height).map_err(|_| "client height is not a u32".to_owned())?,
        })
    }

    /// Validation-only identity without geometry: a minimized window may have
    /// an empty client, but must still retain its original property/owner.
    #[cfg(feature = "validation")]
    pub(super) fn owned_identity(&self) -> Result<(u64, u32, u32), String> {
        let state = self.live()?;
        let mut pid = 0;
        let thread = unsafe { GetWindowThreadProcessId(state.hwnd, &mut pid) };
        if thread != state.owner_thread || pid != unsafe { GetCurrentProcessId() } {
            return Err("owned probe HWND changed process/thread".into());
        }
        Ok((state.hwnd as usize as u64, pid, thread))
    }

    /// Read-only identity facts for non-input focus/animation observations.
    pub(super) fn owner_thread(&self) -> Result<u32, String> {
        Ok(self.live()?.owner_thread)
    }

    pub(super) fn is_foreground(&self) -> Result<bool, String> {
        let state = self.live()?;
        Ok(unsafe { GetForegroundWindow() } == state.hwnd)
    }

    /// Ordinary foreground request for this verified owned fixture HWND only.
    /// No synthetic ALT/input, thread attachment or foreground-policy changes.
    /// OS denial is an error, never a reason to inject into another window.
    pub(super) fn request_foreground(&self) -> Result<(), String> {
        let info = self.info()?;
        let state = self.live()?;
        if unsafe { GetForegroundWindow() } == state.hwnd {
            return Ok(());
        }
        let accepted = unsafe { SetForegroundWindow(state.hwnd) };
        if accepted == 0 || unsafe { GetForegroundWindow() } != state.hwnd {
            return Err(format!(
                "ordinary foreground request denied for owned HWND {}",
                info.hwnd
            ));
        }
        Ok(())
    }

    pub(super) fn arm(&self, x: i32, y: i32) -> Result<(), String> {
        if i16::try_from(x).is_err() || i16::try_from(y).is_err() {
            return Err(
                "armed client coordinates must be representable as signed 16-bit message words"
                    .into(),
            );
        }
        let state = self.live()?;
        if state.reentry.get() {
            return Err("pointer observation was re-entered; state is invalid".into());
        }
        let mut data = state
            .data
            .try_borrow_mut()
            .map_err(|_| "pointer probe state is currently borrowed".to_owned())?;
        if let Some(sticky) = data.sticky {
            return Err(format!(
                "pointer observation is sticky-rejected: {}",
                sticky.describe()
            ));
        }
        if data.armed.is_some() {
            return Err("pointer probe is already armed".into());
        }
        data.armed = Some(Arm {
            x,
            y,
            phase: ArmPhase::Down,
        });
        Ok(())
    }

    pub(super) fn snapshot(&self) -> Result<PointerSnapshot, String> {
        let state = self.live()?;
        if state.reentry.get() {
            return Err("pointer observation was re-entered; snapshot is invalid".into());
        }
        let data = state
            .data
            .try_borrow()
            .map_err(|_| "pointer probe state is currently borrowed".to_owned())?;
        if let Some(sticky) = data.sticky {
            return Err(format!(
                "pointer observation is sticky-rejected: {}",
                sticky.describe()
            ));
        }
        Ok(PointerSnapshot {
            down: data.down,
            up: data.up,
            messages: data.messages.clone(),
        })
    }

    pub(super) fn disarm(&self) -> Result<(), String> {
        let state = self.live()?;
        if state.reentry.get() {
            return Err("pointer observation was re-entered; state is invalid".into());
        }
        let mut data = state
            .data
            .try_borrow_mut()
            .map_err(|_| "pointer probe state is currently borrowed".to_owned())?;
        if let Some(sticky) = data.sticky {
            return Err(format!(
                "pointer observation is sticky-rejected: {}",
                sticky.describe()
            ));
        }
        let Some(arm) = data.armed else {
            return Err("pointer probe is not armed".into());
        };
        if arm.phase != ArmPhase::Complete {
            return Err("disarm requires the complete armed left-button down/up pair".into());
        }
        data.armed = None;
        Ok(())
    }

    /// Restores the original WNDPROC and removes the probe properties. Returns
    /// an error instead of guessing on a wrong thread, while our own subclass
    /// frame is still on the stack, on a foreign subclass, or on a
    /// foreign/reused HWND. If the window was already destroyed, only the local
    /// owner is cleared and no HWND state is restored.
    pub(super) fn retire(&mut self) -> Result<(), String> {
        let Some(state) = self.state.take() else {
            return Err("pointer probe is already retired".to_owned());
        };
        let retired = Self::retire_state(&state);
        match retired {
            Ok(()) => Ok(()),
            Err(error) => {
                // Keep ownership so `Drop` can make its own attempt and keep the
                // state reachable while a subclass may still be installed.
                self.state = Some(state);
                Err(error)
            }
        }
    }

    fn retire_state(state: &HookState) -> Result<(), String> {
        if thread_id() != state.owner_thread {
            return Err("pointer probe must be retired from the GPUI owner thread".into());
        }
        if state.in_proc.get() {
            // The active subclass frame holds this state; freeing it now would
            // be a use-after-free even if the window is concurrently dying.
            return Err("refusing to retire while the subclass frame is on the stack".into());
        }
        if !state.alive.get() {
            // WM_NCDESTROY already observed: only clear the local owner. No
            // user32 call is made on the old HWND at all.
            return Ok(());
        }
        let hwnd = state.hwnd;
        let pointer = state.pointer();
        let original = state.original_wndproc.get();
        if unsafe { IsWindow(hwnd) } == 0 {
            // Destroyed without the subclass seeing WM_NCDESTROY (for example a
            // forced teardown). Still clear the local owner without restoring.
            return Ok(());
        }
        if unsafe { GetPropW(hwnd, prop_name()) } != pointer
            || unsafe { GetPropW(hwnd, prop_original()) } != original
        {
            return Err(
                "refusing to retire on a foreign or reused HWND: probe property is absent or changed"
                    .into(),
            );
        }
        let subclass = subclass_address();
        let current = unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) };
        if current != subclass {
            return Err(
                "refusing to restore a foreign WNDPROC: probe subclass is not current".into(),
            );
        }
        let previous = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, original) };
        if previous != current {
            // No CAS exists here. Restore the actual displaced chain only if
            // the slot still holds the value this call wrote; do not knowingly
            // overwrite a subsequent writer. The Box/properties remain owned
            // on every error and are leaked if safe retirement is unconfirmed.
            let rollback =
                if previous != 0 && unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) } == original {
                    let displaced = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, previous) };
                    displaced == original
                        && unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) } == previous
                } else {
                    false
                };
            return Err(format!(
                "SetWindowLongPtrW restore returned unexpected previous value 0x{previous:x}; actual displaced chain rollback confirmed={rollback}; single-writer contract violated"
            ));
        }
        if unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) } != original {
            return Err("GPUI window did not accept the original WNDPROC restore".into());
        }
        let removed = unsafe { RemovePropW(hwnd, prop_name()) };
        if removed != pointer {
            return Err("RemovePropW did not return this probe's property handle".into());
        }
        unsafe {
            RemovePropW(hwnd, prop_original());
        }
        Ok(())
    }
}

impl Drop for PointerProbe {
    fn drop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        // A wrong-thread drop must not call user32 on this HWND. Leaking keeps
        // the WNDPROC state pointer valid instead of leaving a dangling hook.
        if thread_id() != state.owner_thread {
            let _ = Box::leak(state);
            return;
        }
        if Self::retire_state(&state).is_err() {
            // Restoration was refused or unconfirmed on this live window. Keep
            // the state box reachable from the installed subclass rather than
            // freeing memory a chained WNDPROC may still use.
            let _ = Box::leak(state);
        }
    }
}
