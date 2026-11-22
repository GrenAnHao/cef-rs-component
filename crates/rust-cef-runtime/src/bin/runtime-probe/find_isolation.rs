//! Two real OSR Browsers, independent menu/result owners. The fixture sends
//! only existing CEF-local mouse input, never Win32 input or a late result.
use crate::find_validation::FindProbe;
use rust_cef_core::{
    contracts::{SessionState, ThreadMode},
    find::{FindOutcome, FindPhase, FindReceipt, FindResult},
    ports::{BrowserHandle, CloseMode, SurfaceKind},
};
use rust_cef_runtime::{
    find_probe_report::{FindIsolationReport, operation_key},
    runtime::Runtime,
    test_support::{FrameProbeControl, ProbeBrowserEvent},
};
use std::{
    num::NonZeroUsize,
    sync::Mutex,
    time::{Duration, Instant},
};

type Tick = fn(&mut Runtime, ThreadMode) -> Result<(), String>;

#[derive(Default)]
struct FirstClose {
    first: (u64, u64),
    survivor: (u64, u64),
    original_operation: [u64; 3],
    peer_boundary_operation: [u64; 3],
    counts: [u64; 3],
    generation: u64,
    frame_before: u64,
    frame_after: u64,
    source_closed: bool,
    result_preserved: bool,
}

pub(super) struct FindIsolationProbe {
    first: FindProbe,
    peer: FindProbe,
    first_close: Option<FirstClose>,
}

fn identity(handle: &BrowserHandle) -> (u64, u64) {
    let id = handle.snapshot().browser;
    (id.runtime().value(), id.local())
}

fn real_peer_result(receipt: &dyn FindReceipt) -> Result<FindResult, String> {
    let s = receipt.status();
    if s.outcome != Some(FindOutcome::Result)
        || s.phase != FindPhase::Finished
        || s.first_failure.is_some()
    {
        return Err("peer Find not successfully terminal".into());
    }
    s.last_result
        .filter(|r| r.final_update && r.operation == s.operation && r.matches == 1)
        .ok_or_else(|| "peer real final Find result missing".into())
}

impl FindIsolationProbe {
    pub fn new() -> Self {
        Self {
            first: FindProbe::new(),
            peer: FindProbe::new(),
            first_close: None,
        }
    }

