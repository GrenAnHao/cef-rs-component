use super::*;
use rust_cef_core::{
    contracts::RuntimeGeneration,
    menu_model::{MenuCommand, MenuItem},
};
use std::sync::atomic::AtomicBool;
fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}
fn limits() -> MenuLimits {
    MenuLimits {
        max_entries: nz(8),
        max_label_bytes: nz(32),
        max_total_label_bytes: nz(64),
    }
}
fn context_limits() -> MenuContextLimits {
    MenuContextLimits {
        max_frame_id_bytes: nz(32),
        max_text_bytes: nz(32),
        max_suggestions: nz(2),
        max_total_bytes: nz(64),
    }
}
fn context() -> MenuContext {
    let browser = BrowserId::new(RuntimeGeneration::new(7), 3);
    MenuContext {
        binding: WindowBinding { browser, window: 9 },
        binding_revision: 1,
        x: 2,
        y: 3,
        frame_id: "frame".into(),
        selection: String::new(),
        misspelled_word: String::new(),
        dictionary_suggestions: Vec::new(),
    }
}
fn model() -> MenuModel {
    MenuModel {
        entries: vec![MenuItem::Command(MenuCommand {
            command_id: 1,
            label: "item".into(),
            enabled: true,
            visible: true,
            checked: None,
            radio_group: None,
        })],
        x: 2,
        y: 3,
    }
}
fn payload(table: &NativeMenus, f: impl FnOnce() + Send + 'static) -> RetainedMenu {
    RetainedMenu {
        reply: Some(Reply::Test(Box::new(f))),
        faults: table.faults.clone(),
        scope: None,
        #[cfg(feature = "test-support")]
        probe: None,
    }
}
fn open(table: &NativeMenus, p: RetainedMenu) -> (MenuPresentation, Option<MenuWork>) {
    match table.open(model(), context(), context_limits(), p) {
        Ok(v) => v,
        Err((e, w)) => {
            drop(w);
            panic!("open failed: {e:?}")
        }
    }
}
#[test]
fn replacement_reentry_invalidates_outer_publication_without_taking_newer_request() {
    let table = Arc::new(NativeMenus::new());
    table.configure(nz(1), limits()).unwrap();
    let newer = Arc::new(Mutex::new(None));
    let inside = table.clone();
    let observed = newer.clone();
    let (first, _) = open(
        &table,
        payload(&table, move || {
            let (third, second_work) = open(&inside, payload(&inside, || {}));
            second_work
                .expect("nested replacement owns outer second")
                .cancel(&NativeAudit::new());
            *observed.lock().unwrap() = Some(third.request);
        }),
    );
    let (second, first_work) = open(&table, payload(&table, || {}));
    assert!(!table.is_pending(first.request));
    assert!(table.is_pending(second.request));
    first_work.unwrap().cancel(&NativeAudit::new());
    let third = newer.lock().unwrap().unwrap();
    assert!(third.sequence > second.request.sequence);
    assert!(!table.is_pending(second.request));
    assert!(table.is_pending(third));
    assert!(table.take_id(second.request).is_none());
    assert_eq!(table.pending(), 1);
    table.take_id(third).unwrap().cancel(&NativeAudit::new());
    assert_eq!(table.pending(), 0);
    let (fourth, _) = open(&table, payload(&table, || {}));
    let guard = table.callback_guard();
    assert_eq!(table.pending(), 2);
    assert_eq!(table.pending_for(third.browser), 2);
    drop(guard);
    assert_eq!(table.pending(), 1);
    table
        .take_id(fourth.request)
        .unwrap()
        .cancel(&NativeAudit::new());
    assert_eq!(table.pending(), 0);
}
#[test]
fn exact_rollback_and_reentrant_answer_keep_work_accounted_until_release() {
    let table = Arc::new(NativeMenus::new());
    table.configure(nz(1), limits()).unwrap();
    let inside = table.clone();
    let called = Arc::new(AtomicBool::new(false));
    let mark = called.clone();
    let (p, old) = open(
        &table,
        payload(&table, move || {
            assert_eq!(inside.pending(), 1);
            assert!(inside.is_active(context().binding.browser));
            mark.store(true, Ordering::Release);
        }),
    );
    assert!(old.is_none());
    assert!(
        table
            .reserve_cancel(p.request, p.context.binding, 2)
            .is_err()
    );
    let a = table
        .reserve_cancel(p.request, p.context.binding, 1)
        .unwrap();
    assert!(table.rollback(a));
    let b = table
        .reserve_cancel(p.request, p.context.binding, 1)
        .unwrap();
    assert!(!table.rollback(a));
    assert!(table.take_cancel(a).is_none());
    assert_eq!(table.pending(), 1);
    let work = table.take_cancel(b).unwrap();
    assert_eq!(table.pending(), 1);
    assert!(!called.load(Ordering::Acquire));
    work.cancel(&NativeAudit::new());
    assert!(called.load(Ordering::Acquire));
    assert_eq!(table.pending(), 0);
    assert!(table.take_cancel(b).is_none());
    assert!(!table.rollback(b));
}
#[test]
fn replacement_and_rejection_return_callbacks_for_outside_lock_cleanup() {
    let table = Arc::new(NativeMenus::new());
    table.configure(nz(1), limits()).unwrap();
    let first = table.clone();
    let (p, old) = open(
        &table,
        payload(&table, move || assert_eq!(first.pending(), 2)),
    );
    assert!(old.is_none());
    let (next, old) = open(&table, payload(&table, || {}));
    assert_ne!(p.request, next.request);
    assert_eq!(table.pending(), 2);
    old.unwrap().cancel(&NativeAudit::new());
    assert_eq!(table.pending(), 1);
    let mut bad = model();
    let MenuItem::Command(c) = &mut bad.entries[0] else {
        unreachable!()
    };
    c.label.clear();
    let rejected = table.clone();
    let result = table.open(
        bad,
        context(),
        context_limits(),
        payload(&table, move || assert_eq!(rejected.pending(), 2)),
    );
    match result {
        Err((
            MenuOpenError::Request(rust_cef_core::menu_requests::MenuRequestError::Hierarchy(
                rust_cef_core::menu_model::MenuTreeError::EmptyLabel,
            )),
            work,
        )) => work.cancel(&NativeAudit::new()),
        _ => panic!("invalid model was not refused"),
    }
    assert_eq!(table.pending(), 1);
    assert!(
        table
            .reserve_cancel(p.request, p.context.binding, 1)
            .is_err()
    );
    let ticket = table
        .reserve_cancel(next.request, next.context.binding, 1)
        .unwrap();
    table
        .take_cancel(ticket)
        .unwrap()
        .cancel(&NativeAudit::new());
    assert_eq!(table.pending(), 0);
}
#[test]
fn unexpected_close_and_unexecuted_work_never_native_drop_or_report_drained() {
    let table = NativeMenus::new();
    table.configure(nz(1), limits()).unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let mark = called.clone();
    open(
        &table,
        payload(&table, move || {
            mark.store(true, Ordering::Release);
        }),
    );
    table.native_closed(context().binding.browser);
    assert!(!called.load(Ordering::Acquire));
    assert_eq!(table.pending(), 1);
    assert!(!table.can_capture());
    assert!(table.configure(nz(1), limits()).is_err());
    table.abandon();
    assert_eq!(table.pending(), 1);
    let other = NativeMenus::new();
    other.configure(nz(1), limits()).unwrap();
    open(&other, payload(&other, || {}));
    drop(other.drain());
    assert_eq!(other.pending(), 1);
}
