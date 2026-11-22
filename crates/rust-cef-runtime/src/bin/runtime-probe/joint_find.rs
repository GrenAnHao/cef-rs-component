//! Original B Find stays unresolved across A's Source/flush close. The native
//! renderer is not asserted to be searching after Stop returned.
use crate::find_validation::FindProbe;
use rust_cef_core::{
    contracts::{BrowserId, SessionState, ThreadMode},
    find::{FindOutcome, FindPhase, FindReceipt},
    menu::ContextMenuRequestId,
    ports::BrowserHandle,
    source_view::SourceOperationId,
};
use rust_cef_runtime::{
    find_probe_report::{FindJointDrain, operation_key},
    runtime::Runtime,
    test_support::{
        CookieFlushMarkerProbe, ManagedPressureCut, PRESSURE_EVENT_CAPACITY, PRESSURE_TITLE_PREFIX,
        ProbeBrowserEvent,
    },
};
use std::sync::Arc;

type Tick = fn(&mut Runtime, ThreadMode) -> Result<(), String>;

pub(super) struct JointFind {
    probe: FindProbe,
    pending: Arc<dyn FindReceipt>,
    source: SourceOperationId,
    marker: CookieFlushMarkerProbe,
    before: [u64; 3],
    after: Option<[u64; 3]>,
    title_at_cut: Option<u64>,
}

fn identity(id: BrowserId) -> (u64, u64) {
    (id.runtime().value(), id.local())
}

impl JointFind {
    pub(super) fn start(
        rt: &mut Runtime,
        mode: ThreadMode,
        first: &BrowserHandle,
        survivor: &BrowserHandle,
        marker: CookieFlushMarkerProbe,
        tick: Tick,
    ) -> Result<Self, String> {
        let original = first.snapshot().browser;
        let source = rt
            .probe_held_source_visitor()
            .filter(|s| s.browser == original)
            .ok_or("joint Find requires A's original visitor")?;
        let mut probe = FindProbe::new();
        // The fixture's shared configuration allows two menu registrations;
        // this owner and its receipts belong solely to B.
        probe.run_configured(rt, mode, survivor, tick, true)?;
        let [_, pending] = probe
            .receipts()
            .try_into()
            .map_err(|_| "joint Find expected two receipts")?;
        let mut joint = Self {
            probe,
            pending,
            source,
            marker,
            before: [0; 3],
            after: None,
            title_at_cut: None,
        };
        joint.qualify_source_and_flush(rt, true)?;
        if !first.snapshot().is_ready() || !survivor.snapshot().is_ready() {
            return Err("both Browsers must still be live at joint admission".into());
        }
        joint.before = joint.pending_key(rt, survivor.snapshot().browser)?;
        Ok(joint)
    }

    fn pending_key(&self, rt: &Runtime, expected: BrowserId) -> Result<[u64; 3], String> {
        let q = self.pending.status();
        let d = rt.find_diagnostics();
        if q.operation.browser != expected
            || !q.issued
            || !q.stop_attempted
            || !q.stop_submitted
            || q.outcome.is_some()
            || q.last_result.is_some()
            || q.first_failure.is_some()
            || d.started != 2
            || d.finished != 1
            || d.pending != 1
            || d.faulted
            || d.first_failure.is_some()
            || !matches!(d.active.as_slice(), [a] if a.operation == q.operation)
        {
            return Err("B's exact unresolved Find did not survive the sampled boundary".into());
        }
        Ok(operation_key(q.operation))
    }

    fn qualify_source_and_flush(&self, rt: &Runtime, host_view_live: bool) -> Result<(), String> {
        let d = rt.source_view_diagnostics();
        let x = rt.diagnostics();
        if rt.probe_held_source_visitor() != Some(self.source)
            || d.faulted
            || d.pending != 1
            || !matches!(d.active.as_slice(), [q] if q.operation == self.source && q.callback_received
                && q.native_live && q.host_live == host_view_live && q.cancelled != host_view_live)
            || rt.probe_cookie_flush_marker() != Some(self.marker)
            || self.marker.release_reserved
            || self.marker.faulted
            || self.marker.callback_thread == 0
            || Some(self.marker.callback_thread) != x.cef_ui_thread_id
            || x.cookie_flushes_accepted != 1
            || x.cookie_flushes_completed != 0
            || x.pending_cookie_flushes != 1
        {
            return Err("Find sample did not retain the original Source and flush".into());
        }
        Ok(())
    }

