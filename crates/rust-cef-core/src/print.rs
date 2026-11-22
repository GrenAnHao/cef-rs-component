//! Bounded lifecycle of a host-managed Print action, separate from menu reply.
//!
//! No CEF, printer, window, worker, or callback is owned or invoked here. The
//! adapter must verify the actual owner, native document and user's decision
//! before calling `confirm` and requalify them at the submission cutpoint.
//! IDs, registration serials and document epochs are correlation, not proof of
//! native liveness, user authorization, or an atomic external effect.
use crate::{
    contracts::{BrowserId, RuntimeGeneration},
    menu::ContextMenuRequestId,
    ports::WindowBinding,
};
use std::{
    collections::BTreeMap,
    num::{NonZeroU64, NonZeroUsize},
    sync::{Arc, Weak},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrintOrigin {
    pub binding: WindowBinding,
    /// Original registration, not a transient focus or paint revision.
    pub registration: NonZeroU64,
    /// The adapter's original valid document context; not an authentication token.
    pub document_epoch: NonZeroU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PrintId {
    pub browser: BrowserId,
    pub sequence: NonZeroU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrintError {
    WrongRuntime,
    InvalidOrigin,
    Unknown,
    Closing,
    Backpressure,
    Replay,
    InvalidPhase,
    Exhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrintFailure {
    OwnerLost,
    DocumentLost,
    DeviceUnavailable,
    RequestRejected,
    Provider { code: Option<i32> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoJobOutcome {
    Cancelled,
    Failed(PrintFailure),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrintOutcome {
    Completed,
    Cancelled,
    Failed(PrintFailure),
}

impl From<NoJobOutcome> for PrintOutcome {
    fn from(value: NoJobOutcome) -> Self {
        match value {
            NoJobOutcome::Cancelled => Self::Cancelled,
            NoJobOutcome::Failed(reason) => Self::Failed(reason),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrintPhase {
    AwaitingConfirmation,
    Confirmed,
    SubmissionClaimed,
    JobAccepted,
    Terminal,
}

/// Dispatch accounting, not evidence that the provider cancelled a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrintCancelDispatch {
    NotClaimed,
    InFlight,
    Released,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrintStatus {
    pub operation: PrintId,
    pub menu: ContextMenuRequestId,
    pub origin: PrintOrigin,
    pub phase: PrintPhase,
    pub cancel_requested: bool,
    pub cancel_dispatch: PrintCancelDispatch,
    /// Provider-declared receipt, not a raw spooler pointer or proof of completion.
    pub accepted_job: Option<NonZeroU64>,
    pub outcome: Option<PrintOutcome>,
    /// The adapter verified that the confirmation UI and all operation-owned
    /// native/provider resources have actually been retired.
    pub resources_retired: bool,
}

#[derive(Debug)]
pub struct RejectedPrint<T> {
    pub error: PrintError,
    pub payload: T,
}

#[derive(Debug)]
pub struct RetiredPrint<T> {
    pub status: PrintStatus,
    pub payload: T,
}

/// One issued claim. A claim records permission for one provider dispatch,
/// not a submitted job. Its Drop changes no ledger state.
#[derive(Debug)]
pub struct PrintSubmission {
    id: PrintId,
    book: Weak<()>,
}

impl PrintSubmission {
    pub fn operation(&self) -> PrintId {
        self.id
    }
}

/// A one-shot dispatch claim for the exact accepted job. Dropping it is not
/// evidence of release; the operation remains counted until actively resolved.
#[must_use = "acknowledge dispatch release after the provider call or verified no-call"]
#[derive(Debug)]
pub struct PrintJobCancel {
    id: PrintId,
    book: Weak<()>,
    receipt: NonZeroU64,
}

impl PrintJobCancel {
    pub fn operation(&self) -> PrintId {
        self.id
    }

    pub fn receipt(&self) -> NonZeroU64 {
        self.receipt
    }
}

struct Active<T> {
    status: PrintStatus,
    payload: T,
}

struct BrowserRow<T> {
    closing: bool,
    last_menu: u64,
    active: Option<Active<T>>,
}

/// One per Runtime, with at most one unreaped operation per enrolled Browser.
/// Capacity bounds enrollment (including menu replay fences), not the native
/// printer queue, process memory, or a guarantee that an action is available.
/// No internal locks and never invokes opaque code. A failed admission or a
/// retired operation returns its payload so the caller can drop it lock-free.
pub struct PrintBook<T> {
    runtime: RuntimeGeneration,
    capacity: NonZeroUsize,
    slots: BTreeMap<BrowserId, BrowserRow<T>>,
    highest_browser: u64,
    next: u64,
    draining: bool,
    identity: Arc<()>,
}

impl<T> PrintBook<T> {
    pub fn new(runtime: RuntimeGeneration, capacity: NonZeroUsize) -> Self {
        Self {
            runtime,
            capacity,
            slots: BTreeMap::new(),
            highest_browser: 0,
            next: 1,
            draining: false,
            identity: Arc::new(()),
        }
    }

    fn qualify_browser(&self, browser: BrowserId) -> Result<(), PrintError> {
        if browser.runtime() != self.runtime {
            return Err(PrintError::WrongRuntime);
        }
        if self.runtime.value() == 0 || browser.local() == 0 {
            return Err(PrintError::InvalidOrigin);
        }
        Ok(())
    }

    /// Call in accepted Browser allocation order (not async `OnAfterCreated`
    /// order). A retired BrowserId must never re-enroll, even after reaping.
    pub fn enroll(&mut self, browser: BrowserId) -> Result<(), PrintError> {
        self.qualify_browser(browser)?;
        if self.draining {
            return Err(PrintError::Closing);
        }
        if browser.local() <= self.highest_browser {
            return Err(PrintError::Replay);
        }
        if self.slots.len() >= self.capacity.get() {
            return Err(PrintError::Backpressure);
        }
        self.slots.insert(
            browser,
            BrowserRow {
                closing: false,
                last_menu: 0,
                active: None,
            },
        );
        self.highest_browser = browser.local();
        Ok(())
    }

    pub fn pending_count(&self) -> usize {
        self.slots.values().filter(|r| r.active.is_some()).count()
    }

    pub fn pending_for(&self, browser: BrowserId) -> bool {
        self.slots.get(&browser).is_some_and(|r| r.active.is_some())
    }

    /// Stop only the current operation for this original Browser/registration.
    /// Navigation and a preventable close are not terminal Browser retirement.
    pub fn invalidate_browser(
        &mut self,
        browser: BrowserId,
        registration: Option<NonZeroU64>,
    ) -> bool {
        self.slots
            .get_mut(&browser)
            .and_then(|row| row.active.as_mut())
            .filter(|item| registration.is_none_or(|r| r == item.status.origin.registration))
            .is_some_and(|item| cancel(&mut item.status))
    }

    /// Called only from an authorized, already-consumed menu action. A menu
    /// sequence cannot open another print operation even after the first is reaped.
    pub fn begin(
        &mut self,
        menu: ContextMenuRequestId,
        origin: PrintOrigin,
        payload: T,
    ) -> Result<PrintId, RejectedPrint<T>> {
        match self.reserve(menu, origin) {
            Ok((id, row)) => {
                row.last_menu = menu.sequence;
                row.active = Some(Active {
                    status: PrintStatus {
                        operation: id,
                        menu,
                        origin,
                        phase: PrintPhase::AwaitingConfirmation,
                        cancel_requested: false,
                        cancel_dispatch: PrintCancelDispatch::NotClaimed,
                        accepted_job: None,
                        outcome: None,
                        resources_retired: false,
                    },
                    payload,
                });
                Ok(id)
            }
            Err(error) => Err(RejectedPrint { error, payload }),
        }
    }

    fn reserve(
        &mut self,
        menu: ContextMenuRequestId,
        origin: PrintOrigin,
    ) -> Result<(PrintId, &mut BrowserRow<T>), PrintError> {
        self.qualify_browser(menu.browser)?;
        if menu.sequence == 0
            || origin.binding.browser != menu.browser
            || origin.binding.window == 0
        {
            return Err(PrintError::InvalidOrigin);
        }
        if self.draining {
            return Err(PrintError::Closing);
        }
        let row = self
            .slots
            .get_mut(&menu.browser)
            .ok_or(PrintError::Unknown)?;
        if row.closing {
            return Err(PrintError::Closing);
        }
        if menu.sequence <= row.last_menu {
            return Err(PrintError::Replay);
        }
        if row.active.is_some() {
            return Err(PrintError::Backpressure);
        }
        let sequence = NonZeroU64::new(self.next).ok_or(PrintError::Exhausted)?;
        self.next = self.next.checked_add(1).unwrap_or(0);
        Ok((
            PrintId {
                browser: menu.browser,
                sequence,
            },
            row,
        ))
    }

    pub fn status(&self, id: PrintId) -> Option<PrintStatus> {
        self.slots
            .get(&id.browser)?
            .active
            .as_ref()
            .filter(|w| w.status.operation == id)
            .map(|w| w.status)
    }

    fn status_mut(&mut self, id: PrintId) -> Result<&mut PrintStatus, PrintError> {
        self.qualify_browser(id.browser)?;
        self.slots
            .get_mut(&id.browser)
            .and_then(|r| r.active.as_mut())
            .filter(|w| w.status.operation == id)
            .map(|w| &mut w.status)
            .ok_or(PrintError::Unknown)
    }

    fn qualified(
        &mut self,
        id: PrintId,
        original: PrintOrigin,
    ) -> Result<&mut PrintStatus, PrintError> {
        let s = self.status_mut(id)?;
        if s.origin != original {
            return Err(PrintError::InvalidOrigin);
        }
        if s.cancel_requested || s.resources_retired {
            return Err(PrintError::Closing);
        }
        Ok(s)
    }

    pub fn confirm(&mut self, id: PrintId, original: PrintOrigin) -> Result<(), PrintError> {
        let s = self.qualified(id, original)?;
        if s.phase != PrintPhase::AwaitingConfirmation {
            return Err(PrintError::InvalidPhase);
        }
        s.phase = PrintPhase::Confirmed;
        Ok(())
    }

    /// Claim just before dispatch to the provider; no lock may span dispatch.
    /// Closing after the claim does not revoke a call already entering native.
    pub fn claim_submission(
        &mut self,
        id: PrintId,
        original: PrintOrigin,
    ) -> Result<PrintSubmission, PrintError> {
        let s = self.qualified(id, original)?;
        if s.phase != PrintPhase::Confirmed {
            return Err(PrintError::InvalidPhase);
        }
        s.phase = PrintPhase::SubmissionClaimed;
        Ok(PrintSubmission {
            id,
            book: Arc::downgrade(&self.identity),
        })
    }

    fn claimed(&mut self, submission: &PrintSubmission) -> Result<&mut PrintStatus, PrintError> {
        if !Weak::ptr_eq(&submission.book, &Arc::downgrade(&self.identity)) {
            return Err(PrintError::Unknown);
        }
        self.status_mut(submission.id)
    }

    /// A valid acceptance can arrive after cancellation intent. Preserve the
    /// real job receipt; the adapter still owes a final provider outcome.
    pub fn job_accepted(
        &mut self,
        claim: &PrintSubmission,
        receipt: NonZeroU64,
    ) -> Result<(), PrintError> {
        let s = self.claimed(claim)?;
        if s.phase != PrintPhase::SubmissionClaimed {
            return Err(PrintError::InvalidPhase);
        }
        s.phase = PrintPhase::JobAccepted;
        s.accepted_job = Some(receipt);
        Ok(())
    }

    /// Only with positive evidence the call did NOT submit a job. A timeout,
    /// missing response, or ambiguous transport failure is not that evidence.
    pub fn no_job_from_claim(
        &mut self,
        claim: &PrintSubmission,
        outcome: NoJobOutcome,
    ) -> Result<(), PrintError> {
        let s = self.claimed(claim)?;
        if s.phase != PrintPhase::SubmissionClaimed {
            return Err(PrintError::InvalidPhase);
        }
        finish(s, outcome.into());
        Ok(())
    }

    fn accepted_job(
        &mut self,
        claim: &PrintSubmission,
        receipt: NonZeroU64,
    ) -> Result<&mut PrintStatus, PrintError> {
        let s = self.claimed(claim)?;
        if s.phase != PrintPhase::JobAccepted {
            return Err(PrintError::InvalidPhase);
        }
        if s.accepted_job != Some(receipt) {
            return Err(PrintError::Unknown);
        }
        Ok(s)
    }

    pub fn job_finished(
        &mut self,
        claim: &PrintSubmission,
        receipt: NonZeroU64,
        outcome: PrintOutcome,
    ) -> Result<(), PrintError> {
        finish(self.accepted_job(claim, receipt)?, outcome);
        Ok(())
    }

    /// Reserve one provider cancellation attempt after both intent and an
    /// exact job receipt. Dispatch outside locks, independently requalifying
    /// the actual provider job. The call may race the real terminal callback.
    pub fn claim_cancel_dispatch(
        &mut self,
        claim: &PrintSubmission,
        receipt: NonZeroU64,
    ) -> Result<PrintJobCancel, PrintError> {
        let s = self.accepted_job(claim, receipt)?;
        if s.cancel_dispatch != PrintCancelDispatch::NotClaimed {
            return Err(PrintError::Replay);
        }
        if !s.cancel_requested || s.resources_retired {
            return Err(PrintError::InvalidPhase);
        }
        s.cancel_dispatch = PrintCancelDispatch::InFlight;
        Ok(PrintJobCancel {
            id: claim.operation(),
            book: claim.book.clone(),
            receipt,
        })
    }

    /// Report that the dispatch path has truly released its use (including
    /// a verified no-call path), from outside the provider call. This does
    /// not report job cancellation, request success, or provider retirement.
    pub fn cancel_dispatch_released(&mut self, claim: &PrintJobCancel) -> Result<(), PrintError> {
        if !Weak::ptr_eq(&claim.book, &Arc::downgrade(&self.identity)) {
            return Err(PrintError::Unknown);
        }
        let s = self.status_mut(claim.id)?;
        if s.accepted_job != Some(claim.receipt) {
            return Err(PrintError::Unknown);
        }
        if s.cancel_dispatch != PrintCancelDispatch::InFlight {
            return Err(PrintError::Replay);
        }
        s.cancel_dispatch = PrintCancelDispatch::Released;
        Ok(())
    }

    pub fn no_job_before_claim(
        &mut self,
        id: PrintId,
        outcome: NoJobOutcome,
    ) -> Result<(), PrintError> {
        let s = self.status_mut(id)?;
        if !matches!(
            s.phase,
            PrintPhase::AwaitingConfirmation | PrintPhase::Confirmed
        ) {
            return Err(PrintError::InvalidPhase);
        }
        finish(s, outcome.into());
        Ok(())
    }

    pub fn request_cancel(&mut self, id: PrintId) -> Result<bool, PrintError> {
        let s = self.status_mut(id)?;
        Ok(cancel(s))
    }

    /// Reject subsequent work for this Browser without cancelling another.
    /// Retain any unretired operation through Browser teardown.
    pub fn close_browser(&mut self, browser: BrowserId) -> Result<(), PrintError> {
        self.qualify_browser(browser)?;
        let row = self.slots.get_mut(&browser).ok_or(PrintError::Unknown)?;
        row.closing = true;
        if let Some(active) = &mut row.active {
            cancel(&mut active.status);
        } else {
            self.slots.remove(&browser);
        }
        Ok(())
    }

    pub fn begin_drain(&mut self) {
        self.draining = true;
        self.slots.retain(|_, row| {
            row.closing = true;
            if let Some(active) = &mut row.active {
                cancel(&mut active.status);
                true
            } else {
                false
            }
        });
    }

    /// Report actual resource retirement, not merely an attempted close. A
    /// provider outcome may still be pending and must not be synthesized here.
    pub fn resources_retired(&mut self, id: PrintId) -> Result<bool, PrintError> {
        let s = self.status_mut(id)?;
        let first = !s.resources_retired;
        s.resources_retired = true;
        cancel(s);
        Ok(first)
    }

    pub fn take_retired(&mut self, id: PrintId) -> Option<RetiredPrint<T>> {
        let s = self.status(id)?;
        if !s.resources_retired
            || s.phase != PrintPhase::Terminal
            || s.cancel_dispatch == PrintCancelDispatch::InFlight
        {
            return None;
        }
        let row = self
            .slots
            .get_mut(&id.browser)
            .expect("qualified Browser row");
        let Active { status, payload } = row.active.take().expect("qualified operation");
        if row.closing {
            self.slots.remove(&id.browser);
        }
        Some(RetiredPrint { status, payload })
    }
}

fn finish(status: &mut PrintStatus, outcome: PrintOutcome) {
    status.phase = PrintPhase::Terminal;
    status.outcome = Some(outcome);
}

fn cancel(status: &mut PrintStatus) -> bool {
    if status.phase == PrintPhase::Terminal || status.cancel_requested {
        return false;
    }
    status.cancel_requested = true;
    true
}

#[cfg(test)]
mod tests;
