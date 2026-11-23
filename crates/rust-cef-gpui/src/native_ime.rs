//! UI-thread Win32 IME placement for one explicitly bound GPUI view.
//!
//! This is the native placement half of task 11.2. It consumes a bounded
//! composition anchor projected from owned CEF character geometry and gives the
//! original `HWND` an `ImmSetCompositionWindow` point plus an
//! `ImmSetCandidateWindow` position/exclusion rectangle. For active
//! Chinese/Japanese layouts (`LANG_CHINESE` / `LANG_JAPANESE`), during an
//! active composition on the focused owned `HWND` only (matching
//! `CefSharp.Wpf/Experimental/WpfIMEKeyboardHandler.cs:334-444`), it also
//! manages a temporary 1x1 system caret (`CreateCaret`/`SetCaretPos`) for
//! legacy non-TSF IMEs that read `GetCaretPos`, and guarantees `DestroyCaret`
//! on `invalidate`, refusal/focus loss, layout change away from CJK, or drop.
//! It deliberately does not:
//!
//! * activate a Browser or window, switch IME/layout, or send any key/text,
//! * leave a system caret alive outside an active CJK composition (`system_caret_owned`
//!   is `false` at construction, at pre-keyboard cutpoints, and after `invalidate`),
//! * claim that a Chinese IME visually adopted the candidate position. Nonzero
//!   Win32/IMM32 results only mean Windows accepted the placement request;
//!   live UIPI-free composition/candidate visual adoption remains unverified
//!   (`candidate_adoption_verified` stays `false`).
//!
//! One controller owns at most one latest projection and one armed
//! `App::defer`. A later `queue` replaces the pending projection in place; it
//! never becomes a second input queue, a timer, a retry loop, a global table or
//! a TLS slot. The deferred closure takes the payload and drops every
//! `RefCell` borrow before it validates GPUI state and calls any native API.
//! `invalidate` is a *transient* cancellation of the current pending projection:
//! it bumps an internal cancel generation so any already armed defer sees a
//! stale generation and skips IMM, while later `queue` calls are still honored.
//! Only binding retirement (`WindowToken` retired/rebound) or checked cancel
//! generation overflow makes this controller permanently refuse work.
//!
//! Geometry enters as client-area DIP that already contains the surface origin.
//! The queued GPUI scale must match `GetDpiForWindow`; the resulting native
//! DPI must remain current across the placement. Geometry is scaled once and
//! never converted with `ClientToScreen`. Win32 documents the position relative to
//! the containing window; the pinned CEF Windows sample likewise passes
//! device-scaled view coordinates directly. This contract is not evidence that
//! a real IME adopted the position; `candidate_adoption_verified` stays false.
//!
//! The owning view cancels projections when released or retired. An armed
//! `App::defer` holds an `Rc` clone, but only a fresh weak-view validation may
//! authorize placement; keeping this controller alive does not keep a view live.

use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    rc::Rc,
};

use gpui::{AnyWindowHandle, App, FocusHandle, Window};
use rust_cef_core::{
    contracts::{RendererState, SessionState},
    ime::ImeStatus,
    ports::{BrowserHandle, EngineError, ImeGeometry, NativeViewServices, WindowBinding},
};

use crate::binding::WindowToken;

type Handle = *mut c_void;

const CFS_POINT: u32 = 0x0002;
const CFS_CANDIDATEPOS: u32 = 0x0040;
const CFS_EXCLUDE: u32 = 0x0080;
const LANG_CHINESE: u16 = 0x0004;
const LANG_JAPANESE: u16 = 0x0011;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
#[allow(dead_code)] // Written for the Win32 ABI; the fields are not read back here.
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
#[allow(dead_code)] // Written for the Win32 ABI; the fields are not read back here.
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
#[allow(dead_code)] // Written for the Win32 ABI; the fields are not read back here.
struct CompositionForm {
    style: u32,
    point: Point,
    area: Rect,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
#[allow(dead_code)] // Written for the Win32 ABI; the fields are not read back here.
struct CandidateForm {
    index: u32,
    style: u32,
    point: Point,
    area: Rect,
}

// Fixed Win32 layouts, not pointer-size dependent.
const _: () = assert!(std::mem::size_of::<CompositionForm>() == 28);
const _: () = assert!(std::mem::size_of::<CandidateForm>() == 32);

#[link(name = "user32")]
unsafe extern "system" {
    fn IsWindow(hwnd: Handle) -> i32;
    fn GetWindowThreadProcessId(hwnd: Handle, process_id: *mut u32) -> u32;
    fn GetFocus() -> Handle;
    fn GetForegroundWindow() -> Handle;
    fn GetDpiForWindow(hwnd: Handle) -> u32;
    fn GetKeyboardLayout(id_thread: u32) -> Handle;
    fn CreateCaret(hwnd: Handle, bitmap: Handle, width: i32, height: i32) -> i32;
    fn SetCaretPos(x: i32, y: i32) -> i32;
    fn DestroyCaret() -> i32;
}

fn primary_lang_id(hkl: Handle) -> u16 {
    (hkl as usize as u16) & 0x03ff
}

fn layout_uses_temporary_caret(hkl: Handle) -> bool {
    matches!(primary_lang_id(hkl), LANG_CHINESE | LANG_JAPANESE)
}

// IMM32 has no documented `GetLastError` contract for these calls, so success
// is recorded as the real BOOL result and never as a fabricated error code.
#[link(name = "imm32")]
unsafe extern "system" {
    fn ImmGetContext(hwnd: Handle) -> Handle;
    fn ImmAssociateContextEx(hwnd: Handle, context: Handle, flags: u32) -> i32;
    fn ImmReleaseContext(hwnd: Handle, context: Handle) -> i32;
    fn ImmSetCompositionWindow(context: Handle, form: *const CompositionForm) -> i32;
    fn ImmSetCandidateWindow(context: Handle, form: *const CandidateForm) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
    fn GetCurrentProcessId() -> u32;
}

/// Why this controller did not place the IME, or which native step failed.
///
/// These are observation categories, not proof about IME behaviour. In
/// particular `FocusNotOwned` means the original HWND was not both
/// `GetForegroundWindow` and `GetFocus`; no other window is ever substituted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeImeFailure {
    WrongThread,
    WrongProcess,
    BindingMismatch,
    InvalidWindow,
    RetiredBinding,
    WrongWindow,
    GeometryRejected,
    StaleRevision,
    /// The original View projection could not be revalidated, including when
    /// its GPUI scale and the execution-time Win32 DPI no longer agree.
    ProjectionUnavailable,
    WindowUnavailable,
    WindowInactive,
    FocusLost,
    FocusNotOwned,
    BrowserUnavailable,
    RendererUnavailable,
    InputNotReady,
    CleanupPending,
    EpochMismatch,
    DpiUnavailable,
    NoImmContext,
    CompositionFailed,
    CandidateFailed,
    ContextReleaseFailed,
    /// A later `invalidate` superseded this projection before it reached IMM.
    Cancelled,
}

