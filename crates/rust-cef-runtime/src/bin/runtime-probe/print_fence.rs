//! Runtime barrier integration: real CEF Browser shutdown with two metadata-only
//! print confirmations. No Print call, UI owner, spool job, or provider callback.
use rust_cef_core::{
    contracts::{SessionState, ThreadMode},
    ports::{BrowserHandle, CloseMode, CloseResult, EngineError},
    print::{PrintId, PrintOutcome, PrintPhase},
};
use rust_cef_runtime::{
    runtime::{Runtime, ShutdownError, ShutdownStatus, ShutdownTicket},
    test_support::PrintFenceReport,
};
use std::time::{Duration, Instant};

type Tick = fn(&mut Runtime, ThreadMode) -> Result<(), String>;

fn counted(rt: &Runtime, proof: &mut PrintFenceReport, expected: u64) -> Result<(), String> {
    let pending = rt.diagnostics().pending_requests;
    proof.pending_by_cut.push(pending);
    if pending != expected {
        return Err(format!("print gate count {pending}, expected {expected}"));
    }
    Ok(())
}

fn await_cut(
    rt: &mut Runtime,
    mode: ThreadMode,
    tick: Tick,
    label: &str,
    ready: impl Fn(&Runtime) -> bool,
) -> Result<(), String> {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        if ready(rt) {
            return Ok(());
        }
        if Instant::now() >= until {
            return Err(format!("print fence {label} timeout"));
        }
        tick(rt, mode)?;
    }
}

fn blocked(
    rt: &mut Runtime,
    ticket: &ShutdownTicket,
    proof: &mut PrintFenceReport,
) -> Result<(), String> {
    let poll = rt.poll_shutdown(ticket);
    if !matches!(&poll, ShutdownStatus::Blocked {open_browsers,pending_tasks:0,pending_profiles:0,pending_requests:1,pending_cookie_flushes:0,post_failures:0} if open_browsers.is_empty())
    {
        return Err(format!(
            "original print did not exclusively block poll_shutdown: {poll:?}"
        ));
    }
    let finish = rt.finish_shutdown();
    let same = matches!(&finish,Err(ShutdownError::Blocked {open_browsers,pending_tasks:0,pending_profiles:0,pending_requests:1,pending_cookie_flushes:0,post_failures:0}) if open_browsers.is_empty());
    proof.both_shutdown_gates_blocked.push(same);
    if !same || rt.diagnostics().cef_shutdown_calls != 0 {
        return Err(format!(
            "original print did not exclusively block finish_shutdown: {finish:?}"
        ));
    }
    Ok(())
}

fn cancelled(rt: &Runtime, id: PrintId) -> Result<(), String> {
    rt.probe_print_cancelled(id)
        .map_err(|e| format!("exact no-job cancellation receipt: {e:?}"))?;
    if !rt.probe_print_status(id).is_some_and(|s| {
        s.phase == PrintPhase::Terminal
            && s.cancel_requested
            && s.outcome == Some(PrintOutcome::Cancelled)
            && s.accepted_job.is_none()
            && !s.resources_retired
    }) {
        return Err("recorded print outcome was confused with resource retirement".into());
    }
    Ok(())
}

fn retired(rt: &Runtime, id: PrintId) -> Result<(), String> {
    let s = rt
        .probe_print_retired(id)
        .map_err(|e| format!("exact original print resource retirement: {e:?}"))?;
    if s.operation != id
        || s.outcome != Some(PrintOutcome::Cancelled)
        || !s.resources_retired
        || s.accepted_job.is_some()
        || rt.probe_print_status(id).is_some()
    {
        return Err("original print operation was not retired precisely".into());
    }
    Ok(())
}

