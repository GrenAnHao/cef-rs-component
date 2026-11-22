//! Real marked input on two owned GPUI windows. Pointer messages are observed
//! at WNDPROC; keyboard messages at WH_GETMESSAGE(PM_REMOVE), before a production
//! keyboard adapter may consume them. Neither observation proves GPUI/DOM work.
//! Two fixed queue callbacks filter their own window slot, so their chains
//! cannot double-record. This observer never changes MSG or injects input.
//! Never install PointerProbe on either HWND as well.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use rust_cef_gpui::{InputSnapshot, ports::BrowserHandle};
use std::{
    cell::{Cell, RefCell},
    marker::PhantomData,
    rc::Rc,
};

/// Distinct positive 32-bit even marker. Compare the complete raw ULONG_PTR;
/// neither bit 0 nor low-word matches are a substitute for exact equality.
const MARKER: u64 = 0x4346_4532;
const GWLP_WNDPROC: i32 = -4;
const WM_NCDESTROY: u32 = 0x82;
const WM_KEYDOWN: u32 = 0x100;
const WM_KEYUP: u32 = 0x101;
const WM_CHAR: u32 = 0x102;
const WM_MOUSEMOVE: u32 = 0x200;
const WM_LBUTTONDOWN: u32 = 0x201;
const WM_LBUTTONUP: u32 = 0x202;
const WM_RBUTTONDOWN: u32 = 0x204;
const WM_RBUTTONUP: u32 = 0x205;
const WM_MOUSEWHEEL: u32 = 0x20a;
const MAX: usize = 32;
const NAME: &[u16] = &[71, 80, 85, 73, 95, 67, 69, 70, 95, 73, 110, 112, 49, 0];
const ORIGINAL: &[u16] = &[71, 80, 85, 73, 95, 67, 69, 70, 95, 73, 110, 112, 50, 0];