/// Truthful counters for one controller. No text, DOM caret or IME contents are
/// exposed. `applied` counts a completed placement (three accepted Win32 calls
/// and successful context release), not IME adoption.
///
/// `last_error`/`last_failure` keep the most recent refusal; a later success
/// does not erase the record of an earlier one, and the per-call counters show
/// what the latest attempt actually did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NativeImeDiagnostics {
    pub binding: Option<WindowBinding>,
    pub window_id: u64,
    pub owner_thread: u32,
    pub owner_process: u32,
    pub token_live: bool,
    pub pending: bool,
    pub defer_armed: bool,
    pub queued: u64,
    pub coalesced: u64,
    pub deferred_runs: u64,
    pub applied: u64,
    pub refused: u64,
    pub invalidated: u64,
    /// Projections dropped by `invalidate` before their defer ran.
    pub cancelled_pending: u64,
    /// Projections dropped by their own defer because a later `invalidate` won.
    pub cancelled: u64,
    /// Bumped by every `invalidate`; `None` means permanently closed.
    pub cancel_generation: Option<u64>,
    pub imm_context_attempts: u64,
    pub imm_contexts: u64,
    pub imm_context_failures: u64,
    pub imm_releases: u64,
    pub imm_release_failures: u64,
    pub composition_calls: u64,
    pub composition_successes: u64,
    pub candidate_calls: u64,
    pub candidate_successes: u64,
    pub last_error: Option<EngineError>,
    pub last_failure: Option<NativeImeFailure>,
    pub last_applied_revision: Option<u64>,
    pub last_applied_epoch: Option<u64>,
    /// Last client-pixel x/y accepted by IMM, or `None` before any placement.
    pub last_applied_point: Option<(i32, i32)>,
    /// True only while this controller owns an active CJK composition's
    /// temporary 1x1 system caret (`CreateCaret`/`SetCaretPos`). Always `false`
    /// at rest and after `invalidate` or drop.
    pub system_caret_owned: bool,
    pub temporary_caret_placements: u64,
    pub temporary_caret_releases: u64,
    /// Always false in this version: no evidence exists that the active IME
    /// adopted the candidate/composition request.
    pub candidate_adoption_verified: bool,
    /// Always true in this version: `ImeGeometry` was scaled from client DIP to
    /// client physical pixels and was not converted with `ClientToScreen`.
    pub client_pixels_without_screen_conversion: bool,
}

/// UI-thread-only IME placement owner.
///
/// Constructed from the original [`WindowToken`]; it stores the raw `HWND`,
/// owner thread and process only after validating the real GPUI window and the
/// token. Construction is inert: no IMM context is opened and no IME state is
/// changed.
pub(crate) struct NativeImeController {
    token: WindowToken,
    binding: WindowBinding,
    window_id: u64,
    window_handle: AnyWindowHandle,
    thread: u32,
    process: u32,
    service: WindowsImeService,
    pending: RefCell<Option<Projection>>,
    armed: Cell<bool>,
    gate: Rc<CancelGate>,
    last_revision: Cell<Option<u64>>,
    diagnostics: RefCell<NativeImeDiagnostics>,
}

/// One owned projection. `BrowserHandle`, `FocusHandle` and the window handle
/// are the exact identities supplied by the host; no ambient lookup is done.
type ProjectionValidator = Rc<dyn Fn(&Window, &App) -> bool>;

struct Projection {
    geometry: ImeGeometry,
    projected_scale: f32,
    snapshot_revision: u64,
    epoch: u64,
    /// Cancel generation observed when this projection was queued. A later
    /// `invalidate` changes the controller's generation and makes this stale.
    generation: u64,
    handle: BrowserHandle,
    focus: FocusHandle,
    window: AnyWindowHandle,
    validate: ProjectionValidator,
}

/// Transient cancellation state shared by `queue`, `invalidate` and the defer.
///
/// `invalidate` bumps the generation. A projection captures the generation it
/// was queued under and is dropped if that generation moved by the time the
/// defer runs. Only checked overflow closes the gate permanently; ordinary
/// invalidation leaves the controller usable.
struct CancelGate {
    generation: Cell<Option<u64>>,
}

impl CancelGate {
    fn new() -> Self {
        Self {
            generation: Cell::new(Some(1)),
        }
    }

    /// `None` once the generation space is exhausted or already closed.
    fn current(&self) -> Option<u64> {
        self.generation.get()
    }

    /// Issues the next generation, or closes the gate on overflow.
    fn bump(&self) -> Option<u64> {
        let Some(generation) = self.generation.get() else {
            return None;
        };
        match generation.checked_add(1) {
            Some(next) => {
                self.generation.set(Some(next));
                Some(next)
            }
            None => {
                self.generation.set(None);
                None
            }
        }
    }

    /// Whether a projection queued at `generation` is still current.
    fn permits(&self, generation: u64) -> bool {
        self.generation.get() == Some(generation)
    }
}

impl NativeImeController {
    /// Validates the original window/token on the calling UI thread.
    ///
    /// Only the process/thread/HWND identity and the token's window binding are
    /// checked. Nothing is activated and IMM is not contacted.
    #[allow(clippy::new_ret_no_self)] // The host contract requires a shared handle.
    pub(crate) fn new(token: WindowToken, window: &Window) -> Result<Rc<Self>, EngineError> {
        if !token.is_live() {
            return Err(EngineError::Closed);
        }
        let binding = token.binding();
        let window_id = window.window_handle().window_id().as_u64();
        if binding.window != window_id || !token.matches_window(window_id) {
            return Err(EngineError::InvalidRequest);
        }

        let thread = unsafe { GetCurrentThreadId() };
        let process = unsafe { GetCurrentProcessId() };
        // Fully qualified on purpose: gpui's `Window` has an inherent
        // `window_handle()` returning its own handle type, and only the trait
        // method yields the raw Win32 handle.
        let raw = raw_window_handle::HasWindowHandle::window_handle(window)
            .map_err(|_| EngineError::InvalidRequest)?
            .as_raw();
        let raw_window_handle::RawWindowHandle::Win32(handle) = raw else {
            return Err(EngineError::InvalidRequest);
        };
        let hwnd = handle.hwnd.get() as Handle;
        if hwnd.is_null() {
            return Err(EngineError::InvalidRequest);
        }
        let mut owner_process = 0u32;
        let owner_thread = unsafe { GetWindowThreadProcessId(hwnd, &mut owner_process) };
        if owner_thread != thread {
            return Err(EngineError::WrongThread);
        }
        if owner_process != process {
            return Err(EngineError::InvalidRequest);
        }

        // A fresh GPUI window has no IMM context; without one every later
        // composition query returns NoImmContext. Install the thread's default
        // IME context here and prove it is queryable/releasable. No private
        // context is created and no mode/candidate is changed.
        const IACE_DEFAULT: u32 = 0x0010;
        unsafe { ImmAssociateContextEx(hwnd, std::ptr::null_mut(), IACE_DEFAULT) };
        let probed = unsafe { ImmGetContext(hwnd) };
        if probed.is_null() || unsafe { ImmReleaseContext(hwnd, probed) } == 0 {
            return Err(EngineError::NotReady);
        }

        let gate = Rc::new(CancelGate::new());
        let cancel_generation = gate.current();
        let service = WindowsImeService::new(
            token.clone(),
            Rc::clone(&gate),
            binding,
            hwnd,
            thread,
            process,
        );
        Ok(Rc::new(Self {
            token,
            binding,
            window_id,
            window_handle: window.window_handle(),
            thread,
            process,
            service,
            pending: RefCell::new(None),
            armed: Cell::new(false),
            gate,
            last_revision: Cell::new(None),
            diagnostics: RefCell::new(NativeImeDiagnostics {
                binding: Some(binding),
                window_id,
                owner_thread: thread,
                owner_process: process,
                token_live: true,
                cancel_generation,
                system_caret_owned: false,
                candidate_adoption_verified: false,
                client_pixels_without_screen_conversion: true,
                ..NativeImeDiagnostics::default()
            }),
        }))
    }

