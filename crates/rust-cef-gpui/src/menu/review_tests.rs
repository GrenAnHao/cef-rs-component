use super::tests::{binding, presentation, setup, spy};
use super::*;
use std::sync::atomic::AtomicBool;
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
#[test]
fn stale_delivery_and_terminal_reentry_never_cancel_a_newer_opening() {
    let (mut owner, inbox) = setup();
    let stamp = inbox.capture().unwrap();
    let drops = Arc::new(AtomicU64::new(0));
    inbox
        .publish(presentation(1), spy(&inbox, &drops), stamp.clone())
        .unwrap();
    let (old, _) = inbox.take_update().unwrap();
    let old = old.unwrap();
    assert!(
        inbox
            .live_request(old.presentation.request, &old.stamp)
            .unwrap()
    );
    inbox.dismiss(old.presentation.request).unwrap();
    assert!(
        !inbox
            .live_request(old.presentation.request, &old.stamp)
            .unwrap()
    );
    owner.invalidate().unwrap();
    let stamp = inbox.capture().unwrap();
    let mut p = presentation(2);
    Arc::make_mut(&mut p.context).binding_revision = stamp.revision();
    let raw = Arc::new(Counting(AtomicU64::new(0)));
    inbox.publish(p, raw.clone(), stamp.clone()).unwrap();
    let revision = owner.lease().snapshot().unwrap().revision;
    assert!(
        inbox
            .apply_browser_return(old.presentation.request, &old.stamp, &mut owner, true)
            .is_none()
    );
    assert_eq!(owner.lease().snapshot().unwrap().revision, revision);
    let cancel = inbox
        .apply_browser_return(presentation(2).request, &stamp, &mut owner, true)
        .unwrap();
    assert_eq!(cancel, presentation(2).request);
    let next_stamp = inbox.capture().unwrap();
    let mut next = presentation(3);
    Arc::make_mut(&mut next.context).binding_revision = next_stamp.revision();
    inbox
        .publish(next, raw.clone(), next_stamp.clone())
        .unwrap();
    // The exact cancelled id was captured atomically. A new producer arriving
    // before the lock-free post must not get cancelled in its place.
    inbox.cancel_id(cancel, false).unwrap();
    inbox.cancel_id(old.presentation.request, false).unwrap();
    assert_eq!(raw.0.load(Ordering::Acquire), 0);
    assert!(
        inbox
            .live_request(presentation(3).request, &next_stamp)
            .unwrap()
    );
    assert!(
        !inbox
            .live_request(old.presentation.request, &old.stamp)
            .unwrap_or(false)
    );
}
struct Counting(AtomicU64);
impl ContextMenuResponseSink for Counting {
    fn submit(&self, _: ContextMenuRequestId, _: ContextMenuResponse) -> Result<(), EngineError> {
        self.0.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}
struct Blocking {
    calls: AtomicU64,
    entered: mpsc::SyncSender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}
impl ContextMenuResponseSink for Blocking {
    fn submit(&self, _: ContextMenuRequestId, _: ContextMenuResponse) -> Result<(), EngineError> {
        if self.calls.fetch_add(1, Ordering::AcqRel) == 0 {
            return Err(EngineError::Backpressure);
        }
        self.entered.send(()).unwrap();
        self.release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        Ok(())
    }
}
#[test]
fn cleanup_receipt_survives_owner_drop_and_serializes_concurrent_explicit_retries() {
    let (owner, inbox) = setup();
    let stamp = inbox.capture().unwrap();
    let (entered, observed) = mpsc::sync_channel(1);
    let (release, wait) = mpsc::channel();
    let raw = Arc::new(Blocking {
        calls: AtomicU64::new(0),
        entered,
        release: Mutex::new(wait),
    });
    inbox.publish(presentation(1), raw.clone(), stamp).unwrap();
    let cleanup = MenuCleanup {
        inbox: inbox.clone(),
    };
    drop(owner);
    inbox.close();
    assert!(cleanup.status().unwrap().lease.retired);
    assert_eq!(
        cleanup.status().unwrap().cancellation_error,
        Some(EngineError::Backpressure)
    );
    let worker = cleanup.clone();
    let worker = thread::spawn(move || worker.retry());
    observed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(cleanup.status().unwrap().submission_in_flight);
    assert_eq!(cleanup.retry(), Err(EngineError::Backpressure));
    assert_eq!(raw.calls.load(Ordering::Acquire), 2);
    release.send(()).unwrap();
    worker.join().unwrap().unwrap();
    let s = cleanup.status().unwrap();
    assert!(s.awaiting_terminal);
    assert!(!s.submission_in_flight);
    assert!(s.cancellation_error.is_none());
    assert_eq!(s.first_cancellation_error, Some(EngineError::Backpressure));
    inbox.dismiss(presentation(1).request).unwrap();
    assert!(!cleanup.status().unwrap().awaiting_terminal);
    assert_eq!(binding().browser, presentation(1).request.browser);
}

#[test]
fn cleanup_receipt_preserves_first_lease_error_after_owner_retirement() {
    let (owner, inbox) = setup();
    let cleanup = MenuCleanup {
        inbox: inbox.clone(),
    };
    // Tests diagnostic storage, not a fabricated native/revision-overflow run.
    inbox.record_lease_error(MenuLeaseError::RevisionExhausted);
    inbox.record_lease_error(MenuLeaseError::Invariant);
    drop(owner);
    inbox.close();
    drop(inbox);
    let status = cleanup.status().unwrap();
    assert!(status.lease.retired);
    assert_eq!(
        status.first_lease_error,
        Some(MenuLeaseError::RevisionExhausted)
    );
}
struct ReturningSelection {
    inbox: SyncWeak<Inbox>,
    stamp: MenuLeaseStamp,
    refuse: bool,
    observed: AtomicBool,
    cancels: AtomicU64,
}
impl ContextMenuResponseSink for ReturningSelection {
    fn submit(
        &self,
        id: ContextMenuRequestId,
        response: ContextMenuResponse,
    ) -> Result<(), EngineError> {
        let inbox = self.inbox.upgrade().unwrap();
        assert!(inbox.state.try_lock().is_ok());
        match response {
            ContextMenuResponse::Select { .. } => {
                assert_eq!(inbox.returned_to_browser(id, &self.stamp), Some(true));
                self.observed.store(true, Ordering::Release);
                if self.refuse {
                    Err(EngineError::Backpressure)
                } else {
                    Ok(())
                }
            }
            ContextMenuResponse::Cancel => {
                self.cancels.fetch_add(1, Ordering::AcqRel);
                Ok(())
            }
        }
    }
}
#[test]
fn own_focus_return_distinguishes_select_admission_refusal_and_cancel() {
    for refuse in [false, true] {
        let (mut owner, inbox) = setup();
        let stamp = inbox.capture().unwrap();
        let raw = Arc::new(ReturningSelection {
            inbox: Arc::downgrade(&inbox),
            stamp: stamp.clone(),
            refuse,
            observed: AtomicBool::new(false),
            cancels: AtomicU64::new(0),
        });
        inbox
            .publish(presentation(1), raw.clone(), stamp.clone())
            .unwrap();
        let (d, _) = inbox.take_update().unwrap();
        let d = d.unwrap();
        let response = ScopedResponse {
            id: d.presentation.request,
            stamp: stamp.clone(),
            inbox: Arc::downgrade(&inbox),
            allowed: allowed(&d.presentation.model),
            inner: d.response,
        };
        let result = response.submit(
            presentation(1).request,
            ContextMenuResponse::Select { command_id: 1 },
        );
        assert!(raw.observed.load(Ordering::Acquire));
        let cleanup = MenuCleanup {
            inbox: inbox.clone(),
        };
        if refuse {
            assert_eq!(result, Err(EngineError::Backpressure));
            assert!(!cleanup.status().unwrap().reply_admitted);
            assert_eq!(
                cleanup.status().unwrap().selection_error,
                Some(EngineError::Backpressure)
            );
            assert!(cleanup.status().unwrap().cancellation_error.is_none());
            assert!(cleanup.status().unwrap().first_cancellation_error.is_none());
            assert!(cleanup.status().unwrap().cleanup_requested);
            assert_eq!(raw.cancels.load(Ordering::Acquire), 0);
            cleanup.retry().unwrap();
            assert_eq!(raw.cancels.load(Ordering::Acquire), 1);
            assert_eq!(
                cleanup.status().unwrap().first_selection_error,
                Some(EngineError::Backpressure)
            );
            // Cancel admission is not successful Select focus permission.
            assert_eq!(
                inbox.returned_to_browser(presentation(1).request, &stamp),
                Some(false)
            );
            let revoked =
                inbox.apply_browser_return(presentation(1).request, &stamp, &mut owner, true);
            assert_eq!(revoked, Some(presentation(1).request));
            assert!(inbox.capture().is_ok());
        } else {
            result.unwrap();
            assert!(cleanup.status().unwrap().reply_admitted);
            assert_eq!(
                inbox.returned_to_browser(presentation(1).request, &stamp),
                Some(true)
            );
            let revision = owner.lease().snapshot().unwrap().revision;
            assert!(
                inbox
                    .apply_browser_return(presentation(1).request, &stamp, &mut owner, true)
                    .is_none()
            );
            assert_eq!(owner.lease().snapshot().unwrap().revision, revision);
            inbox.cancel(false).unwrap();
            assert_eq!(raw.cancels.load(Ordering::Acquire), 0);
        }
        assert_eq!(
            inbox.returned_to_browser(presentation(2).request, &stamp),
            None
        );
    }
}
