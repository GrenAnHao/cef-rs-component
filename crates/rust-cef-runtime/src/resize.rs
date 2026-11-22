//! Runtime-owned resize recovery scheduling.
//!
//! This module is pure owned state: it never calls CEF, Win32 or the registry,
//! and stores no native reference. The Runtime posts a delayed task carrying a
//! [`ResizeTicket`] on the CEF UI sequence and hands that ticket back to
//! [`ResizeState::begin_tick`]. Only the returned [`ResizeTick`] decides whether
//! the caller may issue `Invalidate(VIEW)`; Native/popup work stays in its own
//! registry path.
//!
//! The bounded refresh burst itself is the original core [`ResizeRecovery`]:
//! at most 8 requests, at least 100 ms apart, a new target keeps the cooldown,
//! and hiding never resets it. This module adds the identity/generation gate,
//! one queued ticket per Browser, the final empty "probe" tick that observes
//! exhaustion without issuing a 9th request, and pure diagnostics.

use std::{
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use rust_cef_core::{
    ports::{FrameInfo, SurfaceKind},
    resize_recovery::ResizeRecovery,
};
use serde::Serialize;

/// Opaque, monotonic, never-zero recovery task ticket. A stale ticket cannot
/// clear or consume a newer one; `u64::MAX` stops issuing tickets instead of
/// wrapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResizeTicket(u64);

/// What one delayed UI task is allowed to do. The caller issues
/// `Invalidate(VIEW)` only when `request` is true, and posts exactly one
/// follow-up task (100 ms later) when `needs_another` is true.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResizeTick {
    pub request: bool,
    /// Set exactly once per target when the 8-request budget is spent and the
    /// last request's response window has elapsed without a complete frame.
    pub exhausted: bool,
    /// The target is still pending and another 100 ms probe/retry is useful.
    pub needs_another: bool,
}

/// Pure snapshot for probes and error reporting. It contains no native handle,
/// pixel bytes or scheduler state.
#[derive(Clone, Debug, Serialize)]
pub struct FrameRecoveryDiagnostics {
    /// `Idle`, `Pending`, `Recovered`, `Hidden`, `Closed` or `Exhausted`.
    pub status: &'static str,
    pub generation: u64,
    /// Last complete VIEW target in device pixels; `0` before any valid target.
    pub expected_width: u32,
    pub expected_height: u32,
    /// Requests granted for the current target; `0` after a hide, close or a
    /// new target, and retained through the last ack/exhaustion for post-mortem
    /// probing.
    pub attempts: u8,
    pub requests_total: u64,
    /// Audited entries immediately preceding the actual UI Invalidate call,
    /// separate from core retry grants (which alone are not native evidence).
    pub native_invalidate_entries: u64,
    pub native_thread: u64,
    /// At most eight offsets for the current target, retained on hide/close
    /// as historical diagnostics and cleared when a new target is armed.
    pub native_offsets_us: Vec<u64>,
    pub acks: u64,
    pub exhaustions: u64,
    pub post_failures: u64,
    pub queued: bool,
    /// Up to 8 granted-request offsets from the current target start. They are
    /// monotonic and cleared for a new target.
    pub request_offsets_us: Vec<u64>,
    /// Minimum spacing between adjacent granted requests over the whole
    /// instance, across targets. The cooldown makes it at least 100 ms.
    pub min_spacing_us: Option<u64>,
    /// Test-support probe counters; only the owning test seam increments them.
    pub probe_dropped: u64,
    pub incomplete_rejected: u64,
}

impl Default for FrameRecoveryDiagnostics {
    fn default() -> Self {
        Self {
            status: "Idle",
            generation: 0,
            expected_width: 0,
            expected_height: 0,
            attempts: 0,
            requests_total: 0,
            native_invalidate_entries: 0,
            native_thread: 0,
            native_offsets_us: Vec::new(),
            acks: 0,
            exhaustions: 0,
            post_failures: 0,
            queued: false,
            request_offsets_us: Vec::new(),
            min_spacing_us: None,
            probe_dropped: 0,
            incomplete_rejected: 0,
        }
    }
}

