//! Focused integration tests for the pure host-owned menu request book.
//!
//! These exercise bookkeeping only: no native callback, UI, mutex or lock
//! discipline is modeled. Returned payloads stand in for continuations whose
//! owner must end its native borrow before invoking or dropping them.

use std::num::NonZeroUsize;

use rust_cef_core::{
    contracts::{BrowserId, RuntimeGeneration},
    menu::{
        ContextMenu, ContextMenuRequestId, ContextMenuResponse, MenuEntry, MenuLimits,
        MenuModelError,
    },
    menu_requests::{MenuRequestError, MenuRequests},
    ports::EngineError,
};

fn nz(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("test values are nonzero")
}

fn limits() -> MenuLimits {
    MenuLimits {
        max_entries: nz(4),
        max_label_bytes: nz(32),
        max_total_label_bytes: nz(64),
    }
}

fn browser(local: u64) -> BrowserId {
    BrowserId::new(RuntimeGeneration::new(1), local)
}

fn command(command_id: i32, label: &str, enabled: bool) -> MenuEntry {
    MenuEntry::Command {
        command_id,
        label: label.into(),
        enabled,
        checked: false,
    }
}

fn menu() -> ContextMenu {
    ContextMenu {
        entries: vec![
            MenuEntry::Separator,
            command(1, "Back", true),
            MenuEntry::Separator,
            command(2, "Reload", false),
        ],
        x: 10,
        y: 20,
    }
}

#[test]
fn capacity_bounds_replacements_and_model_rejections_return_the_payload() {
    let mut book = MenuRequests::new(nz(2), limits());
    let first = book.open(browser(1), menu(), "one").expect("first fits");
    assert!(first.displaced.is_none());
    // Validation inspects the raw model, separators included, before any
    // normalization or replacement of the current opening. Normalizing these
    // six raw entries would collapse them to a single command, so refusal here
    // proves the limit is applied to the raw model, not the cleaned one.
    let too_many = ContextMenu {
        entries: vec![
            MenuEntry::Separator,
            MenuEntry::Separator,
            MenuEntry::Separator,
            MenuEntry::Separator,
            command(1, "Back", true),
            MenuEntry::Separator,
        ],
        x: 0,
        y: 0,
    };
    let rejected = book
        .open(browser(1), too_many, "unvalidated")
        .expect_err("raw model exceeds max_entries");
    assert_eq!(
        rejected.error,
        MenuRequestError::Model(MenuModelError::TooManyEntries)
    );
    assert_eq!(rejected.payload, "unvalidated");
    // The refused replacement left the current opening intact.
    let second = book.open(browser(2), menu(), "two").expect("second fits");
    assert_eq!(book.len(), 2);
    assert!(!book.is_empty());
    let full = book.open(browser(3), menu(), "three");
    assert_eq!(full.expect_err("bounded").error, MenuRequestError::Capacity);
    assert_eq!(book.len(), 2);

    // A full book still replaces the same browser, returning the old payload.
    let replacement = book
        .open(browser(1), menu(), "one-replaced")
        .expect("same browser may be replaced at capacity");
    assert_eq!(replacement.id.browser, first.id.browser);
    assert!(replacement.id.sequence > first.id.sequence);
    let displaced = replacement.displaced.expect("old opening returned");
    assert_eq!(displaced.id(), first.id);
    assert_eq!(displaced.payload(), &"one");
    assert_eq!(displaced.commands(), [(1, true), (2, false)].as_slice());
    assert_eq!(replacement.menu.entries.len(), 3);
    assert!(matches!(
        replacement.menu.entries.first(),
        Some(MenuEntry::Command { .. })
    ));

    // The displaced id is stale; it cannot be reserved or taken.
    let stale_id = first.id;
    assert!(book.take(stale_id).is_none());
    assert!(
        book.reserve_reply(stale_id, ContextMenuResponse::Cancel)
            .is_err()
    );
    assert!(
        book.reserve_reply(second.id, ContextMenuResponse::Cancel)
            .is_ok()
    );
}

