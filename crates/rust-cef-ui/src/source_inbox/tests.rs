use super::*;
use rust_cef_core::{
    contracts::RuntimeGeneration, menu::ContextMenuRequestId, menu_context::MenuContext,
    menu_lease::MenuLeaseOwner, ports::WindowBinding,
};
use std::sync::{Mutex, Weak};
fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}
fn setup(max: usize) -> (MenuLeaseOwner, Arc<Inbox>) {
    let binding = WindowBinding {
        browser: BrowserId::new(RuntimeGeneration::new(7), 3),
        window: 11,
    };
    let owner = MenuLeaseOwner::new(binding, nz(1)).unwrap();
    let inbox = Inbox::new(binding, owner.lease(), nz(1), nz(max));
    (owner, inbox)
}
struct Control {
    inbox: Weak<Inbox>,
    state: Arc<Mutex<SourceOperationStatus>>,
}
impl Control {
    fn unlocked(&self) {
        if let Some(inbox) = self.inbox.upgrade() {
            assert!(
                inbox.state.try_lock().is_ok(),
                "opaque endpoint under inbox mutex"
            );
        }
    }
}
impl Drop for Control {
    fn drop(&mut self) {
        self.unlocked();
    }
}
impl SourceViewControl for Control {
    fn status(&self) -> SourceOperationStatus {
        self.unlocked();
        self.state.lock().unwrap().clone()
    }
    fn is_current(&self) -> bool {
        self.unlocked();
        true
    }
    fn presented(&self) -> Result<(), EngineError> {
        self.unlocked();
        self.state.lock().unwrap().phase = SourcePhase::Presented;
        Ok(())
    }
    fn closed(&self) -> Result<(), EngineError> {
        self.unlocked();
        let mut q = self.state.lock().unwrap();
        q.host_live = false;
        q.phase = SourcePhase::Finished;
        q.outcome = Some(SourceOutcome::Closed);
        Ok(())
    }
    fn request_close(&self) -> Result<(), EngineError> {
        self.unlocked();
        let mut q = self.state.lock().unwrap();
        q.cancelled = true;
        q.phase = SourcePhase::Closing;
        Ok(())
    }
}
fn control(inbox: &Arc<Inbox>, sequence: u64) -> Arc<Control> {
    Arc::new(Control {
        inbox: Arc::downgrade(inbox),
        state: Arc::new(Mutex::new(SourceOperationStatus {
            operation: SourceOperationId {
                browser: inbox.binding.browser,
                sequence,
            },
            menu: ContextMenuRequestId {
                browser: inbox.binding.browser,
                sequence: sequence + 10,
            },
            phase: SourcePhase::Fetching,
            native_live: true,
            host_live: false,
            callback_received: false,
            cancelled: false,
            source_bytes: 0,
            first_failure: None,
            first_close_failure: None,
            outcome: None,
        })),
    })
}
fn queue(c: &Control, text: &str) -> SourceViewRequest {
    let inbox = c.inbox.upgrade().unwrap();
    let mut q = c.state.lock().unwrap();
    q.phase = SourcePhase::Queued;
    q.host_live = true;
    q.source_bytes = text.len();
    SourceViewRequest {
        operation: q.operation,
        menu: q.menu,
        context: Arc::new(MenuContext {
            binding: inbox.binding,
            binding_revision: 1,
            x: 0,
            y: 0,
            frame_id: "original-frame".into(),
            selection: String::new(),
            misspelled_word: String::new(),
            dictionary_suggestions: Vec::new(),
        }),
        source: SourceText::new(text.to_owned(), nz(1024)).unwrap(),
    }
}
#[test]
fn receipt_capacity_retirement_and_alias_wrappers_never_invoke_opaque_ports_under_lock() {
    let (_owner, inbox) = setup(32);
    let c = control(&inbox, 1);
    let q = c.status();
    inbox.started(q.operation, q.menu, c.clone()).unwrap();
    assert!(inbox.take_document(q.operation).is_none());
    let next = control(&inbox, 2);
    let n = next.status();
    assert_eq!(
        inbox.started(n.operation, n.menu, next),
        Err(EngineError::Backpressure)
    );
    let doc = queue(&c, "<literal>& plain text");
    let alias = Arc::new(Control {
        inbox: Arc::downgrade(&inbox),
        state: c.state.clone(),
    });
    assert!(!Arc::ptr_eq(&c, &alias));
    inbox.publish(doc, alias.clone()).unwrap();
    inbox.close(q.operation, alias).unwrap();
    assert!(inbox.closing(q.operation));
    let doc = inbox.take_document(q.operation).unwrap();
    assert_eq!(doc.source.as_str(), "<literal>& plain text");
    drop(doc);
    let slot = inbox.remove(q.operation);
    drop(slot);
    inbox.retire();
    let later = control(&inbox, 3);
    let l = later.status();
    assert_eq!(
        inbox.started(l.operation, l.menu, later),
        Err(EngineError::Closed)
    );
}
#[test]
fn stale_binding_budget_and_duplicate_delivery_never_replace_the_original_payload() {
    let (mut owner, inbox) = setup(3);
    let c = control(&inbox, 1);
    let q = c.status();
    inbox.started(q.operation, q.menu, c.clone()).unwrap();
    assert_eq!(
        inbox.publish(queue(&c, "long"), c.clone()),
        Err(EngineError::InvalidRequest)
    );
    assert!(inbox.take_document(q.operation).is_none());
    let mut wrong = queue(&c, "abc");
    Arc::make_mut(&mut wrong.context).binding.window += 1;
    assert_eq!(
        inbox.publish(wrong, c.clone()),
        Err(EngineError::InvalidRequest)
    );
    inbox.publish(queue(&c, "abc"), c.clone()).unwrap();
    assert_eq!(
        inbox.publish(queue(&c, "def"), c.clone()),
        Err(EngineError::InvalidRequest)
    );
    assert_eq!(
        inbox.take_document(q.operation).unwrap().source.as_str(),
        "abc"
    );
    owner.invalidate().unwrap();
    assert_eq!(
        inbox.publish(queue(&c, "abc"), c),
        Err(EngineError::InvalidRequest)
    );
}
#[test]
fn original_receipt_not_new_wrapper_metadata_authorizes_source_publication() {
    let (_owner, inbox) = setup(32);
    let c = control(&inbox, 1);
    let q = c.status();
    inbox.started(q.operation, q.menu, c.clone()).unwrap();
    let imposter = control(&inbox, 1);
    let doc = queue(&imposter, "inert");
    assert_eq!(
        inbox.publish(doc, imposter),
        Err(EngineError::InvalidRequest)
    );
    assert_eq!(c.status().phase, SourcePhase::Fetching);
    assert!(inbox.take_document(q.operation).is_none());
    let mut stale = q.operation;
    stale.sequence += 100;
    assert_eq!(
        inbox.close(stale, c.clone()),
        Err(EngineError::InvalidRequest)
    );
    assert!(!inbox.closing(q.operation));
}
#[test]
fn publication_consumption_and_normal_dispatch_never_reset_one_shot_ack_or_delivery() {
    let (_owner, inbox) = setup(32);
    let c = control(&inbox, 1);
    let q = c.status();
    inbox.started(q.operation, q.menu, c.clone()).unwrap();
    inbox.publish(queue(&c, "literal"), c.clone()).unwrap();
    drop(inbox.take_document(q.operation));
    assert_eq!(
        inbox.publish(queue(&c, "second"), c.clone()),
        Err(EngineError::InvalidRequest)
    );
    assert!(inbox.reserve_ack(q.operation, false));
    assert!(!inbox.reserve_ack(q.operation, false));
    assert!(!inbox.reserve_ack(q.operation, false));
    inbox.reset_ack(q.operation);
    assert!(inbox.reserve_ack(q.operation, false));
    let old = inbox.remove(q.operation);
    drop(old);
    assert!(!inbox.reserve_ack(q.operation, true));
}