/// One Browser's resize recovery state. Shared with the frame sink and the
/// registry entry; all mutation is internal synchronization and short.
pub(crate) struct ResizeState {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    recovery: ResizeRecovery,
    generation: u64,
    /// Last accepted complete target. Retained after ack/hide so diagnostics
    /// still identify what was requested and generation gating keeps meaning.
    last_dims: Option<(u32, u32)>,
    target_started: Option<Instant>,
    attempts: u8,
    requests_total: u64,
    native_invalidate_entries: u64,
    native_thread: u64,
    native_offsets_us: Vec<u64>,
    acks: u64,
    exhaustions: u64,
    post_failures: u64,
    probe_dropped: u64,
    incomplete_rejected: u64,
    /// The single delayed task outstanding for this Browser, if any.
    queued: Option<u64>,
    next_ticket: u64,
    last_request_at: Option<Instant>,
    min_spacing_us: Option<u64>,
    offsets_us: Vec<u64>,
    exhausted_once: bool,
    closed: bool,
    hidden: bool,
}

impl ResizeState {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Arms or preserves recovery for the current VIEW target.
    ///
    /// An unchanged target (same generation and complete pixel size) must not
    /// reset the 8-request budget, its offsets or the cooldown. A changed
    /// target keeps any already-queued ticket (no second timer) and the core
    /// cooldown, while restarting the per-target attempt/offset bookkeeping.
    pub(crate) fn target(
        &self,
        generation: u64,
        width: i32,
        height: i32,
        scale: f32,
        visible: bool,
        now: Instant,
    ) {
        let mut inner = self.lock();
        if inner.closed {
            return;
        }
        if !visible || width <= 1 || height <= 1 {
            inner.hide();
            return;
        }
        // Match the core's f32/ceil device-pixel geometry exactly.
        let dims = (
            (width as f32 * scale).ceil() as u32,
            (height as f32 * scale).ceil() as u32,
        );
        if !inner.hidden && inner.generation == generation && inner.last_dims == Some(dims) {
            // Same target: keep the budget, attempts, offsets, cooldown and any
            // queued ticket untouched.
            return;
        }
        inner.recovery.resized(width, height, scale, now);
        let Some(expected) = inner.recovery.expected() else {
            inner.hide();
            return;
        };
        if expected.0 == 0 || expected.1 == 0 {
            inner.hide();
            return;
        }
        inner.generation = generation;
        inner.last_dims = Some(expected);
        inner.target_started = Some(now);
        inner.attempts = 0;
        inner.offsets_us.clear();
        inner.native_offsets_us.clear();
        inner.exhausted_once = false;
        inner.hidden = false;
    }

    /// Cancels pending recovery. `closed` is sticky: a later `cancel(false)`
    /// from a queued stale tick can never downgrade `Closed` to `Hidden` or
    /// re-arm the Browser. The cooldown is retained by the core.
    pub(crate) fn cancel(&self, closed: bool) {
        let mut inner = self.lock();
        if closed {
            inner.closed = true;
        }
        inner.recovery.cancel();
        if !inner.closed {
            inner.hidden = true;
        }
        inner.attempts = 0;
        inner.target_started = None;
        inner.offsets_us.clear();
        inner.exhausted_once = false;
    }

    /// Acknowledges recovery only for a complete VIEW frame with the current
    /// generation and exact target size. POPUP frames, stale generations and
    /// partial frames never ack. The caller must invoke this after the real
    /// mailbox commit and before waking the host observer, so reentrancy cannot
    /// credit a frame that was not actually published.
    pub(crate) fn frame_received(&self, info: FrameInfo) -> bool {
        let mut inner = self.lock();
        if inner.closed || inner.hidden {
            return false;
        }
        if info.surface != SurfaceKind::View {
            return false;
        }
        if info.surface_generation != inner.generation {
            return false;
        }
        let Some(expected) = inner.recovery.expected() else {
            return false;
        };
        if (info.width, info.height) != expected {
            return false;
        }
        inner.recovery.frame_received(info.width, info.height);
        if inner.recovery.expected().is_some() {
            return false;
        }
        inner.acks = inner.acks.saturating_add(1);
        true
    }

    /// Issues the one queued delayed-task ticket for a pending target. Returns
    /// `None` while a ticket is already queued, after recovery, after close and
    /// once exhaustion has been reported. Before exhaustion is reported, an
    /// extra empty ticket is still issued after the 8th request so the final
    /// request has a full response window and exhaustion can be observed.
    pub(crate) fn schedule(&self) -> Option<ResizeTicket> {
        let mut inner = self.lock();
        if inner.closed || inner.hidden {
            return None;
        }
        if inner.recovery.expected().is_none() || inner.exhausted_once {
            return None;
        }
        if inner.queued.is_some() {
            return None;
        }
        let id = inner.next_ticket.checked_add(1)?;
        inner.next_ticket = id;
        inner.queued = Some(id);
        Some(ResizeTicket(id))
    }