#[test]
fn reservations_issue_once_and_stale_nonces_cannot_clear_a_retry() {
    let mut book = MenuRequests::new(nz(2), limits());
    let opened = book.open(browser(7), menu(), "payload").expect("opening");
    let id = opened.id;
    let select = ContextMenuResponse::Select { command_id: 1 };
    let stale = book.reserve_reply(id, select).expect("first attempt");
    assert_eq!(
        book.reserve_reply(id, select),
        Err(EngineError::InvalidRequest)
    );
    assert!(book.rollback(stale));
    let current = book.reserve_reply(id, select).expect("retry");
    assert!(current.attempt() > stale.attempt());
    // The old token names neither the current attempt nor its response.
    assert!(!book.rollback(stale));
    assert!(book.take_reserved(stale).is_none());
    assert_eq!(book.len(), 1);
    let completed = book.take_reserved(current).expect("current reservation");
    assert_eq!(completed.id, id);
    assert_eq!(completed.response, select);
    assert_eq!(completed.payload, "payload");
    assert!(book.is_empty());
    assert!(book.take_reserved(current).is_none());
    assert!(!book.rollback(current));
    let reopened = book.open(browser(7), menu(), "again").expect("new opening");
    assert!(reopened.id.sequence > id.sequence);
    assert!(reopened.displaced.is_none());
}

#[test]
fn invalid_selections_and_request_identities_are_refused() {
    let mut book = MenuRequests::new(nz(2), limits());
    let opened = book.open(browser(1), menu(), "payload").expect("opening");
    let cancel = ContextMenuResponse::Cancel;
    let disabled = ContextMenuResponse::Select { command_id: 2 };
    let unknown = ContextMenuResponse::Select { command_id: 999 };
    assert_eq!(
        book.reserve_reply(opened.id, disabled),
        Err(EngineError::InvalidRequest)
    );
    assert_eq!(
        book.reserve_reply(opened.id, unknown),
        Err(EngineError::InvalidRequest)
    );
    let zero = ContextMenuRequestId {
        browser: opened.id.browser,
        sequence: 0,
    };
    assert_eq!(
        book.reserve_reply(zero, cancel),
        Err(EngineError::InvalidRequest)
    );
    let foreign = ContextMenuRequestId {
        browser: BrowserId::new(RuntimeGeneration::new(2), 1),
        sequence: opened.id.sequence,
    };
    assert_eq!(
        book.reserve_reply(foreign, cancel),
        Err(EngineError::InvalidRequest)
    );
    let stale = ContextMenuRequestId {
        browser: opened.id.browser,
        sequence: opened.id.sequence + 1,
    };
    assert_eq!(
        book.reserve_reply(stale, cancel),
        Err(EngineError::InvalidRequest)
    );
    // Refusals consume no reservation slot; Cancel is always valid and still
    // completes the exact request with its payload.
    let token = book.reserve_reply(opened.id, cancel).expect("cancel valid");
    assert_eq!(token.response(), cancel);
    let completed = book.take_reserved(token).expect("reserved cancel");
    assert_eq!(completed.response, cancel);
    assert_eq!(completed.payload, "payload");
    assert_eq!(
        book.reserve_reply(opened.id, cancel),
        Err(EngineError::InvalidRequest)
    );
}

#[test]
fn drain_take_and_take_browser_return_every_payload_to_its_owner() {
    let mut book = MenuRequests::new(nz(4), limits());
    let one = book.open(browser(1), menu(), "one").expect("one");
    book.open(browser(2), menu(), "two").expect("two");
    let three = book.open(browser(3), menu(), "three").expect("three");
    assert_eq!(book.take(one.id).expect("exact id").into_payload(), "one");
    assert!(book.take(one.id).is_none());
    let taken = book.take_browser(browser(2)).expect("full browser id");
    assert_eq!(taken.into_payload(), "two");
    assert_eq!(book.len(), 1);
    // A pending reservation is removed with a drain; its token becomes stale.
    let token = book
        .reserve_reply(three.id, ContextMenuResponse::Cancel)
        .expect("reserve before drain");
    let drained = book.drain();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].id(), three.id);
    assert_eq!(drained[0].payload(), &"three");
    let payload = drained
        .into_iter()
        .next()
        .expect("one record")
        .into_payload();
    assert_eq!(payload, "three");
    assert!(book.is_empty());
    assert!(book.take_reserved(token).is_none());
    assert!(!book.rollback(token));
}
