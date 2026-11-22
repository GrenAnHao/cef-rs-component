//! Data/admission checks only: no native frame, real window, CEF or GUI.
use rust_cef_core::{
    contracts::{BrowserId, RuntimeGeneration},
    menu::{ContextMenu, ContextMenuResponse, MenuEntry, MenuLimits},
    menu_context::{MenuContext, MenuContextError, MenuContextLimits},
    menu_model::{MenuChild, MenuCommand, MenuItem, MenuModel, MenuSubmenu, MenuTreeError},
    menu_requests::{MenuRequestError, MenuRequests},
    ports::{EngineError, WindowBinding},
};
use std::{num::NonZeroUsize, sync::Arc};
fn nz(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}
fn browser() -> BrowserId {
    BrowserId::new(RuntimeGeneration::new(9), 1)
}
fn context() -> MenuContext {
    MenuContext {
        binding: WindowBinding {
            browser: browser(),
            window: 10,
        },
        binding_revision: 4,
        x: 3,
        y: 7,
        frame_id: "frame-secret".into(),
        selection: "selection-secret".into(),
        misspelled_word: "word-secret".into(),
        dictionary_suggestions: vec!["suggest-secret".into()],
    }
}
fn context_limits() -> MenuContextLimits {
    MenuContextLimits {
        max_frame_id_bytes: nz(32),
        max_text_bytes: nz(32),
        max_suggestions: nz(2),
        max_total_bytes: nz(96),
    }
}

#[test]
fn context_validates_identity_lengths_and_redacts_private_fields_from_debug() {
    let original = context();
    assert_eq!(original.validate(browser(), context_limits()), Ok(()));
    assert!(original.matches_binding(original.binding, 4));
    assert!(!original.matches_binding(original.binding, 5));
    assert!(!original.matches_binding(
        WindowBinding {
            window: 11,
            ..original.binding
        },
        4
    ));
    let debug = format!("{original:?}");
    for text in [
        "frame-secret",
        "selection-secret",
        "word-secret",
        "suggest-secret",
    ] {
        assert!(!debug.contains(text));
    }
    assert!(debug.contains("selection_bytes"));
    let total = original.frame_id.len()
        + original.selection.len()
        + original.misspelled_word.len()
        + original
            .dictionary_suggestions
            .iter()
            .map(String::len)
            .sum::<usize>();
    let mut exact = context_limits();
    exact.max_total_bytes = nz(total);
    assert_eq!(original.validate(browser(), exact), Ok(()));
    exact.max_total_bytes = nz(total - 1);
    assert_eq!(
        original.validate(browser(), exact),
        Err(MenuContextError::TotalBytesExceeded)
    );
    let mut cases = Vec::new();
    let mut bad = original.clone();
    bad.binding.browser = BrowserId::new(RuntimeGeneration::new(8), 1);
    cases.push((bad, MenuContextError::WrongBrowser));
    let mut bad = original.clone();
    bad.binding.window = 0;
    cases.push((bad, MenuContextError::InvalidBinding));
    let mut bad = original.clone();
    bad.binding_revision = 0;
    cases.push((bad, MenuContextError::InvalidBinding));
    let mut bad = original.clone();
    bad.frame_id.clear();
    cases.push((bad, MenuContextError::MissingFrame));
    let mut bad = original.clone();
    bad.frame_id = "x".repeat(33);
    cases.push((bad, MenuContextError::FrameIdTooLong));
    let mut bad = original.clone();
    bad.selection = "中".repeat(11);
    cases.push((bad, MenuContextError::TextTooLong));
    let mut bad = original.clone();
    bad.misspelled_word = "x".repeat(33);
    cases.push((bad, MenuContextError::TextTooLong));
    let mut bad = original.clone();
    bad.dictionary_suggestions = vec!["x".repeat(33)];
    cases.push((bad, MenuContextError::TextTooLong));
    let mut bad = original.clone();
    bad.dictionary_suggestions = vec!["x".into(); 3];
    cases.push((bad, MenuContextError::TooManySuggestions));
    let mut bad = original.clone();
    bad.selection = "x".repeat(32);
    bad.misspelled_word = "x".repeat(32);
    bad.dictionary_suggestions = vec!["x".repeat(32)];
    cases.push((bad, MenuContextError::TotalBytesExceeded));
    for (bad, error) in cases {
        assert_eq!(bad.validate(browser(), context_limits()), Err(error));
    }
}