    /// Owns the latest projection for this controller and arms at most one
    /// `App::defer`.
    ///
    /// The projection is refused unless the original window/token is still
    /// current and the geometry is finite with positive width/height. A lower
    /// `snapshot_revision` than one already accepted is stale and is refused;
    /// equal revisions are allowed because callers may report several
    /// geometries for one CEF snapshot revision. Nothing here reads IMM state,
    /// and a prior `invalidate` never blocks a legitimate later projection.
    #[allow(clippy::too_many_arguments)] // The host-side projection contract is fixed.
    pub(crate) fn queue(
        self: &Rc<Self>,
        geometry: ImeGeometry,
        projected_scale: f32,
        snapshot_revision: u64,
        epoch: u64,
        handle: BrowserHandle,
        focus: FocusHandle,
        window: AnyWindowHandle,
        validate: ProjectionValidator,
        cx: &mut App,
    ) {
        if unsafe { GetCurrentThreadId() } != self.thread {
            self.refuse(NativeImeFailure::WrongThread, EngineError::WrongThread);
            return;
        }
        let Some(generation) = self.gate.current() else {
            // Only cancel-generation overflow closes the controller; a normal
            // `invalidate` keeps it open for later projections.
            self.refuse(NativeImeFailure::RetiredBinding, EngineError::Closed);
            return;
        };
        if !binding_current(&self.token, self.window_id) {
            self.refuse(NativeImeFailure::RetiredBinding, EngineError::Closed);
            return;
        }
        if window != self.window_handle || window.window_id().as_u64() != self.window_id {
            self.refuse(NativeImeFailure::WrongWindow, EngineError::InvalidRequest);
            return;
        }
        if !geometry_shape_ok(geometry) {
            self.refuse(
                NativeImeFailure::GeometryRejected,
                EngineError::InvalidRequest,
            );
            return;
        }
        if !revision_acceptable(snapshot_revision, self.last_revision.get()) {
            self.refuse(NativeImeFailure::StaleRevision, EngineError::InvalidRequest);
            return;
        }
        // Accepting the revision is independent of whether this projection
        // later applies: an older callback is stale either way.
        self.last_revision.set(Some(snapshot_revision));
        *self.pending.borrow_mut() = Some(Projection {
            geometry,
            projected_scale,
            snapshot_revision,
            epoch,
            generation,
            handle,
            focus,
            window,
            validate,
        });
        {
            let mut diagnostics = self.diagnostics.borrow_mut();
            diagnostics.queued = diagnostics.queued.saturating_add(1);
        }

        if self.armed.get() {
            let mut diagnostics = self.diagnostics.borrow_mut();
            diagnostics.coalesced = diagnostics.coalesced.saturating_add(1);
            return;
        }
        self.armed.set(true);
        self.diagnostics.borrow_mut().defer_armed = true;
        self.arm(cx);
    }

    /// Cancels the pending projection without closing the controller.
    ///
    /// Hosts call this on every render transition where a composition is gone,
    /// geometry is temporarily unavailable, or focus moved. The pending
    /// projection is dropped and the cancel generation is bumped, so an already
    /// armed defer cannot apply it; the defer itself is not cancellable in
    /// GPUI and, when it runs with no payload, contacts nothing. Later `queue`
    /// calls remain valid. Only checked generation overflow (or a retired
    /// `WindowToken`) makes the controller permanently refuse work.
    pub(crate) fn invalidate(&self) {
        let released = self.service.caret.release();
        let had_pending = self.pending.borrow_mut().take().is_some();
        let overflowed = self.gate.bump().is_none();
        let mut diagnostics = self.diagnostics.borrow_mut();
        diagnostics.invalidated = diagnostics.invalidated.saturating_add(1);
        if released {
            diagnostics.temporary_caret_releases =
                diagnostics.temporary_caret_releases.saturating_add(1);
        }
        diagnostics.system_caret_owned = self.service.caret.is_owned();
        if had_pending {
            // The projection is dropped, not retained for a later attempt.
            diagnostics.cancelled_pending = diagnostics.cancelled_pending.saturating_add(1);
        }
        diagnostics.pending = false;
        diagnostics.defer_armed = self.armed.get();
        diagnostics.token_live = binding_current(&self.token, self.window_id);
        diagnostics.cancel_generation = self.gate.current();
        if overflowed {
            // Generation exhaustion is the only controller-level permanent
            // closure; a retired token is checked on every `queue` instead.
            diagnostics.refused = diagnostics.refused.saturating_add(1);
            diagnostics.last_failure = Some(NativeImeFailure::RetiredBinding);
            diagnostics.last_error = Some(EngineError::Closed);
        }
    }

    /// Current counters plus live pending/armed/token state.
    pub(crate) fn diagnostics(&self) -> NativeImeDiagnostics {
        let mut snapshot = self.diagnostics.borrow().clone();
        snapshot.pending = self.pending.borrow().is_some();
        snapshot.defer_armed = self.armed.get();
        snapshot.token_live = binding_current(&self.token, self.window_id);
        snapshot.cancel_generation = self.gate.current();
        snapshot.system_caret_owned = self.service.caret.is_owned();
        snapshot.candidate_adoption_verified = false;
        snapshot.client_pixels_without_screen_conversion = true;
        snapshot
    }

    fn arm(self: &Rc<Self>, cx: &mut App) {
        let controller = Rc::clone(self);
        cx.defer(move |cx| controller.run_deferred(cx));
    }

    /// Runs after GPUI has unwound the window/entity borrows of the current
    /// effect cycle.
    ///
    /// `armed` stays true for the whole run: a nested `queue` during the short
    /// validation update then coalesces into `pending` instead of being lost.
    /// Exactly one follow-up defer is armed if such a projection arrives.
    fn run_deferred(self: &Rc<Self>, cx: &mut App) {
        // Take the payload in its own statement so no `RefCell` borrow survives
        // into GPUI updates or any FFI call.
        let projection = { self.pending.borrow_mut().take() };
        let Some(projection) = projection else {
            self.armed.set(false);
            self.diagnostics.borrow_mut().defer_armed = false;
            return;
        };
        {
            let mut diagnostics = self.diagnostics.borrow_mut();
            diagnostics.deferred_runs = diagnostics.deferred_runs.saturating_add(1);
            diagnostics.pending = false;
            diagnostics.defer_armed = true;
        }

        let thread = self.thread;
        let process = self.process;
        self.try_apply(projection, thread, process, cx);

        // A newer projection may have been queued while this run held `armed`;
        // it owns the next single defer. Otherwise this controller is idle.
        let more = { self.pending.borrow().is_some() } && self.gate.current().is_some();
        if more {
            self.arm(cx);
        } else {
            self.armed.set(false);
        }
    }

