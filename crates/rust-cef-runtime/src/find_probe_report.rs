//! Explicit windowless Find fixture evidence; never a UI acceptance report.
use crate::{menu_bridge_fixture::BridgeFixtureSnapshot, test_support::ProbeBrowserEvent};
use rust_cef_core::find::FindOperationId;

pub fn operation_key(id: FindOperationId) -> [u64; 3] {
    [
        id.browser.runtime().value(),
        id.browser.local(),
        id.sequence,
    ]
}

pub type NativeFindFinal = ([u64; 3], i32, i32, i32, u64);

#[derive(Debug, serde::Serialize)]
pub struct FindOwnerEvidence {
    pub menu: BridgeFixtureSnapshot,
    pub query_bytes: Vec<usize>,
    pub frame_id_bytes: Vec<usize>,
    pub started: Vec<[u64; 3]>,
    pub finished: Vec<([u64; 3], String)>,
    // Operation key, actual CEF id, matches, active ordinal and callback thread.
    pub native_finals: Vec<NativeFindFinal>,
    pub all_receipts_retired: bool,
}
impl FindOwnerEvidence {
    fn complete(&self, keys: &[[u64; 3]], expected: &[&str], cef_ui_thread: u64) -> bool {
        let menu_count = keys.len() + 1;
        self.menu.published == menu_count as u64
            && self.menu.terminal == menu_count as u64
            && self.menu.outstanding == 0
            && self.menu.first_error.is_none()
            && self.menu.terminal_ids.len() == menu_count
            && self.menu.terminal_ids[0] > 0
            && self.menu.terminal_ids.windows(2).all(|ids| ids[0] < ids[1])
            && self.menu.selection_bytes.len() == menu_count
            && self.menu.selection_bytes[0] == 0
            && self.menu.selection_bytes[1..].iter().all(|n| *n == 21)
            && self.started == keys
            && self.query_bytes == vec![21; keys.len()]
            && self.frame_id_bytes == vec![34; keys.len()]
            && self.finished.len() == keys.len()
            && self
                .finished
                .iter()
                .zip(keys)
                .zip(expected)
                .all(|(((key, state), original), outcome)| key == original && state == outcome)
            && self.all_receipts_retired
            && matches!(self.native_finals.as_slice(), [(owner, id, 1, ordinal, thread)]
                if *owner == keys[0] && *id >= 0 && (0..=1).contains(ordinal) && *thread == cef_ui_thread)
    }
}

#[derive(Debug, serde::Serialize)]
pub struct FindIsolationReport {
    pub metadata_owner_only: bool,
    pub original: (u64, u64),
    pub survivor: (u64, u64),
    pub pending_before_peer: [u64; 3],
    pub pending_after_peer: [u64; 3],
    pub counts_at_first_close: [u64; 3],
    pub final_counts: [u64; 3],
    pub final_late_results: u64,
    pub stop_return_not_drain: bool,
    pub source_closed: bool,
    pub survivor_result_unchanged: bool,
    pub survivor_generation: u64,
    pub survivor_frame_before: u64,
    pub survivor_frame_after: u64,
    pub closed_events: Vec<ProbeBrowserEvent>,
    pub first: FindOwnerEvidence,
    pub peer: FindOwnerEvidence,
}
impl FindIsolationReport {
    pub fn complete(&self, cef_ui_thread: u64) -> bool {
        let (ra, a) = self.original;
        let (rb, b) = self.survivor;
        self.metadata_owner_only
            && cef_ui_thread > 0
            && ra != 0
            && ra == rb
            && a > 0
            && b > a
            && self.pending_before_peer == [ra, a, 2]
            && self.pending_after_peer == self.pending_before_peer
            && self.counts_at_first_close == [3, 3, 0]
            && self.final_counts == [3, 3, 0]
            && self.stop_return_not_drain
            && self.source_closed
            && self.survivor_result_unchanged
            && self.survivor_generation > 0
            && self.survivor_frame_before > 0
            && self.survivor_frame_after > self.survivor_frame_before
            && matches!(self.closed_events.as_slice(), [(first, sa), (second, sb)]
                if *first == self.original && *second == self.survivor && *sa > 0 && *sb > *sa)
            && self.first.complete(
                &[[ra, a, 1], [ra, a, 2]],
                &["Some(Result)", "Some(Cancelled)"],
                cef_ui_thread,
            )
            && self
                .peer
                .complete(&[[rb, b, 3]], &["Some(Result)"], cef_ui_thread)
            && self.first.native_finals[0].1 < self.peer.native_finals[0].1
    }
}

