use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn nz(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}

fn browser(local: u64) -> BrowserId {
    BrowserId::new(RuntimeGeneration::new(1), local)
}

fn origin(browser: BrowserId) -> PrintOrigin {
    PrintOrigin {
        binding: WindowBinding {
            browser,
            window: 900 + browser.local(),
        },
        registration: nz(3),
        document_epoch: nz(4),
    }
}

fn menu(browser: BrowserId, sequence: u64) -> ContextMenuRequestId {
    ContextMenuRequestId { browser, sequence }
}

fn book<T>(capacity: usize) -> PrintBook<T> {
    PrintBook::new(
        RuntimeGeneration::new(1),
        NonZeroUsize::new(capacity).unwrap(),
    )
}

#[test]
fn confirmation_and_claim_recheck_the_original_registration_and_document() {
    let a = browser(1);
    let original = origin(a);
    let mut b = book(1);
    b.enroll(a).unwrap();
    let w = b.begin(menu(a, 5), original, ()).unwrap();
    assert_eq!(
        b.claim_submission(w, original).unwrap_err(),
        PrintError::InvalidPhase
    );
    let replacement_owner = PrintOrigin {
        registration: nz(7),
        ..original
    };
    assert_eq!(
        b.confirm(w, replacement_owner),
        Err(PrintError::InvalidOrigin)
    );
    assert_eq!(b.status(w).unwrap().phase, PrintPhase::AwaitingConfirmation);
    b.confirm(w, original).unwrap();
    assert_eq!(b.confirm(w, original), Err(PrintError::InvalidPhase));
    let different_document = PrintOrigin {
        document_epoch: nz(5),
        ..original
    };
    assert_eq!(
        b.claim_submission(w, different_document).unwrap_err(),
        PrintError::InvalidOrigin
    );
    let once = b.claim_submission(w, original).unwrap();
    assert_eq!(once.operation(), w);
    assert_eq!(
        b.claim_submission(w, original).unwrap_err(),
        PrintError::InvalidPhase
    );
    assert_eq!(
        b.job_finished(&once, nz(8), PrintOutcome::Completed),
        Err(PrintError::InvalidPhase)
    );
    assert_eq!(b.status(w).unwrap().accepted_job, None);
}

#[test]
fn close_only_cancels_original_browser_and_keeps_unanswered_work_counted() {
    let a = browser(1);
    let b = browser(2);
    let mut q = book(2);
    q.enroll(a).unwrap();
    q.enroll(b).unwrap();
    let first = q.begin(menu(a, 9), origin(a), "A").unwrap();
    let second = q.begin(menu(b, 10), origin(b), "B").unwrap();
    q.close_browser(a).unwrap();
    assert_eq!(q.pending_count(), 2);
    assert!(q.status(first).unwrap().cancel_requested);
    assert!(!q.status(second).unwrap().cancel_requested);
    assert_eq!(q.confirm(first, origin(a)), Err(PrintError::Closing));
    assert_eq!(
        q.begin(menu(a, 11), origin(a), "late A").unwrap_err().error,
        PrintError::Closing
    );
    assert_eq!(q.enroll(browser(3)), Err(PrintError::Backpressure));
    q.no_job_before_claim(first, NoJobOutcome::Cancelled)
        .unwrap();
    assert!(q.take_retired(first).is_none());
    assert_eq!(q.resources_retired(first), Ok(true));
    assert_eq!(q.resources_retired(first), Ok(false));
    assert_eq!(q.take_retired(first).unwrap().payload, "A");
    assert_eq!(q.pending_count(), 1);
    assert_eq!(q.enroll(a), Err(PrintError::Replay));
    q.confirm(second, origin(b)).unwrap();
    assert!(q.claim_submission(second, origin(b)).is_ok());
    q.enroll(browser(3)).unwrap();
}

