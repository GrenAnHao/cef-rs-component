use super::*;
use rust_cef_core::{
    contracts::RuntimeGeneration,
    menu_context::MenuContext,
    menu_lease::MenuLeaseOwner,
    ports::{
        ContextMenuDeliverySink, ContextMenuResponseSink, ContextMenuTarget, MenuDeliveryError,
        WindowBinding,
    },
};
use std::num::NonZeroUsize;
#[test]
fn empty_native_stop_does_not_release_serial_slot_without_final_or_browser_close() {
    let (_owner, book, scope, sink) = setup("");
    let data = begin(&book, &scope, &sink, 1);
    assert!(book.issue(&data));
    let q = status(&data.status);
    assert!(q.issued && q.cancel_requested);
    assert_eq!(q.phase, FindPhase::Cancelling);
    assert!(q.outcome.is_none());
    assert_eq!(book.pending(), 1);
    assert!(matches!(
        book.begin(
            &scope,
            ContextMenuRequestId {
                browser: scope.browser,
                sequence: 2
            },
            &sink
        ),
        Err(EngineError::Backpressure)
    ));
    book.browser_closed(scope.browser);
    assert_eq!(
        status(&data.status).outcome,
        Some(FindOutcome::EmptySelection)
    );
    assert_eq!(book.pending(), 0);
}
#[test]
fn reentrant_close_notifies_only_retirement_winner_and_preserves_late_delivery_error() {
    struct Reentrant {
        book: Weak<Finds>,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl FindResultSink for Reentrant {
        fn started(&self, _: FindRequest, _: Arc<dyn FindReceipt>) -> Result<(), EngineError> {
            Ok(())
        }
        fn update(&self, _: FindResult) -> Result<(), EngineError> {
            Ok(())
        }
        fn finished(&self, q: FindStatus) -> Result<(), EngineError> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            let book = self.book.upgrade().unwrap();
            assert!(book.state.try_lock().is_ok());
            book.browser_closed(q.operation.browser);
            Err(EngineError::Backpressure)
        }
    }
    let (_owner, book, scope, _sink) = setup("word");
    let sink = Arc::new(Reentrant {
        book: Arc::downgrade(&book),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let port: Arc<dyn FindResultSink> = sink.clone();
    let data = begin(&book, &scope, &port, 1);
    book.issue(&data);
    book.result(scope.browser, 17, 1, 1, None, true).unwrap();
    book.finish(&data, FindOutcome::Result);
    book.finish(&data, FindOutcome::Cancelled);
    assert_eq!(sink.calls.load(Ordering::Acquire), 1);
    let q = status(&data.status);
    assert_eq!(q.outcome, Some(FindOutcome::Result));
    assert_eq!(
        q.first_failure,
        Some(FindFailure::Publish(EngineError::Backpressure))
    );
    assert_eq!(book.pending(), 0);
}
fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}
struct Delivery;
impl ContextMenuDeliverySink for Delivery {
    fn capture(&self) -> Result<rust_cef_core::menu_lease::MenuLeaseStamp, MenuDeliveryError> {
        Err(MenuDeliveryError::Closed)
    }
    fn publish(
        &self,
        _: rust_cef_core::menu_context::MenuPresentation,
        _: Arc<dyn ContextMenuResponseSink>,
        _: rust_cef_core::menu_lease::MenuLeaseStamp,
    ) -> Result<(), MenuDeliveryError> {
        Ok(())
    }
    fn dismiss(&self, _: ContextMenuRequestId) -> Result<(), MenuDeliveryError> {
        Ok(())
    }
}
struct Sink {
    book: Weak<Finds>,
    finished: Mutex<Vec<FindStatus>>,
}
impl FindResultSink for Sink {
    fn started(&self, _: FindRequest, _: Arc<dyn FindReceipt>) -> Result<(), EngineError> {
        assert!(self.book.upgrade().unwrap().state.try_lock().is_ok());
        Ok(())
    }
    fn update(&self, _: FindResult) -> Result<(), EngineError> {
        assert!(self.book.upgrade().unwrap().state.try_lock().is_ok());
        Ok(())
    }
    fn finished(&self, q: FindStatus) -> Result<(), EngineError> {
        assert!(self.book.upgrade().unwrap().state.try_lock().is_ok());
        self.finished.lock().unwrap().push(q);
        Ok(())
    }
}
impl Drop for Sink {
    fn drop(&mut self) {
        if let Some(book) = self.book.upgrade() {
            assert!(book.state.try_lock().is_ok());
        }
    }
}
fn setup(
    text: &str,
) -> (
    MenuLeaseOwner,
    Arc<Finds>,
    Arc<Scope>,
    Arc<dyn FindResultSink>,
) {
    let browser = BrowserId::new(RuntimeGeneration::new(7), 1);
    let binding = WindowBinding { browser, window: 9 };
    let owner = MenuLeaseOwner::new(binding, nz(1)).unwrap();
    let delivery: Arc<dyn ContextMenuDeliverySink> = Arc::new(Delivery);
    let target = ContextMenuTarget {
        lease: owner.lease(),
        delivery,
    };
    let scope = Arc::new(Scope {
        browser,
        native_id: 1,
        frame_id: "frame".into(),
        context: Arc::new(MenuContext {
            binding,
            binding_revision: 1,
            x: 0,
            y: 0,
            frame_id: "frame".into(),
            selection: text.into(),
            misspelled_word: String::new(),
            dictionary_suggestions: Vec::new(),
        }),
        lease: target.lease.clone(),
        stamp: target.lease.capture().unwrap(),
        registry: Weak::new(),
        registration: Weak::new(),
        serial: 1,
        epoch: 1,
        spelling_opening: 0,
        delivery: Arc::downgrade(&target.delivery),
        target: crate::menu_bridge::TargetMenuContext::default(),
        synthesized_target: false,
    });
    let book = Finds::new();
    book.configure(FindLimits {
        capacity: nz(1),
        max_query_bytes: nz(32),
    })
    .unwrap();
    let sink: Arc<dyn FindResultSink> = Arc::new(Sink {
        book: Arc::downgrade(&book),
        finished: Mutex::new(Vec::new()),
    });
    (owner, book, scope, sink)
}
fn begin(book: &Finds, scope: &Arc<Scope>, sink: &Arc<dyn FindResultSink>, sequence: u64) -> Data {
    book.begin(
        scope,
        ContextMenuRequestId {
            browser: scope.browser,
            sequence,
        },
        sink,
    )
    .unwrap()
}
#[test]
fn serialized_find_correlates_native_ids_not_local_sequence_and_ignores_known_late_results() {
    let (_owner, book, scope, sink) = setup("private query");
    let first = begin(&book, &scope, &sink, 1);
    assert!(book.issue(&first));
    assert!(matches!(
        book.begin(
            &scope,
            ContextMenuRequestId {
                browser: scope.browser,
                sequence: 2
            },
            &sink
        ),
        Err(EngineError::Backpressure)
    ));
    let (_, r, done) = book.result(scope.browser, 41, 3, 1, None, false).unwrap();
    assert_eq!(r.operation.sequence, 1);
    assert_ne!(r.cef_identifier as u64, r.operation.sequence);
    assert!(!done);
    assert!(book.result(scope.browser, 41, 3, 1, None, true).unwrap().2);
    let old = book.end(&first, FindOutcome::Result);
    drop(old);
    book.notify_end(&first);
    assert_eq!(book.pending(), 0);
    let next = begin(&book, &scope, &sink, 3);
    assert!(book.issue(&next));
    assert!(
        book.result(scope.browser, 41, 999, 999, None, true)
            .is_none()
    );
    assert!(status(&next.status).last_result.is_none());
    let (_, r, _) = book.result(scope.browser, 42, 0, 0, None, true).unwrap();
    assert_eq!(r.operation, next.id);
    assert_eq!(r.matches, 0);
    let old = book.end(&next, FindOutcome::Result);
    drop(old);
    assert_eq!(book.pending(), 0);
}
#[test]
fn cancelled_unidentified_search_stays_pending_until_real_final_or_original_browser_close() {
    let (_owner, book, scope, sink) = setup("word");
    let data = begin(&book, &scope, &sink, 1);
    book.issue(&data);
    book.reserve_cancel(data.id).unwrap();
    data.status.lock().unwrap().stop_submitted = true;
    assert_eq!(book.pending(), 1);
    assert!(matches!(
        book.begin(
            &scope,
            ContextMenuRequestId {
                browser: scope.browser,
                sequence: 2
            },
            &sink
        ),
        Err(EngineError::Backpressure)
    ));
    assert!(status(&data.status).outcome.is_none());
    book.browser_closed(scope.browser);
    let q = status(&data.status);
    assert_eq!(q.outcome, Some(FindOutcome::Cancelled));
    assert_eq!(book.pending(), 0);
    assert!(book.lock().cursors.is_empty());
}
#[test]
fn unexpected_higher_identifier_quarantines_browser_and_never_relabels_old_search() {
    let (_owner, book, scope, sink) = setup("word");
    let data = begin(&book, &scope, &sink, 1);
    book.issue(&data);
    book.result(scope.browser, 9, 1, 1, None, false).unwrap();
    assert!(book.result(scope.browser, 10, 7, 7, None, true).is_none());
    assert_eq!(status(&data.status).last_result.unwrap().cef_identifier, 9);
    assert_eq!(
        status(&data.status).first_failure,
        Some(FindFailure::Uncorrelated)
    );
    book.result(scope.browser, 9, 1, 1, None, true).unwrap();
    drop(book.end(&data, FindOutcome::Result));
    assert_eq!(book.pending(), 1);
    assert_eq!(status(&data.status).outcome, Some(FindOutcome::Failed));
    assert!(matches!(
        book.begin(
            &scope,
            ContextMenuRequestId {
                browser: scope.browser,
                sequence: 2
            },
            &sink
        ),
        Err(EngineError::Backpressure)
    ));
    book.browser_closed(scope.browser);
    assert_eq!(book.pending(), 0);
}
#[test]
fn bounded_text_receipt_and_exact_cancel_rollback_keep_first_error_without_hidden_retry() {
    let (_owner, book, scope, sink) = setup("secret text");
    let data = begin(&book, &scope, &sink, 1);
    assert!(
        !format!(
            "{:?}",
            FindRequest {
                operation: data.id,
                menu: status(&data.status).menu,
                context: scope.context.clone()
            }
        )
        .contains("secret text")
    );
    book.reserve_cancel(data.id).unwrap();
    assert!(!book.issue(&data));
    book.rollback_cancel(&data, EngineError::Backpressure);
    assert!(book.issue(&data));
    assert_eq!(
        status(&data.status).first_failure,
        Some(FindFailure::Post(EngineError::Backpressure))
    );
    book.result(scope.browser, 5, 1, 1, None, true).unwrap();
    drop(book.end(&data, FindOutcome::Result));
    let next = begin(&book, &scope, &sink, 2);
    book.reserve_cancel(next.id).unwrap();
    book.rollback_cancel(&data, EngineError::Closed);
    assert!(status(&next.status).cancel_requested);
    assert_eq!(
        status(&data.status).first_failure,
        Some(FindFailure::Post(EngineError::Backpressure))
    );
    let (_other_owner, other_book, long_scope, other_sink) = setup(&"x".repeat(33));
    assert!(matches!(
        other_book.begin(
            &long_scope,
            ContextMenuRequestId {
                browser: long_scope.browser,
                sequence: 4
            },
            &other_sink
        ),
        Err(EngineError::Backpressure)
    ));
    book.browser_closed(scope.browser);
}
