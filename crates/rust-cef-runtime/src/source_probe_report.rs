//! Explicit-only source acquisition fixture report; not UI capability proof.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct SourceProbeReport {
    pub metadata_owner_only: bool,
    pub budget_case: bool,
    pub phase: u8,
    pub started: u64,
    pub finished: u64,
    pub final_pending: u64,
    pub final_native_live: u64,
    pub final_host_live: u64,
    pub faulted: bool,
    pub first_failure: Option<String>,
    pub publications: u64,
    pub close_requests: u64,
    pub close_acks: u64,
    pub source_bytes: Vec<usize>,
    pub source_hashes: Vec<u64>,
    pub frame_id_bytes: Vec<usize>,
    /// Command ids the real CEF menu actually offered on this capture.
    pub menu_offered: Vec<Vec<i32>>,
    pub operation_ids: Vec<[u64; 3]>,
    pub outcomes: Vec<String>,
    pub barrier_after_menu_terminal: bool,
    pub viewer_pending_at_shutdown: bool,
    pub budget_rejected: bool,
    pub sink_error: Option<String>,
}
impl SourceProbeReport {
    pub fn complete(&self) -> bool {
        if !self.metadata_owner_only
            || self.faulted
            || self.final_pending != 0
            || self.final_native_live != 0
            || self.final_host_live != 0
            || self.sink_error.is_some()
        {
            return false;
        }
        if self.budget_case {
            self.started == 1
                && self.finished == 1
                && self.publications == 0
                && self.budget_rejected
                && self.first_failure.as_deref() == Some("Budget")
                && self.outcomes == ["Some(Failed)"]
        } else {
            self.started == 2
                && self.finished == 2
                && self.publications == 2
                && self.close_requests == 1
                && self.close_acks == 1
                && self.first_failure.is_none()
                && self.barrier_after_menu_terminal
                && self.viewer_pending_at_shutdown
                && self.source_bytes.len() == 2
                && self.source_bytes.iter().all(|n| *n > 21 && *n <= 8192)
                && self.frame_id_bytes == [34, 34]
                && self.operation_ids == [[1, 1, 1], [1, 1, 2]]
                && self.outcomes == ["Some(Closed)", "Some(Cancelled)"]
        }
    }
}