fn command(id: i32, enabled: bool) -> MenuChild {
    MenuChild::Command(MenuCommand {
        command_id: id,
        label: format!("Item{id}"),
        enabled,
        visible: true,
        radio_group: None,
        checked: Some(false),
    })
}
fn model() -> MenuModel {
    MenuModel {
        entries: vec![
            MenuItem::Submenu(MenuSubmenu {
                label: "Disabled".into(),
                enabled: false,
                visible: true,
                entries: vec![command(1, true)],
            }),
            MenuItem::Submenu(MenuSubmenu {
                label: "Active".into(),
                enabled: true,
                visible: true,
                entries: vec![command(2, false), command(3, true)],
            }),
        ],
        x: 3,
        y: 7,
    }
}

#[test]
fn contextual_book_preserves_canonical_data_and_requires_bound_reserve_and_take() {
    let mut book = MenuRequests::new(
        nz(1),
        MenuLimits {
            max_entries: nz(8),
            max_label_bytes: nz(32),
            max_total_label_bytes: nz(128),
        },
    );
    let ctx = context();
    let binding = ctx.binding;
    let opened = book
        .open_model(
            browser(),
            model(),
            ctx.clone(),
            context_limits(),
            "original",
        )
        .unwrap();
    let mut ui = opened.presentation;
    let id = ui.request;
    let canonical = ui.context.clone();
    assert!(opened.displaced.is_none());
    let mut wrong = ctx.clone();
    wrong.binding.browser = BrowserId::new(RuntimeGeneration::new(8), 1);
    let rejected = book
        .open_model(browser(), model(), wrong, context_limits(), "bad-context")
        .unwrap_err();
    assert_eq!(
        rejected.error,
        MenuRequestError::Context(MenuContextError::WrongBrowser)
    );
    assert_eq!(rejected.payload, "bad-context");
    let mut misplaced = model();
    misplaced.x += 1;
    let rejected = book
        .open_model(
            browser(),
            misplaced,
            ctx.clone(),
            context_limits(),
            "bad-anchor",
        )
        .unwrap_err();
    assert_eq!(
        rejected.error,
        MenuRequestError::Context(MenuContextError::AnchorMismatch)
    );
    assert_eq!(rejected.payload, "bad-anchor");
    let mut raw = model();
    let MenuItem::Submenu(group) = &mut raw.entries[0] else {
        panic!("fixture group")
    };
    group
        .entries
        .splice(0..0, vec![MenuChild::Separator { visible: true }; 9]);
    let rejected = book
        .open_model(browser(), raw, ctx, context_limits(), "bad-model")
        .unwrap_err();
    assert_eq!(
        rejected.error,
        MenuRequestError::Hierarchy(MenuTreeError::TooManyEntries)
    );
    assert_eq!(rejected.payload, "bad-model");
    assert_eq!(book.len(), 1);
    let cancel = ContextMenuResponse::Cancel;
    assert_eq!(
        book.reserve_reply(id, cancel),
        Err(EngineError::InvalidRequest)
    );
    assert_eq!(
        book.reserve_context_reply(
            id,
            WindowBinding {
                window: 11,
                ..binding
            },
            4,
            cancel
        ),
        Err(EngineError::InvalidRequest)
    );
    assert_eq!(
        book.reserve_context_reply(id, binding, 5, cancel),
        Err(EngineError::InvalidRequest)
    );
    for command_id in [1, 2, 999] {
        assert_eq!(
            book.reserve_context_reply(id, binding, 4, ContextMenuResponse::Select { command_id }),
            Err(EngineError::InvalidRequest)
        );
    }
    // A modified UI-owned copy cannot rewrite the canonical frame/text/context.
    let modified = Arc::make_mut(&mut ui.context);
    modified.selection = "forged-selection".into();
    modified.frame_id = "forged-frame".into();
    modified.binding.window = 999;
    modified.binding_revision = 99;
    modified.x = 999;
    modified.y = -999;
    let select = ContextMenuResponse::Select { command_id: 3 };
    let first = book.reserve_context_reply(id, binding, 4, select).unwrap();
    assert_eq!(
        first.attempt(),
        1,
        "rejected scopes/selections consume no reservation"
    );
    assert!(book.take_reserved(first).is_none());
    assert!(
        book.take_context_reserved(
            first,
            WindowBinding {
                window: 11,
                ..binding
            },
            4
        )
        .is_none()
    );
    assert!(book.take_context_reserved(first, binding, 5).is_none());
    assert!(book.rollback(first));
    let second = book.reserve_context_reply(id, binding, 4, select).unwrap();
    assert!(second.attempt() > first.attempt());
    assert!(!book.rollback(first));
    assert!(book.take_context_reserved(first, binding, 4).is_none());
    let completed = book.take_context_reserved(second, binding, 4).unwrap();
    assert_eq!(completed.id, id);
    assert_eq!(completed.response, select);
    assert_eq!(completed.payload, "original");
    let saved = completed.context.unwrap();
    assert!(Arc::ptr_eq(&saved, &canonical));
    assert_eq!(saved.frame_id, "frame-secret");
    assert_eq!(saved.selection, "selection-secret");
    assert_eq!((saved.x, saved.y), (3, 7));
    assert!(book.is_empty());
    assert!(book.take_context_reserved(second, binding, 4).is_none());
    // A full contextual slot can be replaced, returning the precise old context
    // and payload without allowing its token to act on the new opening.
    let pending = book
        .open_model(
            browser(),
            model(),
            context(),
            context_limits(),
            "before-replace",
        )
        .unwrap();
    let mut next_context = context();
    next_context.binding_revision = 5;
    next_context.frame_id = "next-frame".into();
    let replaced = book
        .open_model(
            browser(),
            model(),
            next_context,
            context_limits(),
            "replacement",
        )
        .unwrap();
    let displaced = replaced.displaced.unwrap();
    assert_eq!(displaced.id(), pending.presentation.request);
    assert_eq!(displaced.payload(), &"before-replace");
    assert_eq!(displaced.context().unwrap().frame_id, "frame-secret");
    assert!(book.take(pending.presentation.request).is_none());
    assert!(book.take_context_reserved(second, binding, 4).is_none());
    // Existing flat admission keeps its original unbound contract, not a fallback
    // for replying to the contextual lane. Its explicit replacement still returns
    // the former contextual record to the owner.
    let old = book
        .open(
            browser(),
            ContextMenu {
                entries: vec![MenuEntry::Command {
                    command_id: 10,
                    label: "Old".into(),
                    enabled: true,
                    checked: false,
                }],
                x: 0,
                y: 0,
            },
            "legacy",
        )
        .unwrap();
    assert!(old.id.sequence > id.sequence);
    let displaced = old.displaced.as_ref().unwrap();
    assert_eq!(displaced.payload(), &"replacement");
    assert_eq!(displaced.context().unwrap().frame_id, "next-frame");
    assert_eq!(
        book.reserve_context_reply(old.id, binding, 4, cancel),
        Err(EngineError::InvalidRequest)
    );
    let legacy = book.reserve_reply(old.id, cancel).unwrap();
    let completed = book.take_reserved(legacy).unwrap();
    assert_eq!(completed.payload, "legacy");
    assert!(completed.context.is_none());
}
