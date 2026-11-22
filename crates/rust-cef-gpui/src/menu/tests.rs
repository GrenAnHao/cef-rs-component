use super::*;
use rust_cef_core::ports::WindowBinding;
use rust_cef_core::{
    contracts::RuntimeGeneration,
    menu_context::MenuContext,
    menu_model::{MenuCommand, MenuModel, MenuTreeError},
};
use std::sync::atomic::{AtomicU64, Ordering};
fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}
pub(super) fn binding() -> WindowBinding {
    WindowBinding {
        browser: BrowserId::new(RuntimeGeneration::new(7), 3),
        window: 11,
    }
}
pub(super) fn setup() -> (MenuLeaseOwner, Arc<Inbox>) {
    let owner = MenuLeaseOwner::new(binding(), nz(1)).unwrap();
    let inbox = Inbox::new(
        owner.lease(),
        MenuLimits {
            max_entries: nz(8),
            max_label_bytes: nz(16),
            max_total_label_bytes: nz(32),
        },
        MenuContextLimits {
            max_frame_id_bytes: nz(16),
            max_text_bytes: nz(16),
            max_suggestions: nz(2),
            max_total_bytes: nz(32),
        },
    );
    inbox.set_ready(true).unwrap();
    (owner, inbox)
}
pub(super) fn presentation(sequence: u64) -> MenuPresentation {
    let command = |id, label: &str, enabled| {
        MenuItem::Command(MenuCommand {
            command_id: id,
            label: label.into(),
            visible: true,
            enabled,
            checked: None,
            radio_group: None,
        })
    };
    MenuPresentation {
        request: ContextMenuRequestId {
            browser: binding().browser,
            sequence,
        },
        model: MenuModel {
            entries: vec![command(1, "active", true), command(2, "disabled", false)],
            x: 2,
            y: 3,
        },
        context: Arc::new(MenuContext {
            binding: binding(),
            binding_revision: 1,
            x: 2,
            y: 3,
            frame_id: "frame".into(),
            selection: String::new(),
            misspelled_word: String::new(),
            dictionary_suggestions: Vec::new(),
        }),
    }
}
struct DropSpy {
    inbox: SyncWeak<Inbox>,
    drops: Arc<AtomicU64>,
}
impl ContextMenuResponseSink for DropSpy {
    fn submit(&self, _: ContextMenuRequestId, _: ContextMenuResponse) -> Result<(), EngineError> {
        Ok(())
    }
}
impl Drop for DropSpy {
    fn drop(&mut self) {
        if let Some(i) = self.inbox.upgrade() {
            assert!(
                i.state.try_lock().is_ok(),
                "endpoint dropped while delivery mutex held"
            );
        }
        self.drops.fetch_add(1, Ordering::AcqRel);
    }
}
pub(super) fn spy(inbox: &Arc<Inbox>, drops: &Arc<AtomicU64>) -> Arc<dyn ContextMenuResponseSink> {
    Arc::new(DropSpy {
        inbox: Arc::downgrade(inbox),
        drops: drops.clone(),
    })
}
#[test]
fn registration_identity_budgets_and_terminal_watermark_preserve_new_delivery() {
    let (_owner, inbox) = setup();
    let drops = Arc::new(AtomicU64::new(0));
    let stamp = inbox.capture().unwrap();
    inbox
        .publish(presentation(1), spy(&inbox, &drops), stamp.clone())
        .unwrap();
    let mut invalid = presentation(2);
    let MenuItem::Command(c) = &mut invalid.model.entries[0] else {
        unreachable!()
    };
    c.label.clear();
    assert_eq!(
        inbox.publish(invalid, spy(&inbox, &drops), stamp.clone()),
        Err(MenuDeliveryError::Model(MenuTreeError::EmptyLabel))
    );
    let other = MenuLeaseOwner::new(binding(), nz(1)).unwrap();
    // is_current is intentionally a boolean observation. Allocation identity
    // remains independently rejected by execution admission, not guessed from
    // the delivery error classification.
    assert_eq!(
        inbox
            .lease
            .try_enter(&other.lease().capture().unwrap())
            .err(),
        Some(MenuLeaseError::WrongLease)
    );
    assert_eq!(
        inbox.publish(
            presentation(2),
            spy(&inbox, &drops),
            other.lease().capture().unwrap()
        ),
        Err(MenuDeliveryError::Stale)
    );
    assert_eq!(
        inbox
            .state
            .lock()
            .unwrap()
            .pending
            .as_ref()
            .unwrap()
            .presentation
            .request
            .sequence,
        1
    );
    let before = drops.load(Ordering::Acquire);
    inbox
        .publish(presentation(2), spy(&inbox, &drops), stamp.clone())
        .unwrap();
    assert_eq!(drops.load(Ordering::Acquire), before + 1);
    inbox.dismiss(presentation(1).request).unwrap();
    assert_eq!(
        inbox
            .state
            .lock()
            .unwrap()
            .pending
            .as_ref()
            .unwrap()
            .presentation
            .request
            .sequence,
        2
    );
    assert_eq!(
        inbox.publish(presentation(1), spy(&inbox, &drops), stamp),
        Err(MenuDeliveryError::Stale)
    );
    inbox.dismiss(presentation(2).request).unwrap();
    assert!(inbox.state.lock().unwrap().pending.is_none());
}
struct ReplySpy {
    inbox: SyncWeak<Inbox>,
    selects: AtomicU64,
    cancels: AtomicU64,
}
impl ContextMenuResponseSink for ReplySpy {
    fn submit(
        &self,
        _: ContextMenuRequestId,
        response: ContextMenuResponse,
    ) -> Result<(), EngineError> {
        assert!(
            self.inbox.upgrade().unwrap().state.try_lock().is_ok(),
            "external submit under delivery mutex"
        );
        match response {
            ContextMenuResponse::Select { .. } => {
                self.selects.fetch_add(1, Ordering::AcqRel);
                Ok(())
            }
            ContextMenuResponse::Cancel => {
                if self.cancels.fetch_add(1, Ordering::AcqRel) == 0 {
                    Err(EngineError::Backpressure)
                } else {
                    Ok(())
                }
            }
        }
    }
}
#[test]
fn scoped_reply_and_explicit_cancel_retry_preserve_first_failure_without_hidden_posts() {
    let (mut owner, inbox) = setup();
    let stamp = inbox.capture().unwrap();
    let raw = Arc::new(ReplySpy {
        inbox: Arc::downgrade(&inbox),
        selects: AtomicU64::new(0),
        cancels: AtomicU64::new(0),
    });
    inbox
        .publish(presentation(1), raw.clone(), stamp.clone())
        .unwrap();
    let (delivery, _) = inbox.take_update().unwrap();
    let d = delivery.unwrap();
    let response = ScopedResponse {
        id: d.presentation.request,
        stamp,
        inbox: Arc::downgrade(&inbox),
        allowed: allowed(&d.presentation.model),
        inner: d.response,
    };
    assert_eq!(
        response.submit(
            presentation(2).request,
            ContextMenuResponse::Select { command_id: 1 }
        ),
        Err(EngineError::InvalidRequest)
    );
    assert_eq!(
        response.submit(
            presentation(1).request,
            ContextMenuResponse::Select { command_id: 2 }
        ),
        Err(EngineError::InvalidRequest)
    );
    response
        .submit(
            presentation(1).request,
            ContextMenuResponse::Select { command_id: 1 },
        )
        .unwrap();
    assert_eq!(raw.selects.load(Ordering::Acquire), 1);
    owner.invalidate().unwrap();
    assert_eq!(
        response.submit(
            presentation(1).request,
            ContextMenuResponse::Select { command_id: 1 }
        ),
        Err(EngineError::NotReady)
    );
    // An admitted selection is not native completion, but cleanup must not
    // submit a second terminal response before backend termination arrives.
    inbox.cancel(false).unwrap();
    assert_eq!(raw.cancels.load(Ordering::Acquire), 0);
    inbox.dismiss(presentation(1).request).unwrap();
    let stamp = inbox.capture().unwrap();
    let mut next = presentation(2);
    Arc::make_mut(&mut next.context).binding_revision = stamp.revision();
    inbox.publish(next, raw.clone(), stamp).unwrap();
    assert_eq!(inbox.cancel(false), Err(EngineError::Backpressure));
    assert_eq!(inbox.cancel(false), Err(EngineError::Backpressure));
    assert_eq!(raw.cancels.load(Ordering::Acquire), 1);
    inbox.cancel(true).unwrap();
    inbox.cancel(true).unwrap();
    assert_eq!(raw.cancels.load(Ordering::Acquire), 2);
    let s = inbox.state.lock().unwrap();
    assert_eq!(s.first_cancel_error, Some(EngineError::Backpressure));
    assert!(s.latest.as_ref().unwrap().cancel_error.is_none());
}
#[test]
fn publisher_after_owner_retirement_and_closed_registration_cannot_deliver() {
    use std::{sync::mpsc, thread, time::Duration};
    let (mut owner, inbox) = setup();
    let stamp = inbox.capture().unwrap();
    let worker = inbox.clone();
    let (go, wait) = mpsc::channel();
    let thread = thread::spawn(move || {
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.publish(
            presentation(1),
            spy(&worker, &Arc::new(AtomicU64::new(0))),
            stamp,
        )
    });
    owner.retire().unwrap();
    go.send(()).unwrap();
    assert_eq!(thread.join().unwrap(), Err(MenuDeliveryError::Stale));
    assert!(inbox.state.lock().unwrap().pending.is_none());
    let (_owner, closed) = setup();
    let stamp = closed.capture().unwrap();
    closed.close();
    assert_eq!(closed.capture().unwrap_err(), MenuDeliveryError::Closed);
    assert_eq!(
        closed.publish(
            presentation(1),
            spy(&closed, &Arc::new(AtomicU64::new(0))),
            stamp
        ),
        Err(MenuDeliveryError::Closed)
    );
}
