//! UI-neutral, bounded reference-menu Find. Search text stays in the original
//! owned menu context; diagnostics never format its contents. No generic UI.
use crate::{
    contracts::BrowserId, menu::ContextMenuRequestId, menu_context::MenuContext, ports::EngineError,
};
use std::{num::NonZeroUsize, sync::Arc};
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FindOperationId {
    pub browser: BrowserId,
    pub sequence: u64,
}
/// Capacity counts enrolled Browser correlation records until actual close;
/// one unresolved search per Browser. Byte limit is logical UTF8, not RSS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FindLimits {
    pub capacity: NonZeroUsize,
    pub max_query_bytes: NonZeroUsize,
}
#[derive(Clone, Debug)]
pub struct FindRequest {
    pub operation: FindOperationId,
    pub menu: ContextMenuRequestId,
    pub context: Arc<MenuContext>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FindRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindResult {
    pub operation: FindOperationId,
    pub cef_identifier: i32,
    pub matches: i32,
    pub active_ordinal: i32,
    pub selection_rect: Option<FindRect>,
    pub final_update: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindPhase {
    Admitted,
    Searching,
    Cancelling,
    Finished,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Monotonic result/retirement fact. Check first_failure as well: a reentrant
/// sink may return an error after observing the result. That cannot resurrect
/// work or erase the actual final result, but is NOT successful delivery.
pub enum FindOutcome {
    Result,
    Cancelled,
    EmptySelection,
    Failed,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FindFailure {
    Budget,
    Stale,
    WrongThread,
    Uncorrelated,
    InvalidResult,
    Start(EngineError),
    Publish(EngineError),
    Post(EngineError),
    Wake(EngineError),
    NativeUnavailable,
}
#[derive(Clone, Debug)]
pub struct FindStatus {
    pub operation: FindOperationId,
    pub menu: ContextMenuRequestId,
    pub phase: FindPhase,
    pub query_bytes: usize,
    pub issued: bool,
    pub cancel_requested: bool,
    pub stop_attempted: bool,
    pub stop_submitted: bool,
    pub last_result: Option<FindResult>,
    pub outcome: Option<FindOutcome>,
    pub first_failure: Option<FindFailure>,
}
pub trait FindReceipt: Send + Sync {
    fn status(&self) -> FindStatus;
    /// Explicit cancellation, not proof of native drainage. Unknown/unfinalized
    /// searches remain pending until actual final reply or original Browser close.
    fn cancel(&self) -> Result<(), EngineError>;
}
/// Owned result transport for the existing owner UI, not a window provider.
/// Implementations must not create unregistered auxiliary/native windows here.
/// Admission, real final result and native Browser/client lifetime are distinct.
pub trait FindResultSink: Send + Sync {
    fn started(
        &self,
        request: FindRequest,
        receipt: Arc<dyn FindReceipt>,
    ) -> Result<(), EngineError>;
    fn update(&self, result: FindResult) -> Result<(), EngineError>;
    fn finished(&self, status: FindStatus) -> Result<(), EngineError>;
}
