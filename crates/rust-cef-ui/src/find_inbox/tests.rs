use super::Inbox;
use rust_cef_core::{
    contracts::{BrowserId, RuntimeGeneration},
    find::*,
    menu::ContextMenuRequestId,
    menu_context::MenuContext,
    menu_lease::MenuLeaseOwner,
    ports::{EngineError, WindowBinding},
};
use std::{
    num::NonZeroUsize,
    sync::{Arc, Mutex, Weak},
};
struct Receipt {
    state: Mutex<FindStatus>,
    inbox: Weak<Inbox>,
    hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
impl FindReceipt for Receipt {
    fn status(&self) -> FindStatus {
        if let Some(i) = self.inbox.upgrade() {
            assert!(i.state_for_test());
        }
        let q = self.state.lock().unwrap().clone();
        let hook = { self.hook.lock().unwrap().take() };
        if let Some(f) = hook {
            f();
        }
        q
    }
    fn cancel(&self) -> Result<(), EngineError> {
        assert!(self.inbox.upgrade().unwrap().state_for_test());
        Ok(())
    }
}
impl Drop for Receipt {
    fn drop(&mut self) {
        if let Some(i) = self.inbox.upgrade() {
            assert!(i.state_for_test());
        }
    }
}
fn setup() -> (MenuLeaseOwner, Arc<Inbox>) {
    let browser = BrowserId::new(RuntimeGeneration::new(3), 1);
    let binding = WindowBinding {
        browser,
        window: 11,
    };
    let owner = MenuLeaseOwner::new(binding, NonZeroUsize::new(1).unwrap()).unwrap();
    let inbox = Inbox::new(binding, owner.lease(), NonZeroUsize::new(16).unwrap());
    (owner, inbox)
}
fn request(owner: &MenuLeaseOwner, sequence: u64) -> FindRequest {
    let binding = owner.lease().snapshot().unwrap().binding;
    FindRequest {
        operation: FindOperationId {
            browser: binding.browser,
            sequence,
        },
        menu: ContextMenuRequestId {
            browser: binding.browser,
            sequence,
        },
        context: Arc::new(MenuContext {
            binding,
            binding_revision: owner.lease().snapshot().unwrap().revision,
            x: 0,
            y: 0,
            frame_id: "f".into(),
            selection: "word".into(),
            misspelled_word: String::new(),
            dictionary_suggestions: Vec::new(),
        }),
    }
}
fn receipt(inbox: &Arc<Inbox>, q: &FindRequest) -> Arc<Receipt> {
    Arc::new(Receipt {
        state: Mutex::new(FindStatus {
            operation: q.operation,
            menu: q.menu,
            phase: FindPhase::Admitted,
            query_bytes: 4,
            issued: false,
            cancel_requested: false,
            stop_attempted: false,
            stop_submitted: false,
            last_result: None,
            outcome: None,
            first_failure: None,
        }),
        inbox: Arc::downgrade(inbox),
        hook: Mutex::new(None),
    })
}
fn result(receipt: &Receipt, final_update: bool) -> FindResult {
    let mut q = receipt.state.lock().unwrap();
    let result = FindResult {
        operation: q.operation,
        cef_identifier: 17,
        matches: 1,
        active_ordinal: 0,
        selection_rect: None,
        final_update,
    };
    q.issued = true;
    q.phase = FindPhase::Searching;
    q.last_result = Some(result.clone());
    result
}
fn finish(receipt: &Receipt) -> FindStatus {
    let mut q = receipt.state.lock().unwrap();
    q.phase = FindPhase::Finished;
    q.outcome = Some(FindOutcome::Result);
    q.clone()
}
#[test]
fn terminal_slot_requires_explicit_delivery_and_releases_original_receipt_outside_lock() {
    let (owner, inbox) = setup();
    let q = request(&owner, 1);
    let r = receipt(&inbox, &q);
    inbox.started(q, r.clone()).unwrap();
    inbox.update(result(&r, true)).unwrap();
    inbox.finished(finish(&r)).unwrap();
    let next = request(&owner, 2);
    let next_r = receipt(&inbox, &next);
    assert_eq!(
        inbox.started(next.clone(), next_r.clone()),
        Err(EngineError::Backpressure)
    );
    assert_eq!(inbox.snapshot().finished, 1);
    assert_eq!(
        inbox.take_update().unwrap().outcome,
        Some(FindOutcome::Result)
    );
    r.state.lock().unwrap().first_failure = Some(FindFailure::Wake(EngineError::Backpressure));
    assert!(inbox.snapshot().last.unwrap().first_failure.is_some());
    let weak = Arc::downgrade(&r);
    drop(r);
    inbox.started(next, next_r).unwrap();
    assert!(weak.upgrade().is_none());
    assert_eq!(inbox.snapshot().started, 2);
}
#[test]
fn original_receipt_and_revision_guard_reject_forgery_without_consuming_newer_update() {
    let (owner, inbox) = setup();
    let q = request(&owner, 1);
    let r = receipt(&inbox, &q);
    inbox.started(q, r.clone()).unwrap();
    let mut original = result(&r, false);
    original.matches = -1;
    original.active_ordinal = -1;
    r.state.lock().unwrap().last_result = Some(original.clone());
    inbox.update(original.clone()).unwrap();
    let mut fake = original.clone();
    fake.matches = 999;
    assert_eq!(inbox.update(fake), Err(EngineError::InvalidRequest));
    let i = inbox.clone();
    let rr = r.clone();
    *r.hook.lock().unwrap() = Some(Box::new(move || {
        let updated = result(&rr, true);
        i.update(updated).unwrap();
    }));
    assert!(inbox.take_update().is_none());
    assert_eq!(inbox.snapshot().delivered, 0);
    assert!(
        inbox
            .take_update()
            .unwrap()
            .last_result
            .unwrap()
            .final_update
    );
}
#[test]
fn retired_owner_stops_admission_and_ui_delivery_but_accepts_exact_native_completion() {
    let (mut owner, inbox) = setup();
    let q = request(&owner, 1);
    let r = receipt(&inbox, &q);
    inbox.started(q, r.clone()).unwrap();
    inbox.retire();
    owner.retire().unwrap();
    inbox.update(result(&r, true)).unwrap();
    inbox.finished(finish(&r)).unwrap();
    assert!(inbox.take_update().is_none());
    let s = inbox.snapshot();
    assert!(s.retired && !s.active);
    assert_eq!(s.finished, 1);
    assert_eq!(s.delivered, 0);
    let next = request(&owner, 2);
    let next_r = receipt(&inbox, &next);
    assert_eq!(inbox.started(next, next_r), Err(EngineError::Closed));
}