#[derive(Debug, serde::Serialize)]
pub struct FindJointDrain {
    pub pending_before_first_close: [u64; 3],
    pub pending_after_first_close: [u64; 3],
    pub cancelled_after_survivor_close: [u64; 3],
    pub source_operation_through_first_close: [u64; 3],
    pub flush_marker_through_first_close: [u64; 3],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title_at_joint_cut: Option<u64>,
    pub counts_after_survivor: [u64; 3],
    pub late_results_observed: u64,
    pub closed_events: Vec<ProbeBrowserEvent>,
    pub find_owner: FindOwnerEvidence,
}

impl FindJointDrain {
    pub fn complete(
        &self,
        ui: u64,
        source: &crate::test_support::SourceVisitorDrain,
        flush: &crate::test_support::ProfileFlushDrain,
    ) -> bool {
        let (runtime, browser) = source.survivor;
        let stopped = [runtime, browser, 2];
        ui > 0
            && self.pending_before_first_close == stopped
            && self.pending_after_first_close == stopped
            && self.cancelled_after_survivor_close == stopped
            && self.source_operation_through_first_close == source.operation
            && self.flush_marker_through_first_close
                == [flush.profile.0, flush.profile.1, flush.sequence]
            && self.counts_after_survivor == [2, 2, 0]
            && self.closed_events == source.closed_events
            && self.closed_events == flush.closed_events
            && self.find_owner.complete(
                &[[runtime, browser, 1], stopped],
                &["Some(Result)", "Some(Cancelled)"],
                ui,
            )
    }
}

#[derive(Default, Clone, Debug, serde::Serialize)]
pub struct FindProbeReport {
    pub metadata_owner_only: bool,
    pub phase: u8,
    pub started: u64,
    pub finished: u64,
    pub final_pending: u64,
    pub faulted: bool,
    pub first_failure: Option<String>,
    pub query_bytes: Vec<usize>,
    pub frame_id_bytes: Vec<usize>,
    pub cef_identifiers: Vec<i32>,
    pub local_sequences: Vec<u64>,
    pub matches: Vec<i32>,
    pub active_ordinals: Vec<i32>,
    pub callback_threads: Vec<u64>,
    pub outcomes: Vec<String>,
    pub menu_terminal_separate: bool,
    pub repeat_pending_before_close: bool,
    pub stop_return_not_drain: bool,
    pub operations_drained_after_close: bool,
    pub sink_error: Option<String>,
}
impl FindProbeReport {
    pub fn complete(&self) -> bool {
        self.metadata_owner_only
            && self.phase == 4
            && self.started == 2
            && self.finished == 2
            && self.final_pending == 0
            && !self.faulted
            && self.first_failure.is_none()
            && self.sink_error.is_none()
            && self.query_bytes == [21, 21]
            && self.frame_id_bytes == [34, 34]
            && self.cef_identifiers.len() == 1
            && self.cef_identifiers[0] >= 0
            && self.local_sequences == [1, 2]
            && self.matches == [1]
            && self.active_ordinals.len() == 1
            && self.active_ordinals.iter().all(|n| *n >= 0 && *n <= 1)
            && self.callback_threads.len() == 1
            && self.callback_threads[0] > 0
            && self.outcomes == ["Some(Result)", "Some(Cancelled)"]
            && self.menu_terminal_separate
            && self.repeat_pending_before_close
            && self.stop_return_not_drain
            && self.operations_drained_after_close
    }
}