    /// A delayed post was rejected. Only the matching ticket is disarmed; an
    /// old failure cannot clear a newer queued ticket. The caller may schedule
    /// again from a later target/ready transition.
    pub(crate) fn post_cancelled(&self, ticket: ResizeTicket) {
        let mut inner = self.lock();
        if inner.queued == Some(ticket.0) {
            inner.queued = None;
        }
    }

    pub(crate) fn post_failed(&self, ticket: ResizeTicket) {
        let mut inner = self.lock();
        if inner.queued == Some(ticket.0) {
            inner.queued = None;
            inner.post_failures = inner.post_failures.saturating_add(1);
        }
    }

    /// Consumes the matching queued ticket and decides this tick's effect.
    ///
    /// A request is granted only through the core cooldown/budget. The 8th
    /// grant still reports `needs_another = true` so the next tick can give the
    /// final request its 100 ms response window. Only a later tick, with the
    /// target still pending, zero remaining requests and an elapsed cooldown,
    /// reports `exhausted = true` exactly once and stops scheduling. A late
    /// complete frame may still acknowledge and recover.
    pub(crate) fn begin_tick(&self, ticket: ResizeTicket, now: Instant) -> ResizeTick {
        let mut inner = self.lock();
        if inner.queued != Some(ticket.0) {
            return ResizeTick::default();
        }
        inner.queued = None;
        if inner.closed || inner.hidden || inner.exhausted_once {
            return ResizeTick::default();
        }
        if inner.recovery.expected().is_none() {
            return ResizeTick::default();
        }

        let request = inner.recovery.take_request(now);
        if request {
            inner.attempts = inner.attempts.saturating_add(1);
            inner.requests_total = inner.requests_total.saturating_add(1);
            let offset = inner
                .target_started
                .map(|start| micros(now.saturating_duration_since(start)))
                .unwrap_or(0);
            // Guard against any non-monotonic caller-supplied `now`.
            let offset = inner
                .offsets_us
                .last()
                .copied()
                .map_or(offset, |last| offset.max(last));
            if inner.offsets_us.len() < 8 {
                inner.offsets_us.push(offset);
            }
            if let Some(previous) = inner.last_request_at {
                let spacing = micros(now.saturating_duration_since(previous));
                inner.min_spacing_us = Some(
                    inner
                        .min_spacing_us
                        .map_or(spacing, |minimum| minimum.min(spacing)),
                );
            }
            inner.last_request_at = Some(now);
        }

        let pending = inner.recovery.expected().is_some();
        if pending && inner.recovery.remaining() == 0 && inner.recovery.cooldown_elapsed(now) {
            inner.exhausted_once = true;
            inner.exhaustions = inner.exhaustions.saturating_add(1);
            return ResizeTick {
                request,
                exhausted: true,
                needs_another: false,
            };
        }
        ResizeTick {
            request,
            exhausted: false,
            needs_another: pending,
        }
    }

    pub(crate) fn note_invalidate(&self, now: Instant, thread: u64) {
        let mut inner = self.lock();
        inner.native_invalidate_entries = inner.native_invalidate_entries.saturating_add(1);
        inner.native_thread = thread;
        let offset = inner
            .target_started
            .map(|start| micros(now.saturating_duration_since(start)))
            .unwrap_or(0);
        if inner.native_offsets_us.len() < 8 {
            inner.native_offsets_us.push(offset);
        }
    }

    pub(crate) fn diagnostics(&self) -> FrameRecoveryDiagnostics {
        let inner = self.lock();
        let (expected_width, expected_height) = inner.last_dims.unwrap_or((0, 0));
        FrameRecoveryDiagnostics {
            status: status(&inner),
            generation: inner.generation,
            expected_width,
            expected_height,
            attempts: inner.attempts,
            requests_total: inner.requests_total,
            native_invalidate_entries: inner.native_invalidate_entries,
            native_thread: inner.native_thread,
            native_offsets_us: inner.native_offsets_us.clone(),
            acks: inner.acks,
            exhaustions: inner.exhaustions,
            post_failures: inner.post_failures,
            queued: inner.queued.is_some(),
            request_offsets_us: inner.offsets_us.clone(),
            min_spacing_us: inner.min_spacing_us,
            probe_dropped: inner.probe_dropped,
            incomplete_rejected: inner.incomplete_rejected,
        }
    }