    fn try_apply(&self, projection: Projection, thread: u32, process: u32, cx: &mut App) {
        // `thread`/`process` are the controller's own identities, copied before
        // any work so the projection itself carries no controller borrow.
        if !binding_current(&self.token, self.window_id) {
            self.refuse(NativeImeFailure::RetiredBinding, EngineError::Closed);
            return;
        }
        if unsafe { GetCurrentThreadId() } != thread {
            self.refuse(NativeImeFailure::WrongThread, EngineError::WrongThread);
            return;
        }
        // `process` guards against a fork/child misusing a copied controller;
        // the adapter re-checks it immediately before IMM as well.
        if unsafe { GetCurrentProcessId() } != process {
            self.refuse(NativeImeFailure::WrongProcess, EngineError::WrongThread);
            return;
        }

        let Projection {
            geometry,
            projected_scale,
            snapshot_revision,
            epoch,
            generation,
            handle,
            focus,
            window,
            validate,
        } = projection;
        if !self.gate.permits(generation) {
            // A later `invalidate` superseded this payload. It is dropped, not
            // retried, and the controller stays open for future projections.
            self.refuse(NativeImeFailure::Cancelled, EngineError::NotReady);
            return;
        }
        if window != self.window_handle || window.window_id().as_u64() != self.window_id {
            self.refuse(NativeImeFailure::WrongWindow, EngineError::InvalidRequest);
            return;
        }

        // Short validation window: no native call and no controller `RefCell`
        // borrow is held here. `BrowserHandle` reads are pure runtime snapshots.
        // Every condition is re-checked because the queue-time projection could
        // already be stale: a renderer may have exited, the input epoch may have
        // moved, or focus/activation may have changed after `queue`.
        let bound_browser = self.binding.browser;
        let verdict = window.update(cx, |_, gpui_window, app| {
            if gpui_window.window_handle() != window {
                return Err(NativeImeFailure::WrongWindow);
            }
            if !gpui_window.is_window_active() {
                return Err(NativeImeFailure::WindowInactive);
            }
            if !focus.is_focused(gpui_window) {
                return Err(NativeImeFailure::FocusLost);
            }
            let snapshot = handle.snapshot();
            if snapshot.browser != bound_browser || snapshot.state != SessionState::Alive {
                return Err(NativeImeFailure::BrowserUnavailable);
            }
            if snapshot.renderer.state == RendererState::Terminated {
                return Err(NativeImeFailure::RendererUnavailable);
            }
            let input = handle
                .input_snapshot()
                .map_err(|_| NativeImeFailure::InputNotReady)?;
            // A lane with unresolved teardown must not receive new UI work.
            if input.cleanup_pending || input.cleanup_failed {
                return Err(NativeImeFailure::CleanupPending);
            }
            if input.epoch != epoch {
                return Err(NativeImeFailure::EpochMismatch);
            }
            let ime = handle
                .ime_snapshot()
                .map_err(|_| NativeImeFailure::InputNotReady)?;
            if ime.browser != bound_browser
                || ime.observed_input_epoch != epoch
                || ime.revision != snapshot_revision
                || ime.status != ImeStatus::Ready
            {
                return Err(NativeImeFailure::StaleRevision);
            }
            // The weak originating view rechecks image lifetime, source
            // generation/visibility/viewport and current local layout. All of
            // these immutable borrows end with this closure, before IMM.
            if !validate(gpui_window, app) {
                return Err(NativeImeFailure::ProjectionUnavailable);
            }
            Ok(())
        });
        match verdict {
            Ok(Ok(())) => {}
            Ok(Err(failure)) => {
                self.refuse(failure, EngineError::NotReady);
                return;
            }
            Err(_) => {
                self.refuse(NativeImeFailure::WindowUnavailable, EngineError::Closed);
                return;
            }
        }

        // The focus/epoch proof is intentionally short-lived. Re-check this
        // controller's own binding/cancel generation immediately before
        // touching the original window, and never retry on failure.
        if !binding_current(&self.token, self.window_id) {
            self.refuse(NativeImeFailure::RetiredBinding, EngineError::Closed);
            return;
        }
        if !self.gate.permits(generation) {
            self.refuse(NativeImeFailure::Cancelled, EngineError::NotReady);
            return;
        }
        if unsafe { GetCurrentThreadId() } != thread {
            self.refuse(NativeImeFailure::WrongThread, EngineError::WrongThread);
            return;
        }

        // Copy into a local mutable trait implementation: the adapter itself
        // holds no `RefCell`, so no borrow guard spans the IMM calls. The
        // adapter repeats the owner/PID/HWND/token check immediately before
        // contact, because the GPUI gate above is only a short proof.
        let mut native = self.service.clone();
        native.projection = Some(PlacementScope {
            generation,
            projected_scale,
        });
        let result = NativeViewServices::position_ime(&mut native, self.binding, geometry);
        let outcome = native.outcome();
        self.record_placement(snapshot_revision, epoch, result, outcome);
    }

    fn refuse(&self, failure: NativeImeFailure, error: EngineError) {
        let released = self.service.caret.release();
        let mut diagnostics = self.diagnostics.borrow_mut();
        if released {
            diagnostics.temporary_caret_releases =
                diagnostics.temporary_caret_releases.saturating_add(1);
        }
        diagnostics.system_caret_owned = self.service.caret.is_owned();
        if failure == NativeImeFailure::Cancelled {
            // A superseded projection was never offered to the native layer, so
            // it is not counted as a refusal.
            diagnostics.cancelled = diagnostics.cancelled.saturating_add(1);
            diagnostics.last_failure = Some(failure);
            return;
        }
        diagnostics.refused = diagnostics.refused.saturating_add(1);
        diagnostics.last_failure = Some(failure);
        diagnostics.last_error = Some(error);
    }

    fn record_placement(
        &self,
        snapshot_revision: u64,
        epoch: u64,
        result: Result<(), EngineError>,
        outcome: PlacementOutcome,
    ) {
        let released_on_error = if result.is_err() {
            self.service.caret.release()
        } else {
            false
        };
        let mut diagnostics = self.diagnostics.borrow_mut();
        if outcome.context_attempted {
            diagnostics.imm_context_attempts = diagnostics.imm_context_attempts.saturating_add(1);
        }
        if outcome.context_acquired {
            diagnostics.imm_contexts = diagnostics.imm_contexts.saturating_add(1);
        } else if outcome.context_attempted {
            diagnostics.imm_context_failures = diagnostics.imm_context_failures.saturating_add(1);
        }
        if outcome.release_attempted {
            diagnostics.imm_releases = diagnostics.imm_releases.saturating_add(1);
            if !outcome.release_ok {
                diagnostics.imm_release_failures =
                    diagnostics.imm_release_failures.saturating_add(1);
            }
        }
        if outcome.composition_called {
            diagnostics.composition_calls = diagnostics.composition_calls.saturating_add(1);
            if outcome.composition_ok {
                diagnostics.composition_successes =
                    diagnostics.composition_successes.saturating_add(1);
            }
        }
        diagnostics.candidate_calls = diagnostics
            .candidate_calls
            .saturating_add(u64::from(outcome.candidate_calls));
        diagnostics.candidate_successes = diagnostics
            .candidate_successes
            .saturating_add(u64::from(outcome.candidate_successes));
        if outcome.caret_positioned {
            diagnostics.temporary_caret_placements =
                diagnostics.temporary_caret_placements.saturating_add(1);
        }
        let releases = u64::from(outcome.caret_released) + u64::from(released_on_error);
        if releases > 0 {
            diagnostics.temporary_caret_releases = diagnostics
                .temporary_caret_releases
                .saturating_add(releases);
        }
        diagnostics.system_caret_owned = self.service.caret.is_owned();
        match result {
            Ok(()) => {
                diagnostics.applied = diagnostics.applied.saturating_add(1);
                diagnostics.last_applied_revision = Some(snapshot_revision);
                diagnostics.last_applied_epoch = Some(epoch);
                if outcome.applied_point.is_some() {
                    diagnostics.last_applied_point = outcome.applied_point;
                }
            }
            Err(error) => {
                if outcome.failure == Some(NativeImeFailure::Cancelled) {
                    diagnostics.cancelled = diagnostics.cancelled.saturating_add(1);
                } else {
                    diagnostics.refused = diagnostics.refused.saturating_add(1);
                }
                diagnostics.last_error = Some(error);
                if outcome.failure.is_some() {
                    diagnostics.last_failure = outcome.failure;
                }
            }
        }
    }
}