    pub fn run(
        &mut self,
        rt: &mut Runtime,
        mode: ThreadMode,
        handles: &[BrowserHandle],
        closed: &Mutex<Vec<ProbeBrowserEvent>>,
        tick: Tick,
    ) -> Result<(), String> {
        let [first, peer] = handles else {
            return Err("Find isolation needs two Browsers".into());
        };
        let a = first.snapshot().browser;
        let b = peer.snapshot().browser;
        FindProbe::configure(rt, NonZeroUsize::new(2).unwrap())?;
        self.first.run_configured(rt, mode, first, tick, true)?;
        let original_receipts = self.first.receipts();
        let [_, unresolved] = original_receipts.as_slice() else {
            return Err("original Find receipt missing".into());
        };
        let q = unresolved.status();
        let pending = rt.find_diagnostics();
        if q.operation.browser != a
            || q.outcome.is_some()
            || q.last_result.is_some()
            || q.first_failure.is_some()
            || !q.issued
            || !q.stop_attempted
            || !q.stop_submitted
            || pending.pending != 1
            || !matches!(pending.active.as_slice(), [active] if active.operation == q.operation)
        {
            return Err("original exact search was not pending after Stop".into());
        }
        let original_key = operation_key(q.operation);
        self.peer.run_configured(rt, mode, peer, tick, false)?;
        let receipts = self.peer.receipts();
        let [peer_receipt] = receipts.as_slice() else {
            return Err("peer Find receipt missing".into());
        };
        let peer_result = real_peer_result(peer_receipt.as_ref())?;
        let current = unresolved.status();
        let pending = rt.find_diagnostics();
        if peer_result.operation.browser != b
            || pending.started != 3
            || pending.finished != 2
            || pending.pending != 1
            || pending.faulted
            || pending.first_failure.is_some()
            || current.operation != q.operation
            || current.outcome.is_some()
            || current.last_result.is_some()
            || current.first_failure.is_some()
            || !matches!(pending.active.as_slice(), [active] if active.operation == q.operation)
            || !closed.lock().unwrap().is_empty()
        {
            return Err("peer real final result interfered with the original pending Find".into());
        }

        let source_a = rt
            .frame_source(a)
            .map_err(|e| format!("original frame source: {e:?}"))?;
        let source_b = rt
            .frame_source(b)
            .map_err(|e| format!("survivor frame source: {e:?}"))?;
        first.request_close(CloseMode::Force);
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            if first.snapshot().state == SessionState::Closed && closed.lock().unwrap().len() == 1 {
                break;
            }
            if Instant::now() >= deadline {
                return Err("original Find Browser did not close".into());
            }
            tick(rt, mode)?;
        }
        if !matches!(closed.lock().unwrap().as_slice(), [(actual, seq)] if *actual == identity(first) && *seq > 0)
        {
            return Err("wrong first Closed identity".into());
        }
        let cancelled = unresolved.status();
        let retired = rt.find_diagnostics();
        let a_frames = source_a.snapshot();
        if cancelled.operation != q.operation
            || cancelled.outcome != Some(FindOutcome::Cancelled)
            || cancelled.phase != FindPhase::Finished
            || cancelled.last_result.is_some()
            || cancelled.first_failure.is_some()
            || retired.pending != 0
            || !retired.active.is_empty()
            || retired.finished != 3
            || retired.faulted
            || retired.first_failure.is_some()
            || !a_frames.closed
            || a_frames.view.frame.is_some()
            || a_frames.popup.frame.is_some()
            || !peer.snapshot().is_ready()
            || real_peer_result(peer_receipt.as_ref())? != peer_result
        {
            return Err("original Close failed to retire only the original Find".into());
        }
        let before = source_b
            .latest(SurfaceKind::View)
            .ok_or("surviving first VIEW missing")?
            .info;
        rt.probe_frame_control(b, FrameProbeControl::InvalidateView)
            .map_err(|e| format!("peer invalidate: {e:?}"))?;
        let deadline = Instant::now() + Duration::from_secs(12);
        let after = loop {
            if rt.probe_frame_state(b).is_some_and(|p| p.applied == 1)
                && let Some(frame) = source_b.latest(SurfaceKind::View)
                && frame.info.browser == b
                && frame.info.sequence > before.sequence
                && frame.info.surface_generation == before.surface_generation
                && (frame.info.width, frame.info.height) == (320, 240)
                && frame
                    .pixels
                    .get(4 * (120 * 320 + 160)..4 * (120 * 320 + 160) + 4)
                    == Some(&[126, 76, 26, 255])
            {
                break frame.info;
            }
            if Instant::now() >= deadline {
                return Err("peer did not paint a new original-generation VIEW".into());
            }
            tick(rt, mode)?;
        };
        if !peer.snapshot().is_ready() || real_peer_result(peer_receipt.as_ref())? != peer_result {
            return Err("peer result changed after the surviving frame".into());
        }
        self.first_close = Some(FirstClose {
            first: identity(first),
            survivor: identity(peer),
            original_operation: original_key,
            peer_boundary_operation: operation_key(current.operation),
            counts: [retired.started, retired.finished, retired.pending],
            generation: after.surface_generation,
            frame_before: before.sequence,
            frame_after: after.sequence,
            source_closed: a_frames.closed,
            result_preserved: true,
        });
        Ok(())
    }

    pub(super) fn snapshot(
        &self,
        rt: &Runtime,
        closed: &Mutex<Vec<ProbeBrowserEvent>>,
    ) -> Option<FindIsolationReport> {
        let w = self.first_close.as_ref()?;
        let d = rt.find_diagnostics();
        if d.faulted || d.first_failure.is_some() || !d.active.is_empty() {
            return None;
        }
        Some(FindIsolationReport {
            metadata_owner_only: true,
            original: w.first,
            survivor: w.survivor,
            pending_before_peer: w.original_operation,
            pending_after_peer: w.peer_boundary_operation,
            counts_at_first_close: w.counts,
            final_counts: [d.started, d.finished, d.pending],
            final_late_results: d.late_results,
            stop_return_not_drain: true,
            source_closed: w.source_closed,
            survivor_result_unchanged: w.result_preserved,
            survivor_generation: w.generation,
            survivor_frame_before: w.frame_before,
            survivor_frame_after: w.frame_after,
            closed_events: closed.lock().unwrap().clone(),
            first: self.first.owner_evidence().ok()?,
            peer: self.peer.owner_evidence().ok()?,
        })
    }
}