#[test]
fn drain_preserves_claim_across_late_acceptance_and_actual_provider_terminal() {
    let a = browser(1);
    let mut q = book(2);
    q.enroll(a).unwrap();
    let id = q.begin(menu(a, 14), origin(a), "own spool token").unwrap();
    q.confirm(id, origin(a)).unwrap();
    let claim = q.claim_submission(id, origin(a)).unwrap();
    q.begin_drain();
    assert!(q.status(id).unwrap().cancel_requested);
    assert_eq!(q.enroll(browser(2)), Err(PrintError::Closing));
    assert_eq!(
        q.no_job_before_claim(id, NoJobOutcome::Cancelled),
        Err(PrintError::InvalidPhase)
    );
    assert_eq!(
        q.job_finished(&claim, nz(7), PrintOutcome::Cancelled),
        Err(PrintError::InvalidPhase)
    );
    // Model a provider call already entering before close, then a real job.
    q.job_accepted(&claim, nz(7)).unwrap();
    assert_eq!(q.job_accepted(&claim, nz(9)), Err(PrintError::InvalidPhase));
    assert_eq!(
        q.no_job_from_claim(&claim, NoJobOutcome::Cancelled),
        Err(PrintError::InvalidPhase)
    );
    assert_eq!(
        q.job_finished(&claim, nz(6), PrintOutcome::Completed),
        Err(PrintError::Unknown)
    );
    q.resources_retired(id).unwrap();
    assert_eq!(q.pending_count(), 1);
    assert!(q.take_retired(id).is_none());
    q.job_finished(&claim, nz(7), PrintOutcome::Completed)
        .unwrap();
    assert_eq!(
        q.job_finished(&claim, nz(7), PrintOutcome::Cancelled),
        Err(PrintError::InvalidPhase)
    );
    let done = q.take_retired(id).unwrap();
    assert_eq!(done.payload, "own spool token");
    assert_eq!(done.status.outcome, Some(PrintOutcome::Completed));
    assert_eq!(done.status.accepted_job, Some(nz(7)));
    assert!(done.status.cancel_requested);
    assert_eq!(q.pending_count(), 0);
}

fn opened_claim(q: &mut PrintBook<()>, a: BrowserId, sequence: u64) -> (PrintId, PrintSubmission) {
    let id = q.begin(menu(a, sequence), origin(a), ()).unwrap();
    q.confirm(id, origin(a)).unwrap();
    let claim = q.claim_submission(id, origin(a)).unwrap();
    (id, claim)
}

#[test]
fn cancel_dispatch_exact_late_acceptance_stays_pending_until_real_outcome() {
    let a = browser(1);
    let mut q = book(1);
    q.enroll(a).unwrap();
    let (id, submission) = opened_claim(&mut q, a, 1);
    let receipt = nz(17);
    q.close_browser(a).unwrap();
    assert!(q.status(id).unwrap().cancel_requested);
    assert_eq!(
        q.claim_cancel_dispatch(&submission, receipt).unwrap_err(),
        PrintError::InvalidPhase
    );
    q.job_accepted(&submission, receipt).unwrap();
    assert_eq!(
        q.claim_cancel_dispatch(&submission, nz(18)).unwrap_err(),
        PrintError::Unknown
    );
    let cancel = q.claim_cancel_dispatch(&submission, receipt).unwrap();
    assert_eq!((cancel.operation(), cancel.receipt()), (id, receipt));
    assert_eq!(
        q.status(id).unwrap().cancel_dispatch,
        PrintCancelDispatch::InFlight
    );
    assert_eq!(
        q.claim_cancel_dispatch(&submission, receipt).unwrap_err(),
        PrintError::Replay
    );
    q.cancel_dispatch_released(&cancel).unwrap();
    assert_eq!(q.cancel_dispatch_released(&cancel), Err(PrintError::Replay));
    assert_eq!(
        q.claim_cancel_dispatch(&submission, receipt).unwrap_err(),
        PrintError::Replay
    );
    q.resources_retired(id).unwrap();
    let status = q.status(id).unwrap();
    assert_eq!(status.phase, PrintPhase::JobAccepted);
    assert_eq!(status.outcome, None);
    assert_eq!(q.pending_count(), 1);
    assert!(q.take_retired(id).is_none());
    q.job_finished(&submission, receipt, PrintOutcome::Cancelled)
        .unwrap();
    assert_eq!(
        q.take_retired(id).unwrap().status.outcome,
        Some(PrintOutcome::Cancelled)
    );
    assert_eq!(q.pending_count(), 0);
}

