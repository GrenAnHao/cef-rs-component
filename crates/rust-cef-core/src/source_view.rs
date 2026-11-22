//! Owned source-view operation protocol. No native pointer, UI object or shell.
use crate::{
    contracts::BrowserId, menu::ContextMenuRequestId, menu_context::MenuContext, ports::EngineError,
};
use std::{fmt, num::NonZeroUsize, sync::Arc};
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceOperationId {
    pub browser: BrowserId,
    pub sequence: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceViewLimits {
    pub capacity: NonZeroUsize,
    pub max_source_bytes: NonZeroUsize,
}
/// A bounded owned copy, not a native-allocation or process RSS guarantee.
#[derive(Clone, PartialEq, Eq)]
pub struct SourceText(Arc<str>);
impl SourceText {
    pub fn new(text: String, limit: NonZeroUsize) -> Result<Self, SourceFailure> {
        if text.len() > limit.get() {
            return Err(SourceFailure::Budget);
        }
        Ok(Self(text.into()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
impl fmt::Debug for SourceText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceText")
            .field("bytes", &self.len())
            .finish()
    }
}
#[derive(Clone, Debug)]
pub struct SourceViewRequest {
    pub operation: SourceOperationId,
    pub menu: ContextMenuRequestId,
    pub context: Arc<MenuContext>,
    pub source: SourceText,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceFailure {
    Budget,
    InvalidText,
    MissingText,
    NoCallback,
    WrongThread,
    Stale,
    DuplicateCallback,
    Post(EngineError),
    Start(EngineError),
    Publish(EngineError),
    Close(EngineError),
    Wake(EngineError),
    Invariant,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourcePhase {
    Fetching,
    Queued,
    Presented,
    Closing,
    Finished,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Monotonic retirement fact, not a promise that a reentrant host method
/// cannot return an error after closed/native-release acknowledgement. Inspect
/// first_failure as well; late diagnostics never resurrect native/UI resources.
pub enum SourceOutcome {
    Closed,
    Cancelled,
    Failed,
}
#[derive(Clone, Debug)]
pub struct SourceOperationStatus {
    pub operation: SourceOperationId,
    pub menu: ContextMenuRequestId,
    pub phase: SourcePhase,
    pub native_live: bool,
    pub host_live: bool,
    pub callback_received: bool,
    pub cancelled: bool,
    pub source_bytes: usize,
    pub first_failure: Option<SourceFailure>,
    pub first_close_failure: Option<EngineError>,
    pub outcome: Option<SourceOutcome>,
}
/// Calls are data-only and may originate on the CEF UI thread. The host owns
/// original-window validation, explicit UI dispatch and every viewer window.
pub trait SourceViewSink: Send + Sync {
    /// Accept an owned operation receipt before extraction; retain it to observe
    /// failure/cancellation even when no source document is ever published.
    /// This notification must NOT create a viewer/progress/native window.
    fn started(
        &self,
        id: SourceOperationId,
        menu: ContextMenuRequestId,
        control: Arc<dyn SourceViewControl>,
    ) -> Result<(), EngineError>;
    fn publish(
        &self,
        request: SourceViewRequest,
        control: Arc<dyn SourceViewControl>,
    ) -> Result<(), EngineError>;
    /// Must acknowledge actual absence/closure through control.closed, including
    /// rejected publication. A refused close never authorizes fake drainage.
    fn close(
        &self,
        id: SourceOperationId,
        control: Arc<dyn SourceViewControl>,
    ) -> Result<(), EngineError>;
}
pub trait SourceViewControl: Send + Sync {
    fn status(&self) -> SourceOperationStatus;
    /// Check before creating/displaying the original host-owned source viewer.
    fn is_current(&self) -> bool;
    /// Real viewer creation, not enqueue/admission. Host checks its original
    /// window before creation; own viewer focus does not reuse the menu lease.
    fn presented(&self) -> Result<(), EngineError>;
    /// Real viewer absence AND owned payload retirement, not a close request.
    fn closed(&self) -> Result<(), EngineError>;
    /// Explicit host cancellation/retry only; never a timer or hidden repost.
    fn request_close(&self) -> Result<(), EngineError>;
}