type Hwnd = isize;
type Proc = unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize;
type HookProc = unsafe extern "system" fn(i32, usize, isize) -> isize;
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: i32,
    y: i32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct QueuedMessage {
    hwnd: Hwnd,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    point: Point,
    private: u32,
}
#[repr(C)]
#[derive(Default)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}
#[link(name = "user32")]
unsafe extern "system" {
    fn EnumWindows(
        callback: Option<unsafe extern "system" fn(Hwnd, isize) -> i32>,
        ctx: isize,
    ) -> i32;
    fn IsWindow(hwnd: Hwnd) -> i32;
    fn IsWindowVisible(hwnd: Hwnd) -> i32;
    fn IsIconic(hwnd: Hwnd) -> i32;
    fn GetWindowTextLengthW(hwnd: Hwnd) -> i32;
    fn GetWindowTextW(hwnd: Hwnd, text: *mut u16, len: i32) -> i32;
    fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
    fn GetWindowLongPtrW(hwnd: Hwnd, index: i32) -> isize;
    fn SetWindowLongPtrW(hwnd: Hwnd, index: i32, value: isize) -> isize;
    fn GetPropW(hwnd: Hwnd, name: *const u16) -> isize;
    fn SetPropW(hwnd: Hwnd, name: *const u16, value: isize) -> i32;
    fn RemovePropW(hwnd: Hwnd, name: *const u16) -> isize;
    fn CallWindowProcW(proc: Proc, hwnd: Hwnd, msg: u32, wp: usize, lp: isize) -> isize;
    fn DefWindowProcW(hwnd: Hwnd, msg: u32, wp: usize, lp: isize) -> isize;
    fn GetMessageExtraInfo() -> isize;
    fn GetClientRect(hwnd: Hwnd, rect: *mut Rect) -> i32;
    fn ClientToScreen(hwnd: Hwnd, point: *mut Point) -> i32;
    fn GetDpiForWindow(hwnd: Hwnd) -> u32;
    fn GetForegroundWindow() -> Hwnd;
    fn SetForegroundWindow(hwnd: Hwnd) -> i32;
    fn GetKeyboardLayout(thread: u32) -> isize;
    fn SetWindowsHookExW(kind: i32, proc: HookProc, module: isize, thread: u32) -> isize;
    fn UnhookWindowsHookEx(hook: isize) -> i32;
    fn CallNextHookEx(hook: isize, code: i32, removed: usize, message: isize) -> isize;
}
#[link(name = "imm32")]
unsafe extern "system" {
    fn ImmGetContext(hwnd: Hwnd) -> isize;
    fn ImmGetOpenStatus(context: isize) -> i32;
    fn ImmGetConversionStatus(context: isize, conversion: *mut u32, sentence: *mut u32) -> i32;
    fn ImmReleaseContext(hwnd: Hwnd, context: isize) -> i32;
    fn ImmIsIME(layout: isize) -> i32;
    fn ImmGetVirtualKey(hwnd: Hwnd) -> u32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
    fn GetCurrentProcessId() -> u32;
}
const _: () = assert!(std::mem::size_of::<Proc>() == std::mem::size_of::<isize>());
const _: () = assert!(MARKER <= i32::MAX as u64);
const _: () = assert!(std::mem::size_of::<QueuedMessage>() == 48);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Event {
    Down(i32, i32),
    RightDown(i32, i32),
    RightUp(i32, i32),
    Move(i32, i32),
    Up(i32, i32),
    Wheel(i32, i32, i16),
    KeyDown(u16),
    Char(u16),
    KeyUp(u16),
}
#[derive(Clone, Copy, Debug)]
pub(super) struct ImeFacts {
    pub keyboard_layout: u64,
    pub is_ime: bool,
    pub associated: bool,
    pub open: Option<bool>,
    pub conversion: Option<u32>,
}
#[derive(Clone, Copy, Debug)]
pub(super) struct WindowFacts {
    pub hwnd: u64,
    pub pid: u32,
    pub thread: u32,
    pub dpi: u32,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub ime: ImeFacts,
}
/// Raw protocol evidence observed before forwarding. `WM_CHAR` is tied to a
/// preceding marked keydown but its own raw extra-info is never trusted as a
/// SendInput marker. Numeric WPARAM/LPARAM are not rendered as user text.
#[derive(Clone, Debug)]
pub(super) struct Message {
    pub observation_point: &'static str,
    pub event: Option<Event>,
    pub message: u32,
    pub wparam: u64,
    pub lparam: u64,
    pub raw_extra_info: u64,
    pub char_associated: bool,
    pub accepted: bool,
    /// Exact synchronous Runtime::pump call scope, not a timer/UI turn span.
    pub during_manual_pump: bool,
    /// Pure owned Runtime query before the next queue hook (keyboard), or
    /// WNDPROC forwarding (pointer). Not GPUI/DOM-focus confirmation.
    pub input_snapshot: Option<InputSnapshot>,
}
#[derive(Clone, Debug)]
pub(super) struct Trace {
    pub expected: Vec<Event>,
    pub records: Vec<Message>,
    pub error: Option<String>,
    pub complete: bool,
}
struct Data {
    expected: Vec<Event>,
    records: Vec<Message>,
    got: Vec<Event>,
    armed: bool,
    /// Compose stages cannot predict the exact IME virtual-key traffic, so
    /// only the sequence comparison is relaxed. Marker, WM_CHAR association,
    /// nesting and FIFO bounds still reject foreign or unbalanced input.
    observe_all: bool,
    sticky: Option<&'static str>,
    key: Option<u16>,
}
struct State {
    hwnd: Hwnd,
    thread: u32,
    original: Cell<isize>,
    queue_hook: Cell<isize>,
    queue_slot: u8,
    in_queue: Cell<usize>,
    alive: Cell<bool>,
    in_proc: Cell<bool>,
    reentry: Cell<bool>,
    manual_pump: Rc<Cell<bool>>,
    browser: RefCell<Option<BrowserHandle>>,
    data: RefCell<Data>,
}
impl State {
    fn pointer(&self) -> isize {
        self as *const Self as isize
    }
    fn record(
        &self,
        event: Option<Event>,
        message: u32,
        wp: usize,
        lp: isize,
        marker: u64,
        parse_error: Option<&'static str>,
        observation_point: &'static str,
    ) {
        let Ok(mut data) = self.data.try_borrow_mut() else {
            self.reentry.set(true);
            return;
        };
        let associated = matches!(event, Some(Event::Char(_))) && data.key.is_some();
        let mut record = Message {
            observation_point,
            event,
            message,
            wparam: wp as u64,
            lparam: lp as usize as u64,
            raw_extra_info: marker,
            char_associated: associated,
            accepted: false,
            during_manual_pump: self.manual_pump.get(),
            input_snapshot: self.browser.try_borrow().ok().and_then(|browser| {
                browser
                    .as_ref()
                    .and_then(|handle| handle.input_snapshot().ok())
            }),
        };
        // Capture the first rejected raw message as well; never keep collecting
        // after sticky rejection; event/FIFO buffers were reserved at install.
        if data.sticky.is_some() {
            return;
        }
        if data.records.len() >= MAX {
            data.sticky = Some("input FIFO full; first excess raw message not retained");
            return;
        }
        let mut rejection = parse_error;
        if rejection.is_none() && !data.armed {
            rejection = Some("unarmed input reached owned window")
        }
        if rejection.is_none()
            && !data.observe_all
            && event != data.expected.get(data.got.len()).copied()
        {
            rejection = Some("input sequence, position or key differs from armed stage")
        }
        if rejection.is_none() {
            if let Some(event) = event {
                if matches!(event, Event::Char(_)) {
                    if !associated {
                        rejection = Some("WM_CHAR not associated with marked keydown")
                    }
                } else if marker != MARKER {
                    rejection = Some("complete raw SendInput marker mismatch")
                }
            } else {
                rejection = Some("unexpected input message")
            }
        }
        if rejection.is_none() {
            match event {
                Some(Event::KeyDown(v)) => {
                    if data.key.is_some() {
                        rejection = Some("nested keydown")
                    } else {
                        data.key = Some(v)
                    }
                }
                Some(Event::KeyUp(v)) => {
                    if data.key != Some(v) {
                        rejection = Some("keyup without matching marked keydown")
                    } else {
                        data.key = None
                    }
                }
                _ => {}
            }
        }
        if let Some(reason) = rejection {
            // Compose stages observe the real IME traffic: report the unexpected
            // message but keep collecting, because the complete sequence is the
            // evidence and the summary still fails closed.
            if !data.observe_all {
                data.sticky = Some(reason)
            }
        } else {
            record.accepted = true;
            if let Some(event) = event {
                data.got.push(event)
            }
        }
        data.records.push(record);
    }
}
fn subclass() -> isize {
    hook as Proc as isize
}
fn forward(previous: isize, hwnd: Hwnd, msg: u32, wp: usize, lp: isize) -> isize {
    if previous == 0 {
        return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
    }
    let proc: Proc = unsafe { std::mem::transmute(previous) };
    unsafe { CallWindowProcW(proc, hwnd, msg, wp, lp) }
}
unsafe extern "system" fn hook(hwnd: Hwnd, msg: u32, wp: usize, lp: isize) -> isize {
    // Read before any other user32 call or GPUI's original WNDPROC is entered.
    let marker = unsafe { GetMessageExtraInfo() } as usize as u64;
    let pointer = unsafe { GetPropW(hwnd, NAME.as_ptr()) };
    if pointer == 0 {
        return forward(
            unsafe { GetPropW(hwnd, ORIGINAL.as_ptr()) },
            hwnd,
            msg,
            wp,
            lp,
        );
    }
    // Clone the property owner's Rc before reentrant calls can retire it.
    unsafe { Rc::increment_strong_count(pointer as *const State) };
    let state = unsafe { Rc::from_raw(pointer as *const State) };
    let was_active = state.in_proc.replace(true);
    // A nested GPUI WNDPROC is legal; the probe's RefCell borrow is already
    // released before forwarding. Only an actual borrow conflict is sticky.
    let previous = state.original.get();
    if msg == WM_NCDESTROY {
        state.alive.set(false);
        unsafe {
            RemovePropW(hwnd, NAME.as_ptr());
        }
        let result = forward(previous, hwnd, msg, wp, lp);
        state.in_proc.set(was_active);
        return result;
    }
    if !(0x100..=0x109).contains(&msg) {
        let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            observe_message(&state, msg, wp, lp, marker, "window-proc");
        }));
        if observed.is_err() {
            state.reentry.set(true);
        }
    }
    let result = forward(previous, hwnd, msg, wp, lp);
    state.in_proc.set(was_active);
    result
}
/// Mirror of the production IME sentinel resolution.
fn probe_virtual_key(wp: usize, hwnd: Hwnd) -> u16 {
    if wp != 0xe5 {
        return wp as u16;
    }
    match unsafe { ImmGetVirtualKey(hwnd) } {
        resolved @ 1..=0xff => resolved as u16,
        _ => wp as u16,
    }
}
fn observe_message(
    state: &State,
    msg: u32,
    wp: usize,
    lp: isize,
    marker: u64,
    observation_point: &'static str,
) {
    let hwnd = state.hwnd;
    let xy = || {
        (
            (lp as u32 as u16 as i16) as i32,
            ((lp as u32 >> 16) as u16 as i16) as i32,
        )
    };
    let mut parse_error = None;
    let event = match msg {
        WM_LBUTTONDOWN => {
            let (x, y) = xy();
            Some(Event::Down(x, y))
        }
        WM_LBUTTONUP => {
            let (x, y) = xy();
            Some(Event::Up(x, y))
        }
        WM_RBUTTONDOWN => {
            let (x, y) = xy();
            Some(Event::RightDown(x, y))
        }
        WM_RBUTTONUP => {
            let (x, y) = xy();
            Some(Event::RightUp(x, y))
        }
        WM_MOUSEMOVE => {
            let (x, y) = xy();
            Some(Event::Move(x, y))
        }
        WM_MOUSEWHEEL => {
            // Wheel lParam is in screen, while other pointer lParams are client.
            let (sx, sy) = xy();
            let mut p = Point { x: sx, y: sy };
            if unsafe { ScreenToClient(hwnd, &mut p) } == 0 {
                parse_error = Some("ScreenToClient wheel failed");
                None
            } else {
                Some(Event::Wheel(p.x, p.y, (wp as u32 >> 16) as u16 as i16))
            }
        }
        // A live IME puts VK_PROCESSKEY in the DOWN while its UP keeps the real
        // key. Resolve the sentinel exactly like production so both halves of
        // one physical gesture index the same entry; the raw wparam is still
        // recorded as submitted.
        WM_KEYDOWN => Some(Event::KeyDown(probe_virtual_key(wp, hwnd))),
        WM_CHAR => Some(Event::Char(wp as u16)),
        WM_KEYUP => Some(Event::KeyUp(probe_virtual_key(wp, hwnd))),
        _ => None,
    };
    // Incidental zero-extra-info hover is not evidence. Any nonzero foreign
    // marker, including a one-bit variant, is a rejected raw FIFO record.
    if msg == WM_MOUSEMOVE && marker == 0 {
        // The other window's ordinary cursor hover is unrelated to this arm.
    } else if event.is_some() || parse_error.is_some() {
        state.record(event, msg, wp, lp, marker, parse_error, observation_point);
    } else if matches!(msg,0x203..=0x209 | 0x20b..=0x20d | 0x103..=0x109) {
        state.record(
            None,
            msg,
            wp,
            lp,
            marker,
            Some("unexpected non-fixture mouse/keyboard input"),
            observation_point,
        );
    }
}
fn removed_keyboard(code: i32, removed: usize, message: u32) -> bool {
    code == 0 && removed == 1 && (0x100..=0x109).contains(&message)
}
unsafe extern "system" fn queue_a(code: i32, removed: usize, lp: isize) -> isize {
    queue_observer(0, code, removed, lp)
}
unsafe extern "system" fn queue_b(code: i32, removed: usize, lp: isize) -> isize {
    queue_observer(1, code, removed, lp)
}
fn queue_observer(slot: u8, code: i32, removed: usize, lp: isize) -> isize {
    // A negative hook code belongs immediately to the next hook. Do not even
    // sample message metadata or enter our observer/unwind boundary for it.
    if code < 0 {
        return unsafe { CallNextHookEx(0, code, removed, lp) };
    }
    // Copy the original MSG; never consume, rewrite, Translate or Dispatch it.
    let marker = unsafe { GetMessageExtraInfo() } as usize as u64;
    let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if code != 0 || removed != 1 || lp == 0 {
            return;
        }
        let message = unsafe { (lp as *const QueuedMessage).read() };
        if !removed_keyboard(code, removed, message.message) {
            return;
        }
        let pointer = unsafe { GetPropW(message.hwnd, NAME.as_ptr()) };
        if pointer == 0 {
            return;
        }
        unsafe { Rc::increment_strong_count(pointer as *const State) };
        let state = unsafe { Rc::from_raw(pointer as *const State) };
        if state.queue_slot != slot
            || !state.alive.get()
            || state.thread != unsafe { GetCurrentThreadId() }
        {
            return;
        }
        state.in_queue.set(state.in_queue.get().saturating_add(1));
        struct InQueue(Rc<State>);
        impl Drop for InQueue {
            fn drop(&mut self) {
                self.0.in_queue.set(self.0.in_queue.get().saturating_sub(1));
            }
        }
        let _guard = InQueue(state.clone());
        observe_message(
            &state,
            message.message,
            message.wparam,
            message.lparam,
            marker,
            "removed-queue",
        );
    }));
    if observed.is_err() && lp != 0 && code == 0 && removed == 1 {
        let message = unsafe { (lp as *const QueuedMessage).read() };
        let pointer = unsafe { GetPropW(message.hwnd, NAME.as_ptr()) };
        if pointer != 0 {
            unsafe { (*(pointer as *const State)).reentry.set(true) };
        }
    }
    // All observation borrows and strong temporaries ended before next hook.
    unsafe { CallNextHookEx(0, code, removed, lp) }
}
#[link(name = "user32")]
unsafe extern "system" {
    fn ScreenToClient(hwnd: Hwnd, p: *mut Point) -> i32;
}
/// Called only after live() confirmed the actual HWND owner thread. A foreign
/// Python process querying ImmGetContext cannot establish this target's mode.
fn owned_ime_facts(hwnd: Hwnd) -> Result<ImeFacts, String> {
    let layout = unsafe { GetKeyboardLayout(0) };
    if layout == 0 {
        return Err("owner keyboard layout unavailable".into());
    }
    let context = unsafe { ImmGetContext(hwnd) };
    let mut facts = ImeFacts {
        keyboard_layout: layout as usize as u64,
        is_ime: unsafe { ImmIsIME(layout) } != 0,
        associated: context != 0,
        open: None,
        conversion: None,
    };
    if context != 0 {
        facts.open = Some(unsafe { ImmGetOpenStatus(context) } != 0);
        let mut conversion = 0;
        let mut sentence = 0;
        if unsafe { ImmGetConversionStatus(context, &mut conversion, &mut sentence) } != 0 {
            facts.conversion = Some(conversion);
        }
        if unsafe { ImmReleaseContext(hwnd, context) } == 0 {
            return Err("owner ImmReleaseContext failed".into());
        }
    }
    Ok(facts)
}
struct Search {
    title: *const u16,
    len: usize,
    pid: u32,
    count: usize,
    hwnd: Hwnd,
}
unsafe extern "system" fn visit(hwnd: Hwnd, ctx: isize) -> i32 {
    let s = unsafe { &mut *(ctx as *mut Search) };
    if unsafe { IsWindowVisible(hwnd) } == 0 {
        return 1;
    }
    let mut pid = 0;
    let thread = unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid != s.pid
        || thread != unsafe { GetCurrentThreadId() }
        || unsafe { GetWindowTextLengthW(hwnd) } != s.len as i32
    {
        return 1;
    }
    let mut buf = [0u16; 256];
    if s.len >= buf.len()
        || unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) } != s.len as i32
    {
        return 1;
    }
    if buf.get(..s.len) == Some(unsafe { std::slice::from_raw_parts(s.title, s.len) }) {
        s.count += 1;
        s.hwnd = hwnd
    }
    1
}
/// Validation-only, explicitly owned markers. Drop clears the marks even if
/// pump returns an error; no mark covers the asynchronous wait preceding it.
pub(super) struct PumpObservation<'a> {
    markers: &'a [Rc<Cell<bool>>],
}
impl<'a> PumpObservation<'a> {
    pub fn enter(markers: Option<&'a [Rc<Cell<bool>>]>) -> Result<Self, String> {
        let markers = markers.unwrap_or(&[]);
        if markers.iter().any(|marker| marker.get()) {
            return Err("nested manual-pump observation".into());
        }
        for marker in markers {
            marker.set(true);
        }
        Ok(Self { markers })
    }
}
impl Drop for PumpObservation<'_> {
    fn drop(&mut self) {
        for marker in self.markers {
            marker.set(false);
        }
    }
}
#[cfg(test)]
mod pump_tests {
    use super::*;
    #[test]
    fn keyboard_queue_observer_requires_action_and_actual_remove() {
        for message in [WM_KEYDOWN, WM_KEYUP, WM_CHAR, 0x104, 0x105] {
            assert!(removed_keyboard(0, 1, message));
            assert!(!removed_keyboard(0, 0, message));
            assert!(!removed_keyboard(-1, 1, message));
            assert!(!removed_keyboard(1, 1, message));
        }
        assert!(!removed_keyboard(0, 1, WM_LBUTTONDOWN));
        assert!(!removed_keyboard(0, 1, 0x12)); // Never WM_QUIT.
    }
    #[test]
    fn manual_pump_observation_is_scoped_and_nested_entry_does_not_clear_owner() {
        let markers = [Rc::new(Cell::new(false)), Rc::new(Cell::new(false))];
        {
            let _scope = PumpObservation::enter(Some(&markers)).unwrap();
            assert!(markers.iter().all(|marker| marker.get()));
            assert!(PumpObservation::enter(Some(&markers)).is_err());
            assert!(markers.iter().all(|marker| marker.get()));
        }
        assert!(markers.iter().all(|marker| !marker.get()));
        assert!(PumpObservation::enter(None).is_ok());
    }
}
pub(super) struct InputProbe {
    state: Option<Rc<State>>,
    _single_thread: PhantomData<Rc<()>>,
}
impl InputProbe {
    pub fn install(title: &str, queue_slot: u8) -> Result<Self, String> {
        if queue_slot > 1 {
            return Err("only two explicit input queue observation slots".into());
        }
        let title: Vec<_> = title.encode_utf16().collect();
        if title.is_empty() || title.len() >= 256 || title.contains(&0) {
            return Err("bad probe title".into());
        }
        let mut s = Search {
            title: title.as_ptr(),
            len: title.len(),
            pid: unsafe { GetCurrentProcessId() },
            count: 0,
            hwnd: 0,
        };
        if unsafe { EnumWindows(Some(visit), &mut s as *mut Search as isize) } == 0 || s.count != 1
        {
            return Err(format!(
                "owned title must uniquely resolve on owner thread, found {}",
                s.count
            ));
        }
        let hwnd = s.hwnd;
        if unsafe { GetPropW(hwnd, NAME.as_ptr()) } != 0
            || unsafe { GetPropW(hwnd, ORIGINAL.as_ptr()) } != 0
        {
            return Err("probe property already owned".into());
        }
        let original = unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) };
        if original == 0 {
            return Err("null original WNDPROC".into());
        }
        let state = Rc::new(State {
            hwnd,
            thread: unsafe { GetCurrentThreadId() },
            original: Cell::new(original),
            queue_hook: Cell::new(0),
            queue_slot,
            in_queue: Cell::new(0),
            alive: Cell::new(true),
            in_proc: Cell::new(false),
            reentry: Cell::new(false),
            manual_pump: Rc::new(Cell::new(false)),
            browser: RefCell::new(None),
            data: RefCell::new(Data {
                expected: Vec::with_capacity(MAX),
                records: Vec::with_capacity(MAX),
                got: Vec::with_capacity(MAX),
                armed: false,
                observe_all: false,
                sticky: None,
                key: None,
            }),
        });
        if unsafe { SetPropW(hwnd, NAME.as_ptr(), state.pointer()) } == 0 {
            return Err("SetProp state failed".into());
        }
        if unsafe { GetPropW(hwnd, NAME.as_ptr()) } != state.pointer() {
            unsafe {
                RemovePropW(hwnd, NAME.as_ptr());
            }
            return Err("SetProp state changed during install".into());
        }
        if unsafe { SetPropW(hwnd, ORIGINAL.as_ptr(), original) } == 0
            || unsafe { GetPropW(hwnd, ORIGINAL.as_ptr()) } != original
        {
            unsafe {
                RemovePropW(hwnd, ORIGINAL.as_ptr());
                RemovePropW(hwnd, NAME.as_ptr());
            }
            return Err("SetProp original failed or changed".into());
        }
        let previous = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, subclass()) };
        if previous != original {
            if previous != 0 {
                state.original.set(previous);
                unsafe {
                    SetPropW(hwnd, ORIGINAL.as_ptr(), previous);
                }
            }
            if unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) } == subclass() {
                let restore = if previous != 0 { previous } else { original };
                if unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, restore) } == subclass()
                    && unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) } == restore
                {
                    unsafe {
                        RemovePropW(hwnd, NAME.as_ptr());
                        RemovePropW(hwnd, ORIGINAL.as_ptr());
                    }
                } else {
                    let _ = Rc::into_raw(state);
                    return Err("subclass rollback unconfirmed; state leaked safely".into());
                }
            } else {
                let _ = Rc::into_raw(state);
                return Err("foreign WNDPROC chain; state leaked safely".into());
            }
            return Err("WNDPROC single-writer violation".into());
        }
        if unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) } != subclass() {
            let _ = Rc::into_raw(state);
            return Err("subclass displaced; state leaked safely".into());
        }
        let mut probe = Self {
            state: Some(state.clone()),
            _single_thread: PhantomData,
        };
        let callback: HookProc = if queue_slot == 0 { queue_a } else { queue_b };
        let queue_hook = unsafe { SetWindowsHookExW(3, callback, 0, state.thread) };
        if queue_hook == 0 {
            probe.retire()?;
            return Err("read-only WH_GETMESSAGE observation install failed".into());
        }
        state.queue_hook.set(queue_hook);
        Ok(probe)
    }
    pub fn observe_browser(&self, handle: BrowserHandle) -> Result<(), String> {
        let state = self.live()?;
        let mut browser = state
            .browser
            .try_borrow_mut()
            .map_err(|_| "browser observation busy")?;
        if browser.is_some() {
            return Err("browser observation already bound".into());
        }
        *browser = Some(handle);
        Ok(())
    }
    pub fn manual_pump_marker(&self) -> Result<Rc<Cell<bool>>, String> {
        Ok(self.live()?.manual_pump.clone())
    }
    fn live(&self) -> Result<&State, String> {
        let s = self.state.as_deref().ok_or("probe retired")?;
        if unsafe { GetCurrentThreadId() } != s.thread
            || !s.alive.get()
            || unsafe { IsWindow(s.hwnd) } == 0
            || unsafe { GetPropW(s.hwnd, NAME.as_ptr()) } != s.pointer()
            || unsafe { GetPropW(s.hwnd, ORIGINAL.as_ptr()) } != s.original.get()
            || unsafe { GetWindowLongPtrW(s.hwnd, GWLP_WNDPROC) } != subclass()
        {
            return Err("input probe owner/window/proc changed".into());
        }
        if s.reentry.get() {
            return Err("input probe reentered".into());
        }
        Ok(s)
    }
    pub fn info(&self) -> Result<WindowFacts, String> {
        let s = self.live()?;
        let mut rect = Rect::default();
        let mut point = Point::default();
        let mut pid = 0;
        if unsafe { GetClientRect(s.hwnd, &mut rect) } == 0
            || unsafe { ClientToScreen(s.hwnd, &mut point) } == 0
        {
            return Err("client geometry failed".into());
        }
        let thread = unsafe { GetWindowThreadProcessId(s.hwnd, &mut pid) };
        let dpi = unsafe { GetDpiForWindow(s.hwnd) };
        if pid != unsafe { GetCurrentProcessId() }
            || thread != s.thread
            || dpi == 0
            || rect.right <= rect.left
            || rect.bottom <= rect.top
        {
            return Err("window identity/geometry failed".into());
        }
        Ok(WindowFacts {
            hwnd: s.hwnd as usize as u64,
            pid,
            thread,
            dpi,
            left: point.x,
            top: point.y,
            width: (rect.right - rect.left) as u32,
            height: (rect.bottom - rect.top) as u32,
            ime: owned_ime_facts(s.hwnd)?,
        })
    }
    /// Read-only original Browser state on the verified HWND owner thread.
    pub fn accepts_text(&self) -> Result<bool, String> {
        let s = self.live()?;
        let browser = s
            .browser
            .try_borrow()
            .map_err(|_| "owner Browser temporarily borrowed")?;
        browser
            .as_ref()
            .ok_or("owner Browser not installed")?
            .input_snapshot()
            .map(|snapshot| snapshot.accepts_text)
            .map_err(|error| format!("owner text readiness unavailable: {error:?}"))
    }
    /// Read-only setup prerequisite; never requests or synthesizes activation.
    pub fn is_foreground(&self) -> Result<bool, String> {
        let s = self.live()?;
        Ok(unsafe { GetForegroundWindow() } == s.hwnd)
    }
    /// Menu-only extra qualification; never restores or unhides a window.
    pub fn foreground_visible(&self) -> Result<(), String> {
        let s = self.live()?;
        if unsafe { IsWindowVisible(s.hwnd) } == 0 || unsafe { IsIconic(s.hwnd) } != 0 {
            return Err("menu ordinary foreground requires visible non-iconic owner".into());
        }
        self.foreground()
    }
    /// Ask for our own window, bounded. Windows may refuse `SetForegroundWindow`
    /// for a while (another process owns the foreground queue), so the request is
    /// retried a few times; no synthetic activation, no AttachThreadInput and no
    /// foreign window is touched. A refusal that outlasts the bound still fails.
    pub fn foreground(&self) -> Result<(), String> {
        let s = self.live()?;
        // Wait before each request: Windows denies SetForegroundWindow while a
        // foreign window still owns the foreground queue, and hammering the call
        // makes that worse. Three spaced attempts keep the wait bounded.
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(600));
            }
            if unsafe { GetForegroundWindow() } == s.hwnd {
                return Ok(());
            }
            if unsafe { SetForegroundWindow(s.hwnd) } != 0
                && unsafe { GetForegroundWindow() } == s.hwnd
            {
                return Ok(());
            }
        }
        Err("ordinary owned SetForegroundWindow denied".into())
    }
    pub fn arm(&self, expected: &[Event]) -> Result<(), String> {
        let s = self.live()?;
        if expected.len() > MAX {
            return Err("invalid bounded input arm".into());
        }
        let mut d = s.data.try_borrow_mut().map_err(|_| "probe borrowed")?;
        if d.armed || d.sticky.is_some() || d.key.is_some() {
            return Err("probe already armed or rejected".into());
        }
        d.expected.clear();
        d.expected.extend_from_slice(expected);
        d.got.clear();
        d.records.clear();
        d.armed = true;
        d.observe_all = false;
        Ok(())
    }
    /// Record every marked message without demanding a predicted sequence.
    /// Used only where the real IME decides its own key/char order.
    pub fn arm_observe_all(&self) -> Result<(), String> {
        let s = self.live()?;
        let mut d = s.data.try_borrow_mut().map_err(|_| "probe borrowed")?;
        if d.armed || d.sticky.is_some() || d.key.is_some() {
            return Err("probe already armed or rejected".into());
        }
        d.expected.clear();
        d.got.clear();
        d.records.clear();
        d.armed = true;
        d.observe_all = true;
        Ok(())
    }
    pub fn trace(&self) -> Result<Trace, String> {
        let s = self.state.as_deref().ok_or("probe retired")?;
        if unsafe { GetCurrentThreadId() } != s.thread {
            return Err("trace read on foreign thread".into());
        }
        let d = s.data.try_borrow().map_err(|_| "probe trace borrowed")?;
        Ok(Trace {
            expected: d.expected.clone(),
            records: d.records.clone(),
            error: d
                .sticky
                .map(str::to_owned)
                .or_else(|| s.reentry.get().then(|| "probe borrow reentry".into())),
            complete: d.armed
                && d.sticky.is_none()
                && !s.reentry.get()
                && (d.observe_all && d.records.len() == d.got.len() || d.got == d.expected)
                && d.key.is_none(),
        })
    }
    pub fn complete(&self) -> Result<bool, String> {
        let trace = self.trace()?;
        if let Some(error) = trace.error {
            return Err(error);
        }
        Ok(trace.complete)
    }
    pub fn finish(&self) -> Result<Trace, String> {
        let trace = self.trace()?;
        if let Some(error) = trace.error.as_ref() {
            return Err(error.clone());
        }
        if !trace.complete {
            return Err("incomplete marked input stage".into());
        }
        let s = self.live()?;
        s.data.try_borrow_mut().map_err(|_| "probe borrowed")?.armed = false;
        Ok(trace)
    }
    pub fn retire(&mut self) -> Result<(), String> {
        let s = self.state.as_deref().ok_or("probe retired")?;
        if unsafe { GetCurrentThreadId() } != s.thread || s.in_proc.get() || s.in_queue.get() != 0 {
            return Err("unsafe input-probe retirement context".into());
        }
        if s.queue_hook.get() != 0 {
            if unsafe { UnhookWindowsHookEx(s.queue_hook.get()) } == 0 {
                return Err("read-only queue observer unhook unconfirmed".into());
            }
            s.queue_hook.set(0);
        }
        if s.alive.get() && unsafe { IsWindow(s.hwnd) } != 0 {
            self.live()?;
            let old = unsafe { SetWindowLongPtrW(s.hwnd, GWLP_WNDPROC, s.original.get()) };
            if old != subclass()
                || unsafe { GetWindowLongPtrW(s.hwnd, GWLP_WNDPROC) } != s.original.get()
            {
                if old != 0
                    && unsafe { GetWindowLongPtrW(s.hwnd, GWLP_WNDPROC) } == s.original.get()
                {
                    unsafe {
                        SetWindowLongPtrW(s.hwnd, GWLP_WNDPROC, old);
                    }
                }
                return Err("WNDPROC restoration unconfirmed; refuse freeing state".into());
            }
            if unsafe { RemovePropW(s.hwnd, NAME.as_ptr()) } != s.pointer() {
                return Err("state property changed during retire".into());
            }
            unsafe {
                RemovePropW(s.hwnd, ORIGINAL.as_ptr());
            }
        }
        self.state.take();
        Ok(())
    }
}
impl Drop for InputProbe {
    fn drop(&mut self) {
        if self.state.is_some() && self.retire().is_err() {
            let _ = Rc::into_raw(self.state.take().unwrap());
        }
    }
}