#[test]
fn cancel_dispatch_retains_operation_through_reentrant_completion_and_retirement() {
    let a = browser(1);
    let mut q = book(1);
    q.enroll(a).unwrap();
    let (id, submission) = opened_claim(&mut q, a, 1);
    let receipt = nz(17);
    q.job_accepted(&submission, receipt).unwrap();
    assert_eq!(
        q.claim_cancel_dispatch(&submission, receipt).unwrap_err(),
        PrintError::InvalidPhase
    );
    assert_eq!(q.request_cancel(id), Ok(true));
    let cancel = q.claim_cancel_dispatch(&submission, receipt).unwrap();
    // Delivery may be synchronous with the provider's cancel call, and the
    // job can finish successfully despite the cancellation intent.
    q.job_finished(&submission, receipt, PrintOutcome::Completed)
        .unwrap();
    q.resources_retired(id).unwrap();
    assert!(q.take_retired(id).is_none());
    assert_eq!(q.pending_count(), 1);
    q.cancel_dispatch_released(&cancel).unwrap();
    let retired = q.take_retired(id).unwrap().status;
    assert_eq!(retired.outcome, Some(PrintOutcome::Completed));
    assert_eq!(retired.cancel_dispatch, PrintCancelDispatch::Released);
    assert_eq!(q.pending_count(), 0);
}

#[test]
fn cancel_dispatch_dropped_claim_does_not_clear_the_retirement_gate() {
    let a = browser(1);
    let mut q = book(1);
    q.enroll(a).unwrap();
    let (id, submission) = opened_claim(&mut q, a, 1);
    let receipt = nz(17);
    q.job_accepted(&submission, receipt).unwrap();
    q.request_cancel(id).unwrap();
    drop(q.claim_cancel_dispatch(&submission, receipt).unwrap());
    assert_eq!(
        q.claim_cancel_dispatch(&submission, receipt).unwrap_err(),
        PrintError::Replay
    );
    q.begin_drain();
    q.resources_retired(id).unwrap();
    q.job_finished(&submission, receipt, PrintOutcome::Cancelled)
        .unwrap();
    assert_eq!(
        q.status(id).unwrap().cancel_dispatch,
        PrintCancelDispatch::InFlight
    );
    assert!(q.take_retired(id).is_none());
    assert!(q.pending_for(a));
}

#[test]
fn cancel_dispatch_rejects_foreign_books_and_reused_receipts() {
    let a = browser(1);
    let mut first = book(1);
    let mut second = book(1);
    first.enroll(a).unwrap();
    second.enroll(a).unwrap();
    let (id, submission) = opened_claim(&mut first, a, 1);
    let (other, other_submission) = opened_claim(&mut second, a, 1);
    assert_eq!(id, other);
    let receipt = nz(17);
    for (q, claim) in [(&mut first, &submission), (&mut second, &other_submission)] {
        q.job_accepted(claim, receipt).unwrap();
        q.request_cancel(id).unwrap();
    }
    assert_eq!(
        second
            .claim_cancel_dispatch(&submission, receipt)
            .unwrap_err(),
        PrintError::Unknown
    );
    let cancel = first.claim_cancel_dispatch(&submission, receipt).unwrap();
    assert_eq!(
        second.cancel_dispatch_released(&cancel),
        Err(PrintError::Unknown)
    );
    assert_eq!(
        second.status(other).unwrap().cancel_dispatch,
        PrintCancelDispatch::NotClaimed
    );
    first.cancel_dispatch_released(&cancel).unwrap();
    first
        .job_finished(&submission, receipt, PrintOutcome::Cancelled)
        .unwrap();
    first.resources_retired(id).unwrap();
    first.take_retired(id).unwrap();

    let (new, new_submission) = opened_claim(&mut first, a, 2);
    first.job_accepted(&new_submission, receipt).unwrap();
    first.resources_retired(new).unwrap();
    assert_eq!(
        first
            .claim_cancel_dispatch(&new_submission, receipt)
            .unwrap_err(),
        PrintError::InvalidPhase
    );
    assert_eq!(
        first
            .claim_cancel_dispatch(&submission, receipt)
            .unwrap_err(),
        PrintError::Unknown
    );
    assert_eq!(
        first.cancel_dispatch_released(&cancel),
        Err(PrintError::Unknown)
    );
    assert_eq!(
        first.status(new).unwrap().cancel_dispatch,
        PrintCancelDispatch::NotClaimed
    );
}