impl Drop for NativeImeController {
    fn drop(&mut self) {
        let _ = self.service.caret.release();
    }
}

/// Thread-scoped 1x1 temporary system caret matching
/// `CefSharp.Wpf/Experimental/WpfIMEKeyboardHandler.cs:334-444`. Created only on
/// active CJK compositions on the owner UI thread and released when
/// composition ends, is invalidated, loses focus, switches layout, or drops.
#[derive(Clone)]
struct TemporaryCaret {
    hwnd: Handle,
    thread: u32,
    owned: Rc<Cell<bool>>,
}

impl TemporaryCaret {
    fn new(hwnd: Handle, thread: u32) -> Self {
        Self {
            hwnd,
            thread,
            owned: Rc::new(Cell::new(false)),
        }
    }

    fn is_owned(&self) -> bool {
        self.owned.get()
    }

    fn position(&self, x: i32, y: i32) -> (bool, bool) {
        if self.hwnd.is_null()
            || unsafe { GetCurrentThreadId() } != self.thread
            || unsafe { IsWindow(self.hwnd) } == 0
        {
            return (false, false);
        }
        let mut created_now = false;
        if !self.owned.get() {
            if unsafe { CreateCaret(self.hwnd, std::ptr::null_mut(), 1, 1) } == 0 {
                return (false, false);
            }
            self.owned.set(true);
            created_now = true;
        }
        let positioned_ok = unsafe { SetCaretPos(x, y) } != 0;
        (created_now, positioned_ok)
    }

    fn release(&self) -> bool {
        if !self.owned.get() || unsafe { GetCurrentThreadId() } != self.thread {
            return false;
        }
        self.owned.set(false);
        unsafe {
            let _ = DestroyCaret();
        }
        true
    }
}

/// Local mutable `NativeViewServices` implementation.
///
/// Cloning shares only `Cell<PlacementOutcome>` and `TemporaryCaret` ownership;
/// there is no borrow guard to carry across FFI. Every query re-validates the
/// original owner thread, process and HWND.
#[derive(Clone)]
struct WindowsImeService {
    token: WindowToken,
    gate: Rc<CancelGate>,
    projection: Option<PlacementScope>,
    binding: WindowBinding,
    hwnd: Handle,
    thread: u32,
    process: u32,
    caret: TemporaryCaret,
    outcome: Rc<Cell<PlacementOutcome>>,
}

#[derive(Clone, Copy)]
struct PlacementScope {
    generation: u64,
    projected_scale: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlacementDpi(u32);

impl PlacementDpi {
    fn qualify(projected_scale: f32, observed: u32) -> Result<Self, NativeImeFailure> {
        if observed == 0 {
            return Err(NativeImeFailure::DpiUnavailable);
        }
        let cut = Self(observed);
        // GPUI's pinned Windows backend uses the same f32 DPI / 96 conversion.
        // Do not infer eligibility from a rounded pixel size or a nearby scale.
        if projected_scale != cut.scale() {
            return Err(NativeImeFailure::ProjectionUnavailable);
        }
        Ok(cut)
    }

    fn scale(self) -> f32 {
        self.0 as f32 / 96.0
    }