pub fn exercise(
    rt: &mut Runtime,
    mode: ThreadMode,
    tick: Tick,
    handles: &[BrowserHandle],
    proof: &mut PrintFenceReport,
) -> Result<(), String> {
    let [a, b] = handles else {
        return Err("print fence needs two original Browsers".into());
    };
    let browsers = [a.snapshot().browser, b.snapshot().browser];
    let [a_id, b_id] = rt
        .probe_arm_prints(browsers)
        .map_err(|e| format!("arm metadata print confirmations: {e:?}"))?;
    proof.metadata_only = true;
    proof.ids = [a_id, b_id].map(|id| {
        [
            id.browser.runtime().value(),
            id.browser.local(),
            id.sequence.get(),
        ]
    });
    let result = (|| {
        counted(rt, proof, 2)?;
        let a_source = rt
            .frame_source(a_id.browser)
            .map_err(|e| format!("A frame source: {e:?}"))?;
        let b_source = rt
            .frame_source(b_id.browser)
            .map_err(|e| format!("B frame source: {e:?}"))?;
        let a_close = a.request_close(CloseMode::Force);
        await_cut(rt, mode, tick, "A actual Closed", |_| {
            a_close.result() == CloseResult::Closed
                && a.snapshot().state == SessionState::Closed
                && a_source.snapshot().closed
        })?;
        if a_close.browser != a_id.browser || a_close.sequence == 0 {
            return Err("original A close ticket changed identity".into());
        }
        proof.a_close_sequence = a_close.sequence;
        proof.a_closed_source = a_source.snapshot().closed;
        proof.b_owned_live_at_a_close = b.snapshot().browser == b_id.browser
            && b.snapshot().is_ready()
            && !b_source.snapshot().closed;
        proof.cancelled_original_a_only = rt
            .probe_print_status(a_id)
            .is_some_and(|s| s.phase == PrintPhase::AwaitingConfirmation && s.cancel_requested)
            && rt.probe_print_status(b_id).is_some_and(|s| {
                s.phase == PrintPhase::AwaitingConfirmation && !s.cancel_requested
            });
        if !proof.b_owned_live_at_a_close || !proof.cancelled_original_a_only {
            return Err("A close changed B or lost original print intent".into());
        }
        counted(rt, proof, 2)?;
        if !matches!(
            rt.probe_print_cancelled(b_id),
            Err(EngineError::InvalidRequest)
        ) || !matches!(
            rt.probe_print_retired(a_id),
            Err(EngineError::InvalidRequest)
        ) {
            return Err(
                "print fence accepted a live peer cancellation or premature A retirement".into(),
            );
        }
        cancelled(rt, a_id)?;
        counted(rt, proof, 2)?;
        retired(rt, a_id)?;
        if !matches!(
            rt.probe_print_cancelled(a_id),
            Err(EngineError::InvalidRequest)
        ) || !matches!(
            rt.probe_print_retired(a_id),
            Err(EngineError::InvalidRequest)
        ) || !matches!(
            rt.probe_print_retired(b_id),
            Err(EngineError::InvalidRequest)
        ) {
            return Err(
                "print fence accepted a retired A replay or unconfirmed B retirement".into(),
            );
        }
        proof.wrong_owner_and_replay_refused = true;
        counted(rt, proof, 1)?;

        // A diagnostic deadline already reached: only the outstanding print
        // may block finish after both real Browsers and tasks have drained.
        let ticket = rt.request_shutdown(CloseMode::Force, Instant::now());
        await_cut(rt, mode, tick, "B actual Closed and task drain", |r| {
            b.snapshot().state == SessionState::Closed
                && b_source.snapshot().closed
                && r.diagnostics().tasks.pending == 0
        })?;
        proof.b_closed_source = b_source.snapshot().closed;
        if !matches!(rt.probe_arm_prints(browsers), Err(EngineError::Closing)) {
            return Err("Runtime renegotiated print admission during drain".into());
        }
        if !rt.probe_print_status(b_id).is_some_and(|s| {
            s.cancel_requested
                && s.phase == PrintPhase::AwaitingConfirmation
                && !s.resources_retired
        }) {
            return Err("B original confirmation did not remain pending through close".into());
        }
        counted(rt, proof, 1)?;
        blocked(rt, &ticket, proof)?;
        cancelled(rt, b_id)?;
        counted(rt, proof, 1)?;
        blocked(rt, &ticket, proof)?;
        retired(rt, b_id)?;
        counted(rt, proof, 0)?;
        proof.ready_after_explicit_retirement =
            matches!(rt.poll_shutdown(&ticket), ShutdownStatus::Ready)
                && rt.diagnostics().cef_shutdown_calls == 0;
        if !proof.ready_after_explicit_retirement {
            return Err("resolved original print did not release gate".into());
        }
        Ok(())
    })();
    if result.is_err() {
        // This explicit cleanup is allowed only for the two metadata records:
        // the test has no provider and cannot have submitted any job.
        let _ = rt.request_shutdown(CloseMode::Force, Instant::now());
        for id in [a_id, b_id] {
            if let Some(status) = rt.probe_print_status(id) {
                if status.outcome.is_none()
                    && let Err(e) = rt.probe_print_cancelled(id)
                {
                    proof
                        .cleanup_errors
                        .push(format!("print probe cleanup no-job {id:?}: {e:?}"));
                }
                if let Err(e) = rt.probe_print_retired(id) {
                    proof
                        .cleanup_errors
                        .push(format!("print probe cleanup retirement {id:?}: {e:?}"));
                }
            }
        }
    }
    result
}