#[test]
fn exact_no_job_failure_of_claim_retires_without_a_retry() {
    let a = browser(1);
    let mut q = book(1);
    q.enroll(a).unwrap();
    let id = q.begin(menu(a, 3), origin(a), ()).unwrap();
    q.confirm(id, origin(a)).unwrap();
    let claim = q.claim_submission(id, origin(a)).unwrap();
    q.no_job_from_claim(
        &claim,
        NoJobOutcome::Failed(PrintFailure::DeviceUnavailable),
    )
    .unwrap();
    assert_eq!(q.job_accepted(&claim, nz(7)), Err(PrintError::InvalidPhase));
    assert!(q.take_retired(id).is_none());
    q.resources_retired(id).unwrap();
    let s = q.take_retired(id).unwrap().status;
    assert_eq!(
        s.outcome,
        Some(PrintOutcome::Failed(PrintFailure::DeviceUnavailable))
    );
    assert_eq!(s.accepted_job, None);
    assert_eq!(
        q.begin(menu(a, 3), origin(a), ()).unwrap_err().error,
        PrintError::Replay
    );
}

#[test]
fn old_menu_and_operation_replies_cannot_affect_a_new_operation() {
    let a = browser(1);
    let mut q = book(1);
    q.enroll(a).unwrap();
    let old = q.begin(menu(a, 13), origin(a), 1).unwrap();
    q.confirm(old, origin(a)).unwrap();
    let claim = q.claim_submission(old, origin(a)).unwrap();
    q.job_accepted(&claim, nz(1)).unwrap();
    q.job_finished(&claim, nz(1), PrintOutcome::Cancelled)
        .unwrap();
    q.resources_retired(old).unwrap();
    q.take_retired(old).unwrap();
    let new = q.begin(menu(a, 14), origin(a), 2).unwrap();
    assert!(new.sequence > old.sequence);
    assert_eq!(q.request_cancel(old), Err(PrintError::Unknown));
    assert_eq!(q.confirm(old, origin(a)), Err(PrintError::Unknown));
    assert_eq!(
        q.job_finished(&claim, nz(1), PrintOutcome::Completed),
        Err(PrintError::Unknown)
    );
    assert_eq!(
        q.begin(menu(a, 13), origin(a), 3).unwrap_err().error,
        PrintError::Replay
    );
    assert_eq!(
        q.status(new).unwrap().phase,
        PrintPhase::AwaitingConfirmation
    );
    assert!(!q.status(new).unwrap().cancel_requested);
}

#[test]
fn issued_claim_for_another_book_is_rejected_even_for_equal_numeric_ids() {
    let a = browser(1);
    let mut first = book(1);
    let mut second = book(1);
    first.enroll(a).unwrap();
    second.enroll(a).unwrap();
    let id = first.begin(menu(a, 1), origin(a), ()).unwrap();
    assert_eq!(second.begin(menu(a, 1), origin(a), ()).unwrap(), id);
    first.confirm(id, origin(a)).unwrap();
    second.confirm(id, origin(a)).unwrap();
    let claim = first.claim_submission(id, origin(a)).unwrap();
    let other = second.claim_submission(id, origin(a)).unwrap();
    assert_eq!(second.job_accepted(&claim, nz(1)), Err(PrintError::Unknown));
    assert_eq!(first.job_accepted(&other, nz(1)), Err(PrintError::Unknown));
    assert_eq!(first.status(id).unwrap().accepted_job, None);
    assert_eq!(second.status(id).unwrap().accepted_job, None);
}