    fn verify(self, observed: u32) -> Result<(), NativeImeFailure> {
        match observed {
            0 => Err(NativeImeFailure::DpiUnavailable),
            dpi if self.0 == dpi => Ok(()),
            _ => Err(NativeImeFailure::ProjectionUnavailable),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PlacementOutcome {
    context_attempted: bool,
    context_acquired: bool,
    release_attempted: bool,
    release_ok: bool,
    composition_called: bool,
    composition_ok: bool,
    candidate_calls: u8,
    candidate_successes: u8,
    /// Exact client-pixel origin actually sent to IMM for this placement. The
    /// owner window's move is observable here, so a recomputed geometry after
    /// a real window move can be compared instead of assumed.
    applied_point: Option<(i32, i32)>,
    caret_positioned: bool,
    caret_released: bool,
    failure: Option<NativeImeFailure>,
}

impl PlacementOutcome {
    fn check_dpi<T>(&mut self, result: Result<T, NativeImeFailure>) -> Result<T, EngineError> {
        result.map_err(|failure| {
            self.failure = Some(failure);
            EngineError::NotReady
        })
    }
}

impl WindowsImeService {
    fn new(
        token: WindowToken,
        gate: Rc<CancelGate>,
        binding: WindowBinding,
        hwnd: Handle,
        thread: u32,
        process: u32,
    ) -> Self {
        Self {
            token,
            gate,
            projection: None,
            binding,
            hwnd,
            thread,
            process,
            caret: TemporaryCaret::new(hwnd, thread),
            outcome: Rc::new(Cell::new(PlacementOutcome::default())),
        }
    }

    fn outcome(&self) -> PlacementOutcome {
        self.outcome.get()
    }

    /// The single validity predicate shared by `is_valid` and `position_ime`.
    /// It never touches IME state and never activates the window.
    fn check(&self, binding: WindowBinding) -> Result<(), NativeImeFailure> {
        if binding != self.binding {
            return Err(NativeImeFailure::BindingMismatch);
        }
        if !binding_current(&self.token, binding.window) {
            return Err(NativeImeFailure::RetiredBinding);
        }
        if self
            .projection
            .is_some_and(|projection| !self.gate.permits(projection.generation))
        {
            return Err(NativeImeFailure::Cancelled);
        }
        if self.hwnd.is_null() {
            return Err(NativeImeFailure::InvalidWindow);
        }
        if unsafe { GetCurrentThreadId() } != self.thread {
            return Err(NativeImeFailure::WrongThread);
        }
        if unsafe { GetCurrentProcessId() } != self.process {
            return Err(NativeImeFailure::WrongProcess);
        }
        if unsafe { IsWindow(self.hwnd) } == 0 {
            return Err(NativeImeFailure::InvalidWindow);
        }
        let mut process = 0u32;
        if unsafe { GetWindowThreadProcessId(self.hwnd, &mut process) } != self.thread
            || process != self.process
        {
            return Err(NativeImeFailure::InvalidWindow);
        }
        Ok(())
    }

    fn ready(
        &self,
        binding: WindowBinding,
        outcome: &mut PlacementOutcome,
    ) -> Result<(), EngineError> {
        let failure = self.check(binding).err().or_else(|| {
            (unsafe { GetFocus() } != self.hwnd || unsafe { GetForegroundWindow() } != self.hwnd)
                .then_some(NativeImeFailure::FocusNotOwned)
        });
        match failure {
            None => Ok(()),
            Some(failure) => {
                outcome.failure = Some(failure);
                Err(if failure == NativeImeFailure::WrongThread {
                    EngineError::WrongThread
                } else {
                    EngineError::NotReady
                })
            }
        }
    }

    fn ready_at_dpi(
        &self,
        binding: WindowBinding,
        dpi: PlacementDpi,
        outcome: &mut PlacementOutcome,
    ) -> Result<(), EngineError> {
        self.ready(binding, outcome)?;
        outcome.check_dpi(dpi.verify(unsafe { GetDpiForWindow(self.hwnd) }))
    }

    fn place(
        &self,
        binding: WindowBinding,
        caret: ImeGeometry,
        outcome: &mut PlacementOutcome,
    ) -> Result<(), EngineError> {
        self.ready(binding, outcome)?;
        let Some(projection) = self.projection else {
            outcome.failure = Some(NativeImeFailure::ProjectionUnavailable);
            return Err(EngineError::NotReady);
        };
        let dpi = outcome.check_dpi(PlacementDpi::qualify(projection.projected_scale, unsafe {
            GetDpiForWindow(self.hwnd)
        }))?;
        let pixels = match scale_geometry(caret, dpi.scale()) {
            Ok(pixels) => pixels,
            Err(_) => {
                outcome.failure = Some(NativeImeFailure::GeometryRejected);
                return Err(EngineError::InvalidRequest);
            }
        };

        // RAII owns the HIMC: every early return below still releases it, and
        // the BOOL result is recorded rather than assumed.
        let release = Cell::new(ReleaseState::default());
        outcome.context_attempted = true;
        let Some(context) = ImmContext::acquire(self.hwnd, &release) else {
            outcome.failure = Some(NativeImeFailure::NoImmContext);
            return Err(EngineError::NotReady);
        };
        outcome.context_acquired = true;

        let composition = CompositionForm {
            style: CFS_POINT,
            point: Point {
                x: pixels.left,
                y: pixels.top,
            },
            area: Rect::default(),
        };
        let positioned = (|| {
            // Each IMM call may reenter. Do not reuse the old physical forms
            // after an observed DPI change. Release the HIMC even if a later
            // call is refused; earlier BOOL results remain individual facts.
            self.ready_at_dpi(binding, dpi, outcome)?;
            outcome.composition_called = true;
            outcome.composition_ok =
                unsafe { ImmSetCompositionWindow(context.raw(), &composition) } != 0;
            if outcome.composition_ok {
                outcome.applied_point = Some((pixels.left, pixels.top));
            }
            for candidate in candidate_forms(pixels) {
                self.ready_at_dpi(binding, dpi, outcome)?;
                outcome.candidate_calls += 1;
                outcome.candidate_successes +=
                    u8::from(unsafe { ImmSetCandidateWindow(context.raw(), &candidate) } != 0);
            }
            Ok::<(), EngineError>(())
        })();
        drop(context);
        let release_state = release.get();
        outcome.release_attempted = release_state.attempted;
        outcome.release_ok = release_state.ok;

        // IMM booleans have no documented error-code contract, so the typed
        // `Backend` error intentionally carries `code: None` rather than a
        // guessed `GetLastError` value. A failed `ImmReleaseContext` does not
        // undo accepted positions; individual call-success counters retain
        // those facts, but the overall service result must report the failure.
        positioned?;
        if !outcome.composition_ok {
            outcome.failure = Some(NativeImeFailure::CompositionFailed);
            return Err(EngineError::Backend { code: None });
        }
        if outcome.candidate_calls != 2 || outcome.candidate_successes != 2 {
            outcome.failure = Some(NativeImeFailure::CandidateFailed);
            return Err(EngineError::Backend { code: None });
        }
        if !release_state.ok {
            outcome.failure = Some(NativeImeFailure::ContextReleaseFailed);
            return Err(EngineError::Backend { code: None });
        }
        // A change during the final setter or release is not a current applied
        // placement, even when all three setters returned success.
        self.ready_at_dpi(binding, dpi, outcome)?;

        // Match `CefSharp.Wpf/Experimental/WpfIMEKeyboardHandler.cs:334-444`:
        // legacy non-TSF Chinese/Japanese IMEs read `GetCaretPos()` instead of
        // `ImmSetCandidateWindow()`. Keep the 1x1 caret strictly scoped to
        // active CJK compositions on the focused owned HWND; best-effort only.
        let hkl = unsafe { GetKeyboardLayout(0) };
        if layout_uses_temporary_caret(hkl) {
            let (_created, positioned) = self.caret.position(pixels.left, pixels.bottom);
            outcome.caret_positioned = positioned;
        } else if self.caret.release() {
            outcome.caret_released = true;
        }
        Ok(())
    }
}

impl NativeViewServices for WindowsImeService {
    fn is_valid(&self, binding: WindowBinding) -> bool {
        self.check(binding).is_ok()
    }

    fn position_ime(
        &mut self,
        binding: WindowBinding,
        caret: ImeGeometry,
    ) -> Result<(), EngineError> {
        let mut outcome = PlacementOutcome::default();
        let result = self.place(binding, caret, &mut outcome);
        // Publishing after the call keeps the Cell write outside every FFI
        // scope and outside any borrow of the controller's diagnostics.
        self.outcome.set(outcome);
        result
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ReleaseState {
    attempted: bool,
    ok: bool,
}

/// RAII HIMC. Never stored, never crosses threads, and never passed back to IMM
/// after drop.
struct ImmContext<'a> {
    hwnd: Handle,
    context: Handle,
    release: &'a Cell<ReleaseState>,
}

impl<'a> ImmContext<'a> {
    fn acquire(hwnd: Handle, release: &'a Cell<ReleaseState>) -> Option<Self> {
        let mut context = unsafe { ImmGetContext(hwnd) };
        if context.is_null() {
            // A fresh GPUI window has no IMM context until one is associated.
            // IACE_DEFAULT installs the thread's default IME context; retry once
            // and never create a private context that could leak.
            const IACE_DEFAULT: u32 = 0x0010;
            unsafe { ImmAssociateContextEx(hwnd, std::ptr::null_mut(), IACE_DEFAULT) };
            context = unsafe { ImmGetContext(hwnd) };
        }
        if context.is_null() {
            None
        } else {
            Some(Self {
                hwnd,
                context,
                release,
            })
        }
    }

    fn raw(&self) -> Handle {
        self.context
    }
}

impl Drop for ImmContext<'_> {
    fn drop(&mut self) {
        // The caller's HWND is the same one the context came from; a failed
        // release is recorded, never retried and never hidden.
        let ok = unsafe { ImmReleaseContext(self.hwnd, self.context) } != 0;
        self.release.set(ReleaseState {
            attempted: true,
            ok,
        });
    }
}

/// Client-area DIP rectangle converted to client physical pixels.
///
/// The caller-provided [`ImeGeometry`] already includes the surface origin; this
/// conversion only multiplies by the current window scale. It rejects
/// non-finite values, non-positive width/height, non-positive scale and any
/// result outside `i32`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ImePixels {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

// CANDIDATEFORM.dwStyle accepts one listed value, not their bitwise OR.
// As in the pinned CEF sample, send the position and exclusion separately.
fn candidate_forms(pixels: ImePixels) -> [CandidateForm; 2] {
    let point = Point {
        x: pixels.left,
        y: pixels.bottom,
    };
    [
        CandidateForm {
            index: 0,
            style: CFS_CANDIDATEPOS,
            point,
            area: Rect::default(),
        },
        CandidateForm {
            index: 0,
            style: CFS_EXCLUDE,
            point,
            area: Rect {
                left: pixels.left,
                top: pixels.top,
                right: pixels.right,
                bottom: pixels.bottom,
            },
        },
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GeometryRefusal {
    NonFinite,
    NonPositive,
    Scale,
    OutOfBounds,
    EmptyPixels,
}

fn scale_geometry(geometry: ImeGeometry, scale: f32) -> Result<ImePixels, GeometryRefusal> {
    if !geometry.x.is_finite()
        || !geometry.y.is_finite()
        || !geometry.width.is_finite()
        || !geometry.height.is_finite()
        || !scale.is_finite()
    {
        return Err(GeometryRefusal::NonFinite);
    }
    if geometry.width <= 0.0 || geometry.height <= 0.0 {
        return Err(GeometryRefusal::NonPositive);
    }
    if scale <= 0.0 {
        return Err(GeometryRefusal::Scale);
    }
    let scale = f64::from(scale);
    let left = checked_px(f64::from(geometry.x) * scale).ok_or(GeometryRefusal::OutOfBounds)?;
    let top = checked_px(f64::from(geometry.y) * scale).ok_or(GeometryRefusal::OutOfBounds)?;
    let right = checked_px((f64::from(geometry.x) + f64::from(geometry.width)) * scale)
        .ok_or(GeometryRefusal::OutOfBounds)?;
    let bottom = checked_px((f64::from(geometry.y) + f64::from(geometry.height)) * scale)
        .ok_or(GeometryRefusal::OutOfBounds)?;
    if right <= left || bottom <= top {
        return Err(GeometryRefusal::EmptyPixels);
    }
    Ok(ImePixels {
        left,
        top,
        right,
        bottom,
    })
}

fn checked_px(value: f64) -> Option<i32> {
    let rounded = value.round();
    if rounded.is_finite() && rounded >= i32::MIN as f64 && rounded <= i32::MAX as f64 {
        Some(rounded as i32)
    } else {
        None
    }
}

/// Cheap pre-scale shape check at enqueue time. Scale-dependent bounds are
/// enforced only at execution time, against the then-current DPI.
fn geometry_shape_ok(geometry: ImeGeometry) -> bool {
    geometry.x.is_finite()
        && geometry.y.is_finite()
        && geometry.width.is_finite()
        && geometry.height.is_finite()
        && geometry.width > 0.0
        && geometry.height > 0.0
}

/// A token's binding must still be live and bound to this exact window id.
fn binding_current(token: &WindowToken, window_id: u64) -> bool {
    token.is_live() && token.matches_window(window_id)
}

/// A lower snapshot revision than one already accepted is stale. Equal
/// revisions are allowed: callers may report several geometries for one CEF
/// snapshot revision, and the controller keeps only the latest.
fn revision_acceptable(snapshot_revision: u64, latest: Option<u64>) -> bool {
    latest.is_none_or(|latest| snapshot_revision >= latest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::contracts::{BrowserId, RuntimeGeneration};

    fn binding(window: u64) -> WindowBinding {
        WindowBinding {
            browser: BrowserId::new(RuntimeGeneration::new(7), 3),
            window,
        }
    }

    fn caret() -> ImeGeometry {
        ImeGeometry {
            x: 10.0,
            y: 20.0,
            width: 2.0,
            height: 18.0,
        }
    }

    // Pure geometry regression: the supplied client DIP already contains the
    // surface origin, so the only conversion is one current-scale multiply.
    #[test]
    fn client_dip_scales_once_without_an_extra_origin() {
        let forms = candidate_forms(scale_geometry(caret(), 1.5).unwrap());
        assert_eq!(forms[0].style, CFS_CANDIDATEPOS);
        assert_eq!(forms[1].style, CFS_EXCLUDE);
        assert_eq!((forms[0].point.x, forms[0].point.y), (15, 57));
        assert_eq!((forms[1].area.right, forms[1].area.bottom), (18, 57));
        assert_eq!(
            scale_geometry(caret(), 1.5),
            Ok(ImePixels {
                left: 15,
                top: 30,
                right: 18,
                bottom: 57,
            })
        );
        assert_eq!(
            scale_geometry(caret(), 1.0),
            Ok(ImePixels {
                left: 10,
                top: 20,
                right: 12,
                bottom: 38,
            })
        );
    }

    #[test]
    fn dpi_qualifies_only_the_exact_projected_scale() {
        for dpi in [96, 106, 120, 144, 168, 192] {
            let scale = dpi as f32 / 96.0;
            assert_eq!(PlacementDpi::qualify(scale, dpi), Ok(PlacementDpi(dpi)));
            let adjacent = f32::from_bits(scale.to_bits() + 1);
            assert_eq!(
                PlacementDpi::qualify(adjacent, dpi),
                Err(NativeImeFailure::ProjectionUnavailable)
            );
        }
        assert_eq!(
            PlacementDpi::qualify(1.0, 144),
            Err(NativeImeFailure::ProjectionUnavailable)
        );
        assert_eq!(
            PlacementDpi::qualify(1.5, 96),
            Err(NativeImeFailure::ProjectionUnavailable)
        );
    }

    #[test]
    fn dpi_unknown_and_changed_are_distinct_refusals() {
        let cut = PlacementDpi::qualify(1.5, 144).unwrap();
        let mut outcome = PlacementOutcome::default();
        assert_eq!(outcome.check_dpi(cut.verify(144)), Ok(()));
        assert_eq!(outcome.failure, None);
        assert_eq!(
            outcome.check_dpi(cut.verify(96)),
            Err(EngineError::NotReady)
        );
        assert_eq!(
            outcome.failure,
            Some(NativeImeFailure::ProjectionUnavailable)
        );
        assert_eq!(outcome.check_dpi(cut.verify(0)), Err(EngineError::NotReady));
        assert_eq!(outcome.failure, Some(NativeImeFailure::DpiUnavailable));
        assert_eq!(
            PlacementDpi::qualify(1.5, 0),
            Err(NativeImeFailure::DpiUnavailable)
        );
    }

    #[test]
    fn invalid_or_overflowing_geometry_is_rejected() {
        let mut invalid = caret();
        invalid.x = f32::NAN;
        assert_eq!(
            scale_geometry(invalid, 1.0),
            Err(GeometryRefusal::NonFinite)
        );
        assert_eq!(
            scale_geometry(caret(), f32::NAN),
            Err(GeometryRefusal::NonFinite)
        );
        assert_eq!(scale_geometry(caret(), 0.0), Err(GeometryRefusal::Scale));
        assert_eq!(scale_geometry(caret(), -1.0), Err(GeometryRefusal::Scale));

        let mut zero_width = caret();
        zero_width.width = 0.0;
        assert_eq!(
            scale_geometry(zero_width, 1.0),
            Err(GeometryRefusal::NonPositive)
        );
        let mut negative_height = caret();
        negative_height.height = -1.0;
        assert_eq!(
            scale_geometry(negative_height, 1.0),
            Err(GeometryRefusal::NonPositive)
        );

        let mut huge = caret();
        huge.x = 2.0e9;
        assert_eq!(scale_geometry(huge, 2.0), Err(GeometryRefusal::OutOfBounds));

        let mut subpixel = caret();
        subpixel.x = 0.0;
        subpixel.width = 0.01;
        assert_eq!(
            scale_geometry(subpixel, 1.0),
            Err(GeometryRefusal::EmptyPixels)
        );
    }

    // Pure projection admission: real token cell, shape gate, monotonic snapshot
    // revision and the production cancel generation. No GPUI or Win32 success is
    // simulated, and no parallel fake stands in for the real gate.
    #[test]
    fn projection_admission_requires_live_token_shape_monotonic_revision_and_cancel_generation() {
        let token = WindowToken::new(binding(41));
        assert!(binding_current(&token, 41));
        assert!(!binding_current(&token, 42));

        assert!(geometry_shape_ok(caret()));
        let mut zero = caret();
        zero.width = 0.0;
        assert!(!geometry_shape_ok(zero));
        let mut non_finite = caret();
        non_finite.y = f32::INFINITY;
        assert!(!geometry_shape_ok(non_finite));

        assert!(revision_acceptable(5, None));
        assert!(revision_acceptable(5, Some(5)));
        assert!(!revision_acceptable(4, Some(5)));
        assert!(revision_acceptable(6, Some(5)));

        // `invalidate` is transient: it cancels the queued generation, so an
        // already armed defer is stale, while a later queue still works. Only
        // checked overflow closes the gate.
        let gate = CancelGate::new();
        let first = gate.current().expect("a new gate is open");
        let second = gate.bump().expect("one invalidation keeps it open");
        assert_ne!(first, second);
        assert!(!gate.permits(first), "the superseded projection is stale");
        assert!(gate.permits(second), "a later queue is still admitted");

        let exhaust = CancelGate {
            generation: Cell::new(Some(u64::MAX)),
        };
        assert_eq!(exhaust.bump(), None, "overflow closes the gate");
        assert_eq!(exhaust.current(), None);
        assert_eq!(exhaust.bump(), None, "closed never revives");
        assert!(!exhaust.permits(u64::MAX));

        assert!(token.retire(), "retirement is the other permanent refusal");
        assert!(
            !binding_current(&token, 41),
            "a retired token is never current"
        );
    }

    #[test]
    fn real_hwnd_imm_placement_writes_and_reads_back_composition_and_candidate_forms() {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetModuleHandleW(lp_module_name: *const u16) -> Handle;
        }
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
            fn DestroyWindow(hwnd: Handle) -> i32;
        }
        #[link(name = "imm32")]
        unsafe extern "system" {
            fn ImmGetCompositionWindow(context: Handle, form: *mut CompositionForm) -> i32;
            fn ImmGetCandidateWindow(context: Handle, index: u32, form: *mut CandidateForm) -> i32;
        }

        let class_name: Vec<u16> = "STATIC".encode_utf16().chain(Some(0)).collect();
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                std::ptr::null(),
                0x8000_0000, // WS_POPUP
                0,
                0,
                320,
                240,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null_mut(),
            )
        };
        assert!(!hwnd.is_null(), "test HWND must be created");

        let dpi = unsafe { GetDpiForWindow(hwnd) };
        assert!(dpi >= 96, "real HWND must report a valid DPI: {dpi}");
        let scale = dpi as f32 / 96.0;
        let qualified = PlacementDpi::qualify(scale, dpi).expect("current HWND DPI qualifies");
        let pixels = scale_geometry(caret(), qualified.scale()).expect("geometry scales cleanly");

        let release = Cell::new(ReleaseState::default());
        let ctx = ImmContext::acquire(hwnd, &release).expect("real HWND acquires IMM context");
        let comp_in = CompositionForm {
            style: CFS_POINT,
            point: Point {
                x: pixels.left,
                y: pixels.top,
            },
            area: Rect::default(),
        };
        assert_ne!(
            unsafe { ImmSetCompositionWindow(ctx.raw(), &comp_in) },
            0,
            "ImmSetCompositionWindow must succeed on real HWND"
        );
        let mut comp_out = CompositionForm::default();
        assert_ne!(
            unsafe { ImmGetCompositionWindow(ctx.raw(), &mut comp_out) },
            0,
            "ImmGetCompositionWindow readback must succeed"
        );
        assert_eq!(comp_out.style, CFS_POINT);
        assert_eq!(
            (comp_out.point.x, comp_out.point.y),
            (pixels.left, pixels.top)
        );

        let forms = candidate_forms(pixels);
        for form in &forms {
            assert_ne!(
                unsafe { ImmSetCandidateWindow(ctx.raw(), form) },
                0,
                "ImmSetCandidateWindow(style={}) must succeed",
                form.style
            );
        }
        let mut cand_out = CandidateForm::default();
        assert_ne!(
            unsafe { ImmGetCandidateWindow(ctx.raw(), 0, &mut cand_out) },
            0,
            "ImmGetCandidateWindow readback must succeed"
        );
        assert_eq!(cand_out.style, CFS_EXCLUDE);
        assert_eq!(
            (cand_out.point.x, cand_out.point.y),
            (pixels.left, pixels.bottom)
        );
        assert_eq!(
            (
                cand_out.area.left,
                cand_out.area.top,
                cand_out.area.right,
                cand_out.area.bottom
            ),
            (pixels.left, pixels.top, pixels.right, pixels.bottom)
        );

        drop(ctx);
        assert_eq!(
            release.get(),
            ReleaseState {
                attempted: true,
                ok: true
            }
        );
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }

    #[test]
    fn primary_lang_id_selects_only_chinese_and_japanese_layouts() {
        assert_eq!(primary_lang_id(0x0804_0804usize as Handle), LANG_CHINESE);
        assert!(layout_uses_temporary_caret(0x0804_0804usize as Handle));
        assert!(layout_uses_temporary_caret(0x0404_0404usize as Handle));
        assert!(layout_uses_temporary_caret(0x0411_0411usize as Handle));
        assert!(!layout_uses_temporary_caret(0x0409_0409usize as Handle));
        assert!(!layout_uses_temporary_caret(0x0412_0412usize as Handle));
        assert!(!layout_uses_temporary_caret(std::ptr::null_mut()));
    }

    #[test]
    fn real_hwnd_temporary_caret_creates_positions_reads_back_and_destroys_without_leak() {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetModuleHandleW(lp_module_name: *const u16) -> Handle;
        }
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
            fn DestroyWindow(hwnd: Handle) -> i32;
            fn GetCaretPos(point: *mut Point) -> i32;
        }

        let class_name: Vec<u16> = "STATIC".encode_utf16().chain(Some(0)).collect();
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                std::ptr::null(),
                0x8000_0000, // WS_POPUP
                0,
                0,
                320,
                240,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null_mut(),
            )
        };
        assert!(!hwnd.is_null(), "test HWND must be created");

