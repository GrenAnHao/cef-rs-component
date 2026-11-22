//! Native-state data fidelity and admission, not an actual menu widget.
use rust_cef_core::{
    contracts::{BrowserId, RuntimeGeneration},
    menu::{ContextMenuResponse, MenuLimits},
    menu_context::{MenuContext, MenuContextLimits},
    menu_model::{MenuChild, MenuCommand, MenuItem, MenuModel, MenuSubmenu, MenuTreeError},
    menu_requests::MenuRequests,
    ports::{EngineError, WindowBinding},
};
use std::num::NonZeroUsize;
fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}
fn limits() -> MenuLimits {
    MenuLimits {
        max_entries: nz(32),
        max_label_bytes: nz(32),
        max_total_label_bytes: nz(256),
    }
}
fn command(id: i32, visible: bool, radio_group: Option<i32>) -> MenuCommand {
    MenuCommand {
        command_id: id,
        label: format!("Item{id}"),
        enabled: true,
        visible,
        checked: radio_group.map(|_| false),
        radio_group,
    }
}
fn model() -> MenuModel {
    MenuModel {
        entries: vec![
            MenuItem::Separator { visible: true },
            MenuItem::Command(command(1, false, None)),
            MenuItem::Separator { visible: false },
            MenuItem::Command(command(2, true, Some(7))),
            MenuItem::Separator { visible: true },
            MenuItem::Command(command(3, false, Some(7))),
            MenuItem::Separator { visible: true },
            MenuItem::Submenu(MenuSubmenu {
                label: "Hidden".into(),
                enabled: true,
                visible: false,
                entries: vec![
                    MenuChild::Separator { visible: true },
                    MenuChild::Command(command(4, true, Some(7))),
                    MenuChild::Separator { visible: true },
                ],
            }),
            MenuItem::Command(command(5, true, None)),
            MenuItem::Separator { visible: true },
            MenuItem::Command(command(6, false, None)),
        ],
        x: 3,
        y: 4,
    }
}
#[test]
fn visible_projection_gates_selection_and_preserves_hidden_rows_and_radio_metadata() {
    let mut m = model();
    assert_eq!(m.validate(limits()), Ok(()));
    for id in [1, 3, 4, 6] {
        assert!(!m.allows_selection(id));
    }
    for id in [2, 5] {
        assert!(m.allows_selection(id));
    }
    let raw = m.clone();
    m.normalize();
    assert_eq!(m.entries.len(), 8);
    assert_eq!(
        m.entries
            .iter()
            .filter(|x| matches!(x, MenuItem::Separator { visible: true }))
            .count(),
        1
    );
    assert!(matches!(
        m.entries[1],
        MenuItem::Separator { visible: false }
    ));
    let MenuItem::Command(radio) = &m.entries[2] else {
        panic!("radio lost")
    };
    assert_eq!((radio.checked, radio.radio_group), (Some(false), Some(7)));
    let hidden_before: Vec<_> = raw
        .entries
        .iter()
        .filter(|x| matches!(x, MenuItem::Command(c) if !c.visible))
        .collect();
    let hidden_after: Vec<_> = m
        .entries
        .iter()
        .filter(|x| matches!(x, MenuItem::Command(c) if !c.visible))
        .collect();
    assert_eq!(hidden_before, hidden_after);
    let before = raw
        .entries
        .iter()
        .find_map(|x| match x {
            MenuItem::Submenu(g) if !g.visible => Some(&g.entries),
            _ => None,
        })
        .unwrap();
    let after = m
        .entries
        .iter()
        .find_map(|x| match x {
            MenuItem::Submenu(g) if !g.visible => Some(&g.entries),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        before, after,
        "effective-hidden subtree must remain verbatim"
    );
    let once = m.clone();
    m.normalize();
    assert_eq!(once, m);
    let mut invalid = model();
    let MenuItem::Command(c) = &mut invalid.entries[3] else {
        unreachable!()
    };
    c.checked = None;
    assert_eq!(
        invalid.validate(limits()),
        Err(MenuTreeError::InvalidRadioState)
    );
    let mut hidden = command(10, false, None);
    assert_eq!(
        MenuModel {
            entries: vec![MenuItem::Command(hidden.clone())],
            x: 0,
            y: 0
        }
        .validate(limits()),
        Err(MenuTreeError::NoVisibleCommands)
    );
    hidden.radio_group = Some(-7);
    hidden.checked = Some(true); // opaque group IDs have no invented positivity rule
    assert_eq!(
        MenuModel {
            entries: vec![MenuItem::Submenu(MenuSubmenu {
                label: "G".into(),
                enabled: true,
                visible: true,
                entries: vec![MenuChild::Command(hidden)]
            })],
            x: 0,
            y: 0
        }
        .validate(limits()),
        Err(MenuTreeError::EmptyVisibleSubmenu)
    );
}
#[test]
fn contextual_ledger_refuses_hidden_leaf_and_parent_even_if_ui_changes_visibility() {
    let browser = BrowserId::new(RuntimeGeneration::new(2), 1);
    let binding = WindowBinding { browser, window: 4 };
    let mut book = MenuRequests::new(nz(1), limits());
    let mut ui = book
        .open_model(
            browser,
            model(),
            MenuContext {
                binding,
                binding_revision: 1,
                x: 3,
                y: 4,
                frame_id: "frame".into(),
                selection: String::new(),
                misspelled_word: String::new(),
                dictionary_suggestions: vec![],
            },
            MenuContextLimits {
                max_frame_id_bytes: nz(16),
                max_text_bytes: nz(16),
                max_suggestions: nz(5),
                max_total_bytes: nz(64),
            },
            (),
        )
        .unwrap()
        .presentation;
    for item in &mut ui.model.entries {
        match item {
            MenuItem::Command(c) => c.visible = true,
            MenuItem::Submenu(g) => g.visible = true,
            _ => {}
        }
    }
    for command_id in [1, 3, 4, 6] {
        assert_eq!(
            book.reserve_context_reply(
                ui.request,
                binding,
                1,
                ContextMenuResponse::Select { command_id }
            ),
            Err(EngineError::InvalidRequest)
        );
    }
    let r = book
        .reserve_context_reply(
            ui.request,
            binding,
            1,
            ContextMenuResponse::Select { command_id: 2 },
        )
        .unwrap();
    assert!(book.take_context_reserved(r, binding, 1).is_some());
    assert!(book.is_empty());
}
