use super::*;
use rust_cef_core::{
    contracts::RuntimeGeneration,
    menu::MenuLimits,
    menu_context::{MenuContext, MenuContextLimits, MenuPresentation},
    menu_lease::{MenuLease, MenuLeaseOwner, MenuLeaseStamp},
    ports::{
        ContextMenuDeliverySink, ContextMenuResponseSink, ContextMenuTarget, MenuDeliveryError,
        WindowBinding,
    },
};
use std::{num::NonZeroUsize, sync::atomic::AtomicBool};
fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}
struct Target(MenuLease);
impl ContextMenuDeliverySink for Target {
    fn capture(&self) -> Result<MenuLeaseStamp, MenuDeliveryError> {
        self.0.capture().map_err(MenuDeliveryError::Lease)
    }
    fn publish(
        &self,
        _: MenuPresentation,
        _: Arc<dyn ContextMenuResponseSink>,
        _: MenuLeaseStamp,
    ) -> Result<(), MenuDeliveryError> {
        Ok(())
    }
    fn dismiss(&self, _: ContextMenuRequestId) -> Result<(), MenuDeliveryError> {
        Ok(())
    }
}
fn scope() -> (MenuLeaseOwner, Arc<crate::menu_bridge::Binding>, Arc<Scope>) {
    let browser = BrowserId::new(RuntimeGeneration::new(7), 3);
    let owner = MenuLeaseOwner::new(WindowBinding { browser, window: 9 }, nz(1)).unwrap();
    let bridge = crate::menu_bridge::MenuBridge::new();
    bridge
        .configure(
            crate::ContextMenuConfig {
                capacity: nz(1),
                model_limits: MenuLimits {
                    max_entries: nz(8),
                    max_label_bytes: nz(64),
                    max_total_label_bytes: nz(128),
                },
                context_limits: MenuContextLimits {
                    max_frame_id_bytes: nz(64),
                    max_text_bytes: nz(256),
                    max_suggestions: nz(5),
                    max_total_bytes: nz(512),
                },
            },
            || Ok(()),
        )
        .unwrap();
    let target = ContextMenuTarget {
        lease: owner.lease(),
        delivery: Arc::new(Target(owner.lease())),
    };
    let (binding, _) = bridge.bind(browser, &target, true).unwrap();
    let ctx = Arc::new(MenuContext {
        binding: WindowBinding { browser, window: 9 },
        binding_revision: 1,
        x: 2,
        y: 3,
        frame_id: "frame".into(),
        selection: String::new(),
        misspelled_word: String::new(),
        dictionary_suggestions: Vec::new(),
    });
    let scope = Arc::new(Scope {
        browser,
        native_id: 1,
        frame_id: "frame".into(),
        context: ctx,
        lease: owner.lease(),
        stamp: owner.lease().capture().unwrap(),
        registry: Weak::new(),
        registration: Arc::downgrade(&binding),
        serial: binding.serial,
        epoch: 1,
        spelling_opening: 0,
        delivery: Arc::downgrade(&target.delivery),
        target: crate::menu_bridge::TargetMenuContext::default(),
        synthesized_target: false,
    });
    (owner, binding, scope)
}
struct Sink {
    refuse: AtomicBool,
    calls: AtomicUsize,
    book: Weak<SourceViews>,
}
impl SourceViewSink for Sink {
    fn started(
        &self,
        _: SourceOperationId,
        _: ContextMenuRequestId,
        _: Arc<dyn SourceViewControl>,
    ) -> Result<(), EngineError> {
        Ok(())
    }
    fn publish(
        &self,
        _: SourceViewRequest,
        _: Arc<dyn SourceViewControl>,
    ) -> Result<(), EngineError> {
        Ok(())
    }
    fn close(
        &self,
        _: SourceOperationId,
        control: Arc<dyn SourceViewControl>,
    ) -> Result<(), EngineError> {
        assert!(self.book.upgrade().unwrap().pending() > 0);
        assert!(control.status().host_live);
        self.calls.fetch_add(1, Ordering::AcqRel);
        if self.refuse.load(Ordering::Acquire) {
            Err(EngineError::PostRejected)
        } else {
            control.closed()
        }
    }
}
fn book() -> Arc<SourceViews> {
    let b = SourceViews::new();
    b.configure(SourceViewLimits {
        capacity: nz(1),
        max_source_bytes: nz(64),
    })
    .unwrap();
    b
}
fn sink(b: &Arc<SourceViews>) -> Arc<Sink> {
    Arc::new(Sink {
        refuse: AtomicBool::new(false),
        calls: AtomicUsize::new(0),
        book: Arc::downgrade(b),
    })
}
fn begin(b: &Arc<SourceViews>, scope: &Arc<Scope>, sink: &Arc<Sink>) -> SourceOperationId {
    let p: Arc<dyn SourceViewSink> = sink.clone();
    b.begin(
        ContextMenuRequestId {
            browser: scope.browser,
            sequence: 1,
        },
        scope,
        &p,
    )
    .unwrap()
    .0
}
#[test]
fn original_operation_counts_native_release_and_host_absence_independently() {
    let (mut owner, _binding, scope) = scope();
    let b = book();
    let sink = sink(&b);
    let id = begin(&b, &scope, &sink);
    assert_eq!(b.pending(), 1);
    assert!(b.received(id));
    let data = b.begin_publish(id, 21).unwrap();
    let control = b.control(id, &data);
    assert!(control.is_current());
    control.presented().unwrap();
    // The source viewer's own focus is not a new menu authorization. Retirement
    // of the original owner still denies current, independently of native refs.
    owner.invalidate().unwrap();
    assert!(control.is_current());
    b.released(id);
    assert_eq!(b.pending(), 1);
    assert!(!control.status().native_live);
    assert!(control.status().host_live);
    owner.retire().unwrap();
    assert!(!control.is_current());
    control.closed().unwrap();
    assert_eq!(b.pending(), 0);
    assert_eq!(control.status().outcome, Some(SourceOutcome::Closed));
    assert!(control.closed().is_err());
}
#[test]
fn close_rejection_keeps_barrier_and_first_error_until_explicit_retry_and_old_id_is_inert() {
    let (_owner, _binding, scope) = scope();
    let b = book();
    let sink = sink(&b);
    let id = begin(&b, &scope, &sink);
    assert!(b.received(id));
    let d = b.begin_publish(id, 12).unwrap();
    let control = b.control(id, &d);
    b.released(id);
    sink.refuse.store(true, Ordering::Release);
    assert_eq!(control.request_close(), Err(EngineError::PostRejected));
    assert_eq!(b.pending(), 1);
    assert_eq!(
        control.status().first_failure,
        Some(SourceFailure::Close(EngineError::PostRejected))
    );
    // A second lifecycle cutpoint does not silently retry.
    b.cancel(Some(scope.browser), None, None).unwrap();
    assert_eq!(sink.calls.load(Ordering::Acquire), 1);
    sink.refuse.store(false, Ordering::Release);
    control.request_close().unwrap();
    assert_eq!(b.pending(), 0);
    assert_eq!(sink.calls.load(Ordering::Acquire), 2);
    assert_eq!(control.status().outcome, Some(SourceOutcome::Cancelled));
    assert_eq!(
        control.status().first_failure,
        Some(SourceFailure::Close(EngineError::PostRejected))
    );
    let next = begin(&b, &scope, &sink);
    assert!(next.sequence > id.sequence);
    assert!(control.closed().is_err());
    assert_eq!(b.pending(), 1);
    b.cancel(Some(scope.browser), None, None).unwrap();
    assert_eq!(b.pending(), 1);
    b.released(next);
    assert_eq!(b.pending(), 0);
}
#[test]
fn retired_before_native_issue_rolls_back_reservation_but_keeps_operation_receipt() {
    let (mut owner, _binding, scope) = scope();
    let b = book();
    let sink = sink(&b);
    let id = begin(&b, &scope, &sink);
    let data = b.data(id).unwrap();
    let control = b.control(id, &data);
    assert!(start_current(&scope, &data));
    owner.invalidate().unwrap();
    assert!(!start_current(&scope, &data));
    b.cancel(Some(scope.browser), Some(scope.serial), Some(id))
        .unwrap();
    b.unissued(id);
    assert_eq!(b.pending(), 0);
    let q = control.status();
    assert_eq!(q.outcome, Some(SourceOutcome::Cancelled));
    assert!(!q.callback_received && !q.native_live && !q.host_live);
    assert!(q.first_failure.is_none());
}
#[test]
fn reentrant_close_ack_stays_terminal_and_late_host_error_remains_observable() {
    let (_owner, _binding, scope) = scope();
    let b = book();
    let sink = sink(&b);
    let id = begin(&b, &scope, &sink);
    assert!(b.received(id));
    let data = b.begin_publish(id, 12).unwrap();
    let control = b.control(id, &data);
    b.released(id);
    control.closed().unwrap();
    assert_eq!(b.pending(), 0);
    b.data_failure(id, &data, SourceFailure::Publish(EngineError::PostRejected));
    let q = control.status();
    assert_eq!(q.outcome, Some(SourceOutcome::Closed));
    assert_eq!(q.phase, SourcePhase::Finished);
    assert_eq!(
        q.first_failure,
        Some(SourceFailure::Publish(EngineError::PostRejected))
    );
    assert_eq!(b.pending(), 0);
    assert!(b.snapshot().first_failure.is_some());
}
#[test]
fn duplicate_callback_requests_exact_viewer_close_without_converting_failure_to_cancel() {
    let (_owner, _binding, scope) = scope();
    let b = book();
    let sink = sink(&b);
    let id = begin(&b, &scope, &sink);
    assert!(b.received(id));
    let data = b.begin_publish(id, 10).unwrap();
    let control = b.control(id, &data);
    control.presented().unwrap();
    assert!(!b.received(id));
    assert_eq!(sink.calls.load(Ordering::Acquire), 1);
    assert!(!control.status().host_live);
    assert_eq!(b.pending(), 1);
    b.released(id);
    assert_eq!(b.pending(), 0);
    assert_eq!(control.status().outcome, Some(SourceOutcome::Failed));
    assert_eq!(
        control.status().first_failure,
        Some(SourceFailure::DuplicateCallback)
    );
}
#[test]
fn native_visitor_wrapper_clones_cannot_fake_final_release_or_drainage() {
    let (_owner, _binding, scope) = scope();
    let b = book();
    let sink = sink(&b);
    let id = begin(&b, &scope, &sink);
    let life = Arc::new(VisitorLife {
        book: b.clone(),
        registry: Weak::new(),
        id,
        limit: 64,
        ui: std::thread::current().id(),
        fields: AtomicUsize::new(1),
    });
    let first = SourceVisitor::new(VisitorField(life.clone()));
    let second = first.clone();
    drop(first);
    assert_eq!(life.fields.load(Ordering::Acquire), 1);
    assert_eq!(b.pending(), 1);
    assert!(!b.snapshot().faulted);
    drop(second);
    assert_eq!(life.fields.load(Ordering::Acquire), 0);
    // There is deliberately no registry/UI fence in this isolated test. Last
    // Rust interface release must fault/retain, NOT pretend the async fence ran.
    assert!(b.snapshot().faulted);
    assert_eq!(b.pending(), 2);
    assert_eq!(b.snapshot().native_live, 1);
    assert_eq!(
        b.snapshot().first_failure,
        Some((id, SourceFailure::Invariant))
    );
}
#[test]
fn borrowed_utf16_budget_rejects_before_owned_copy_and_debug_never_dumps_source() {
    fn value(units: &mut [u16]) -> CefString {
        CefString::from(cef::sys::_cef_string_utf16_t {
            str_: units.as_mut_ptr(),
            length: units.len(),
            dtor: None,
        })
    }
    let mut units: Vec<_> = "中文".encode_utf16().collect();
    let borrowed = value(&mut units);
    assert_eq!(copy_source(Some(&borrowed), 5), Err(SourceFailure::Budget));
    let text = copy_source(Some(&borrowed), 6).unwrap();
    assert_eq!(text.as_str(), "中文");
    assert_eq!(text.len(), 6);
    assert!(!format!("{text:?}").contains("中文"));
    let mut bad = [0xD800];
    assert_eq!(
        copy_source(Some(&value(&mut bad)), 6),
        Err(SourceFailure::InvalidText)
    );
    assert_eq!(copy_source(None, 6), Err(SourceFailure::MissingText));
    let empty = CefString::from(cef::sys::_cef_string_utf16_t {
        str_: std::ptr::null_mut(),
        length: 0,
        dtor: None,
    });
    assert!(copy_source(Some(&empty), 6).unwrap().is_empty());
}