    pub(super) fn before_first_close_under_titles(
        &mut self,
        rt: &Runtime,
        first: &BrowserHandle,
        survivor: &BrowserHandle,
    ) -> Result<(), String> {
        self.qualify_source_and_flush(rt, true)?;
        let a = first.snapshot();
        let b = survivor.snapshot();
        let title = a
            .navigation
            .title
            .strip_prefix(PRESSURE_TITLE_PREFIX)
            .and_then(|value| value.parse::<u64>().ok());
        if a.browser != self.source.browser
            || !a.is_ready()
            || !b.is_ready()
            || self.pending_key(rt, b.browser)? != self.before
            || title != Some(PRESSURE_EVENT_CAPACITY as u64 + 1)
        {
            return Err("title pressure changed the shared shutdown cut".into());
        }
        self.title_at_cut = title;
        Ok(())
    }

    pub(super) fn managed_cut(&self, menu: ContextMenuRequestId) -> ManagedPressureCut {
        ManagedPressureCut {
            menu,
            source: self.source,
            find: self.pending.status().operation,
            profile: self.marker.profile,
            flush_sequence: self.marker.sequence,
        }
    }

    pub(super) fn after_first_close(
        &mut self,
        rt: &Runtime,
        first: &BrowserHandle,
        survivor: &BrowserHandle,
    ) -> Result<(), String> {
        if first.snapshot().state != SessionState::Closed || !survivor.snapshot().is_ready() {
            return Err("expected only A closed before sampling B Find".into());
        }
        self.qualify_source_and_flush(rt, false)?;
        let after = self.pending_key(rt, survivor.snapshot().browser)?;
        if after != self.before {
            return Err("A close changed B's pending operation identity".into());
        }
        self.after = Some(after);
        Ok(())
    }

    pub(super) fn after_survivor_close(
        &self,
        rt: &Runtime,
        survivor: &BrowserHandle,
        closed_events: Vec<ProbeBrowserEvent>,
    ) -> Result<FindJointDrain, String> {
        // Called after source/flush barrier observation but before either
        // release. The original B Closed and independent Cancelled are required.
        self.qualify_source_and_flush(rt, false)?;
        let b = survivor.snapshot();
        let q = self.pending.status();
        let d = rt.find_diagnostics();
        let key = operation_key(q.operation);
        if b.state != SessionState::Closed
            || q.operation.browser != b.browser
            || key != self.before
            || q.outcome != Some(FindOutcome::Cancelled)
            || q.phase != FindPhase::Finished
            || q.last_result.is_some()
            || q.first_failure.is_some()
            || d.started != 2
            || d.finished != 2
            || d.pending != 0
            || !d.active.is_empty()
            || d.faulted
            || d.first_failure.is_some()
            || !matches!(closed_events.as_slice(), [(a, sa), (b_id, sb)] if *a == identity(self.source.browser)
                && *b_id == identity(b.browser) && *sa > 0 && *sa < *sb)
        {
            return Err("B's original Find did not retire at its own close".into());
        }
        Ok(FindJointDrain {
            pending_before_first_close: self.before,
            pending_after_first_close: self.after.ok_or("missing first-close Find observation")?,
            cancelled_after_survivor_close: key,
            source_operation_through_first_close: [
                self.source.browser.runtime().value(),
                self.source.browser.local(),
                self.source.sequence,
            ],
            flush_marker_through_first_close: [
                self.marker.profile.runtime().value(),
                self.marker.profile.local(),
                self.marker.sequence,
            ],
            title_at_joint_cut: self.title_at_cut,
            counts_after_survivor: [d.started, d.finished, d.pending],
            late_results_observed: d.late_results,
            closed_events,
            find_owner: self.probe.owner_evidence()?,
        })
    }
}