        let thread = unsafe { GetCurrentThreadId() };
        let pixels = scale_geometry(caret(), 1.5).unwrap();
        let temp_caret = TemporaryCaret::new(hwnd, thread);
        assert!(!temp_caret.is_owned());

        let (created, positioned) = temp_caret.position(pixels.left, pixels.bottom);
        assert!(created, "CreateCaret must succeed on owned real HWND");
        assert!(positioned, "SetCaretPos must succeed on owned real HWND");
        assert!(temp_caret.is_owned());

        let mut pt = Point::default();
        assert_ne!(
            unsafe { GetCaretPos(&mut pt) },
            0,
            "GetCaretPos readback must succeed while temporary caret is owned"
        );
        assert_eq!((pt.x, pt.y), (pixels.left, pixels.bottom));

        let (created_again, positioned_again) = temp_caret.position(pixels.right, pixels.top);
        assert!(!created_again, "existing temporary caret is reused");
        assert!(positioned_again);
        assert_ne!(unsafe { GetCaretPos(&mut pt) }, 0);
        assert_eq!((pt.x, pt.y), (pixels.right, pixels.top));

        assert!(temp_caret.release(), "release must destroy owned caret");
        assert!(!temp_caret.is_owned());
        assert!(!temp_caret.release(), "release must be idempotent");

        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}