#[test]
fn payload_return_does_not_drop_opaque_data_until_caller_releases_it() {
    #[derive(Debug)]
    struct Watch(Arc<AtomicUsize>);
    impl Drop for Watch {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let a = browser(1);
    let mut q = book(1);
    q.enroll(a).unwrap();
    let id = q
        .begin(menu(a, 8), origin(a), Watch(drops.clone()))
        .unwrap();
    let bad = q
        .begin(menu(a, 9), origin(a), Watch(drops.clone()))
        .unwrap_err();
    assert_eq!(bad.error, PrintError::Backpressure);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(bad.payload);
    q.no_job_before_claim(id, NoJobOutcome::Cancelled).unwrap();
    q.resources_retired(id).unwrap();
    let done = q.take_retired(id).unwrap();
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    drop(done.payload);
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

#[test]
fn unknown_claim_survives_token_drop_and_no_job_cannot_be_synthesized() {
    let a = browser(1);
    let mut q = book(1);
    q.enroll(a).unwrap();
    let id = q.begin(menu(a, 1), origin(a), ()).unwrap();
    q.confirm(id, origin(a)).unwrap();
    let claim = q.claim_submission(id, origin(a)).unwrap();
    drop(claim);
    q.begin_drain();
    q.resources_retired(id).unwrap();
    assert_eq!(
        q.no_job_before_claim(id, NoJobOutcome::Cancelled),
        Err(PrintError::InvalidPhase)
    );
    assert_eq!(q.pending_count(), 1);
    assert_eq!(q.status(id).unwrap().phase, PrintPhase::SubmissionClaimed);
    assert!(q.take_retired(id).is_none());
}

#[test]
fn invalidation_matches_registration_without_permanently_closing_browser() {
    let a = browser(1);
    let mut q = book(1);
    q.enroll(a).unwrap();
    let first = q.begin(menu(a, 1), origin(a), ()).unwrap();
    assert!(q.pending_for(a));
    assert!(!q.invalidate_browser(a, Some(nz(2))));
    assert!(!q.invalidate_browser(browser(2), None));
    assert!(!q.status(first).unwrap().cancel_requested);
    assert!(q.invalidate_browser(a, Some(origin(a).registration)));
    assert!(!q.invalidate_browser(a, None));
    assert_eq!(q.confirm(first, origin(a)), Err(PrintError::Closing));
    q.no_job_before_claim(first, NoJobOutcome::Cancelled)
        .unwrap();
    q.resources_retired(first).unwrap();
    q.take_retired(first).unwrap();
    assert!(!q.pending_for(a));
    let next = PrintOrigin {
        registration: nz(4),
        ..origin(a)
    };
    let second = q.begin(menu(a, 2), next, ()).unwrap();
    assert!(!q.invalidate_browser(a, Some(origin(a).registration)));
    q.confirm(second, next).unwrap();
}

#[test]
fn rejects_foreign_browser_and_sequence_exhaustion_without_corrupting_active_row() {
    let a = browser(1);
    let foreign = BrowserId::new(RuntimeGeneration::new(2), 1);
    let mut q = book(1);
    assert_eq!(q.enroll(foreign), Err(PrintError::WrongRuntime));
    q.enroll(a).unwrap();
    assert_eq!(
        q.begin(menu(a, 0), origin(a), ()).unwrap_err().error,
        PrintError::InvalidOrigin
    );
    assert_eq!(
        q.begin(menu(a, 1), origin(foreign), ()).unwrap_err().error,
        PrintError::InvalidOrigin
    );
    q.next = u64::MAX;
    let last = q.begin(menu(a, 1), origin(a), ()).unwrap();
    assert_eq!(last.sequence.get(), u64::MAX);
    q.no_job_before_claim(last, NoJobOutcome::Cancelled)
        .unwrap();
    q.resources_retired(last).unwrap();
    q.take_retired(last).unwrap();
    assert_eq!(
        q.begin(menu(a, 2), origin(a), ()).unwrap_err().error,
        PrintError::Exhausted
    );
    assert_eq!(q.pending_count(), 0);
}