    #[cfg_attr(not(feature = "test-support"), allow(dead_code))]
    pub(crate) fn note_probe_drop(&self) {
        let mut inner = self.lock();
        inner.probe_dropped = inner.probe_dropped.saturating_add(1);
    }

    #[cfg_attr(not(feature = "test-support"), allow(dead_code))]
    pub(crate) fn note_incomplete(&self) {
        let mut inner = self.lock();
        inner.incomplete_rejected = inner.incomplete_rejected.saturating_add(1);
    }
}

impl Inner {
    fn hide(&mut self) {
        self.recovery.cancel();
        self.hidden = true;
        self.attempts = 0;
        self.target_started = None;
        self.offsets_us.clear();
        self.exhausted_once = false;
    }
}

fn status(inner: &Inner) -> &'static str {
    if inner.closed {
        "Closed"
    } else if inner.hidden {
        "Hidden"
    } else if inner.recovery.expected().is_some() {
        if inner.exhausted_once {
            "Exhausted"
        } else {
            "Pending"
        }
    } else if inner.last_dims.is_some() {
        "Recovered"
    } else {
        "Idle"
    }
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::{
        contracts::{BrowserId, RuntimeGeneration},
        ports::PixelFormat,
    };

    fn base() -> Instant {
        Instant::now()
    }

    fn at(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    fn next_tick(state: &ResizeState, now: Instant) -> ResizeTick {
        let ticket = state.schedule().expect("a queued ticket is expected");
        state.begin_tick(ticket, now)
    }

    fn frame(generation: u64, width: u32, height: u32, surface: SurfaceKind) -> FrameInfo {
        FrameInfo {
            browser: BrowserId::new(RuntimeGeneration::new(1), 1),
            surface,
            surface_generation: generation,
            sequence: 1,
            width,
            height,
            format: PixelFormat::PremultipliedBgra8,
        }
    }

    #[test]
    fn diagnostics_default_is_idle() {
        let diagnostics = FrameRecoveryDiagnostics::default();
        assert_eq!(diagnostics.status, "Idle");
        assert!(!diagnostics.queued);
        assert!(diagnostics.request_offsets_us.is_empty());
        assert_eq!(diagnostics.min_spacing_us, None);
    }

    #[test]
    fn stale_ticket_and_old_post_failure_do_not_clear_the_queued_ticket() {
        let now = base();
        let state = ResizeState::new();
        state.target(1, 800, 600, 1.0, true, now);
        let first = state.schedule().expect("first ticket");
        assert!(state.begin_tick(first, now).request);

        let second = state.schedule().expect("second ticket");
        state.post_failed(first);
        // An unknown/old ticket cannot clear the current one either.
        state.post_failed(ResizeTicket(9_999));
        assert!(state.diagnostics().queued);

        // A stale tick is inert and does not consume the current ticket.
        assert_eq!(state.begin_tick(first, at(now, 100)), ResizeTick::default());
        assert!(state.diagnostics().queued);

        let tick = state.begin_tick(second, at(now, 100));
        assert!(tick.request);
        assert!(!state.diagnostics().queued);
        assert_eq!(state.diagnostics().attempts, 2);
    }

    #[test]
    fn hide_cancels_and_restore_rearms_without_resetting_the_cooldown() {
        let now = base();
        let state = ResizeState::new();
        state.target(1, 800, 600, 1.0, true, now);
        assert!(next_tick(&state, now).request);

        // Keep one real ticket queued, then hide. The queued task is preserved
        // until it actually runs; hiding never posts a second timer.
        let queued = state.schedule().expect("queued before hide");
        state.target(2, 800, 600, 1.0, false, at(now, 10));
        let hidden = state.diagnostics();
        assert_eq!(hidden.status, "Hidden");
        assert_eq!(hidden.attempts, 0);
        assert!(hidden.queued);
        assert!(state.schedule().is_none());

        // The preserved stale task runs while hidden and is inert.
        assert_eq!(state.begin_tick(queued, at(now, 10)), ResizeTick::default());
        assert!(!state.diagnostics().queued);

        // Restoring is a new generation but the same pixel target. It may
        // start a fresh 8-request budget, not bypass the 100 ms cooldown.
        state.target(3, 800, 600, 1.0, true, at(now, 20));
        let restored = state.diagnostics();
        assert_eq!(restored.status, "Pending");
        assert_eq!(restored.attempts, 0);
        assert_eq!(restored.generation, 3);

        let early = next_tick(&state, at(now, 99));
        assert!(!early.request);
        assert!(early.needs_another);
        let on_time = next_tick(&state, at(now, 100));
        assert!(on_time.request);
        assert_eq!(state.diagnostics().request_offsets_us, vec![80_000]);
    }

    #[test]
    fn wrong_generation_popup_and_partial_frames_never_ack() {
        let now = base();
        let state = ResizeState::new();
        state.target(5, 800, 600, 1.0, true, now);

        assert!(!state.frame_received(frame(5, 800, 600, SurfaceKind::Popup)));
        assert!(!state.frame_received(frame(4, 800, 600, SurfaceKind::View)));
        assert!(!state.frame_received(frame(5, 799, 600, SurfaceKind::View)));
        assert!(!state.frame_received(frame(5, 800, 599, SurfaceKind::View)));
        assert_eq!(state.diagnostics().status, "Pending");

        assert!(state.frame_received(frame(5, 800, 600, SurfaceKind::View)));
        // A second ack has no pending target and is rejected.
        assert!(!state.frame_received(frame(5, 800, 600, SurfaceKind::View)));
        let diagnostics = state.diagnostics();
        assert_eq!(diagnostics.status, "Recovered");
        assert_eq!(diagnostics.acks, 1);
        assert_eq!(diagnostics.expected_width, 800);
        assert_eq!(diagnostics.expected_height, 600);
        assert!(state.schedule().is_none());
    }

    #[test]
    fn hidpi_fractional_target_accepts_only_the_ceiled_size() {
        let now = base();
        let state = ResizeState::new();
        state.target(1, 100, 200, 1.1, true, now);
        assert_eq!(state.diagnostics().expected_width, 110);
        assert_eq!(state.diagnostics().expected_height, 220);
        assert!(!state.frame_received(frame(1, 110, 221, SurfaceKind::View)));
        assert!(state.frame_received(frame(1, 110, 220, SurfaceKind::View)));
    }

    #[test]
    fn unchanged_target_keeps_budget_while_same_pixels_new_generation_must_wait() {
        let now = base();
        let state = ResizeState::new();
        state.target(1, 800, 600, 1.0, true, now);
        assert!(next_tick(&state, now).request);
        assert!(next_tick(&state, at(now, 100)).request);
        let before = state.diagnostics();
        assert_eq!(before.attempts, 2);

        // Same generation and same complete size: no new budget, no reset.
        state.target(1, 800, 600, 1.0, true, at(now, 150));
        let unchanged = state.diagnostics();
        assert_eq!(unchanged.attempts, 2);
        assert_eq!(unchanged.request_offsets_us, before.request_offsets_us);

        // Same pixels, new generation: genuinely new target, but the cooldown
        // from the previous granted request is still enforced.
        state.target(2, 800, 600, 1.0, true, at(now, 160));
        let changed = state.diagnostics();
        assert_eq!(changed.status, "Pending");
        assert_eq!(changed.generation, 2);
        assert_eq!(changed.attempts, 0);
        assert!(changed.request_offsets_us.is_empty());

        let early = next_tick(&state, at(now, 160));
        assert!(!early.request, "a new target bypassed the cooldown");
        assert!(early.needs_another);
        assert!(next_tick(&state, at(now, 200)).request);
    }

    #[test]
    fn permanent_missing_frame_requests_exactly_eight_then_exhausts_once() {
        let now = base();
        let state = ResizeState::new();
        state.target(1, 800, 600, 1.0, true, now);

        let mut requests = 0u32;
        let mut exhaustions = 0u32;
        let mut step = 0u64;
        loop {
            let Some(ticket) = state.schedule() else {
                break;
            };
            let tick = state.begin_tick(ticket, at(now, step * 100));
            if tick.request {
                requests += 1;
            }
            if tick.exhausted {
                exhaustions += 1;
                assert!(!tick.needs_another);
                break;
            }
            // The 8th grant must still allow the final 100 ms response window.
            assert!(tick.needs_another);
            step += 1;
            assert!(step < 20, "recovery scheduling did not terminate");
        }

        assert_eq!(requests, 8);
        assert_eq!(exhaustions, 1);
        assert!(state.schedule().is_none(), "no 9th ticket may be posted");

        let diagnostics = state.diagnostics();
        assert_eq!(diagnostics.status, "Exhausted");
        assert_eq!(diagnostics.attempts, 8);
        assert_eq!(diagnostics.requests_total, 8);
        assert_eq!(diagnostics.exhaustions, 1);
        assert_eq!(diagnostics.request_offsets_us.len(), 8);
        assert_eq!(
            diagnostics.request_offsets_us,
            vec![
                0, 100_000, 200_000, 300_000, 400_000, 500_000, 600_000, 700_000
            ]
        );
        assert_eq!(diagnostics.min_spacing_us, Some(100_000));
    }

    #[test]
    fn eighth_request_gets_its_response_window_and_can_still_recover() {
        let now = base();
        let state = ResizeState::new();
        state.target(1, 800, 600, 1.0, true, now);

        for step in 0..8u64 {
            let tick = next_tick(&state, at(now, step * 100));
            assert!(tick.request);
            assert!(
                !tick.exhausted,
                "the 8th request must not be reported as exhausted immediately"
            );
            assert!(tick.needs_another);
        }
        let probe = state
            .schedule()
            .expect("a final empty probe tick is allowed");

        // A complete frame inside the response window recovers; the already
        // queued probe tick then does nothing and reports no exhaustion.
        assert!(state.frame_received(frame(1, 800, 600, SurfaceKind::View)));
        let tick = state.begin_tick(probe, at(now, 800));
        assert!(!tick.request);
        assert!(!tick.exhausted);
        assert!(!tick.needs_another);
        let diagnostics = state.diagnostics();
        assert_eq!(diagnostics.status, "Recovered");
        assert_eq!(diagnostics.exhaustions, 0);
        assert_eq!(diagnostics.acks, 1);
    }

    #[test]
    fn late_complete_frame_after_exhaustion_still_recovers() {
        let now = base();
        let state = ResizeState::new();
        state.target(1, 800, 600, 1.0, true, now);
        for step in 0..8u64 {
            assert!(next_tick(&state, at(now, step * 100)).request);
        }
        let exhaustion = next_tick(&state, at(now, 800));
        assert!(exhaustion.exhausted);
        assert_eq!(state.diagnostics().status, "Exhausted");

        assert!(state.frame_received(frame(1, 800, 600, SurfaceKind::View)));
        assert_eq!(state.diagnostics().status, "Recovered");
        assert!(state.schedule().is_none());
    }

    #[test]
    fn closed_is_sticky_and_cancel_false_cannot_revive_or_downgrade() {
        let now = base();
        let state = ResizeState::new();
        state.target(1, 800, 600, 1.0, true, now);
        assert!(next_tick(&state, now).request);
        state.cancel(true);
        assert_eq!(state.diagnostics().status, "Closed");

        // A stale tick path may call cancel(false); it must stay Closed.
        state.cancel(false);
        assert_eq!(state.diagnostics().status, "Closed");

        // A late target cannot re-arm a closed Browser.
        state.target(2, 800, 600, 1.0, true, at(now, 200));
        let diagnostics = state.diagnostics();
        assert_eq!(diagnostics.status, "Closed");
        assert_eq!(diagnostics.generation, 1);
        assert_eq!(diagnostics.attempts, 0);
        assert!(state.schedule().is_none());
        assert!(!state.frame_received(frame(2, 800, 600, SurfaceKind::View)));
    }

    #[test]
    fn probe_counters_are_reported_without_native_state() {
        let state = ResizeState::new();
        state.note_probe_drop();
        state.note_probe_drop();
        state.note_incomplete();
        let diagnostics = state.diagnostics();
        assert_eq!(diagnostics.probe_dropped, 2);
        assert_eq!(diagnostics.incomplete_rejected, 1);
        assert_eq!(diagnostics.status, "Idle");
    }

    #[test]
    fn post_failure_is_counted_and_allows_a_later_reschedule() {
        let now = base();
        let state = ResizeState::new();
        state.target(1, 800, 600, 1.0, true, now);
        let failed = state.schedule().expect("first ticket");
        state.post_failed(failed);
        assert!(!state.diagnostics().queued);
        assert_eq!(state.diagnostics().post_failures, 1);

        let cancelled = state.schedule().expect("shutdown-raced reservation");
        state.post_cancelled(failed); // A stale cancellation cannot clear it.
        assert!(state.diagnostics().queued);
        state.post_cancelled(cancelled);
        assert!(!state.diagnostics().queued);
        assert_eq!(state.diagnostics().post_failures, 1);
        let retry = state.schedule().expect("a later schedule may retry");
        assert!(state.begin_tick(retry, now).request);
    }
}
