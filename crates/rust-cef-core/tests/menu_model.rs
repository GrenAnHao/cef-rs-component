//! Data-only menu hierarchy checks, not GPUI/native rendering evidence.
use rust_cef_core::{
    menu::{ContextMenu, MenuEntry, MenuLimits},
    menu_model::{
        MenuChild, MenuCommand, MenuItem, MenuModel, MenuSubmenu, MenuTreeError as MenuModelError,
    },
};
use std::num::NonZeroUsize;

fn limits(entries: usize, label: usize, total: usize) -> MenuLimits {
    MenuLimits {
        max_entries: NonZeroUsize::new(entries).unwrap(),
        max_label_bytes: NonZeroUsize::new(label).unwrap(),
        max_total_label_bytes: NonZeroUsize::new(total).unwrap(),
    }
}
fn command(id: i32, label: &str, enabled: bool, checked: Option<bool>) -> MenuCommand {
    MenuCommand {
        command_id: id,
        label: label.into(),
        enabled,
        visible: true,
        radio_group: None,
        checked,
    }
}
fn group(label: &str, enabled: bool, entries: Vec<MenuChild>) -> MenuItem {
    MenuItem::Submenu(MenuSubmenu {
        label: label.into(),
        enabled,
        visible: true,
        entries,
    })
}
fn model(entries: Vec<MenuItem>) -> MenuModel {
    MenuModel {
        entries,
        x: 17,
        y: -3,
    }
}

#[test]
fn hierarchy_preserves_checkability_parent_gates_and_separators_at_each_level() {
    let mut menu = model(vec![
        MenuItem::Separator { visible: true },
        group(
            "Disabled group",
            false,
            vec![
                MenuChild::Separator { visible: true },
                MenuChild::Command(command(1, "Unchecked", true, Some(false))),
                MenuChild::Separator { visible: true },
                MenuChild::Separator { visible: true },
                MenuChild::Command(command(2, "Checked disabled", false, Some(true))),
                MenuChild::Separator { visible: true },
            ],
        ),
        MenuItem::Separator { visible: true },
        MenuItem::Separator { visible: true },
        group(
            "Active group",
            true,
            vec![MenuChild::Command(command(
                3,
                "Unchecked active",
                true,
                Some(false),
            ))],
        ),
        MenuItem::Command(command(4, "Ordinary", true, None)),
        MenuItem::Separator { visible: true },
    ]);
    assert_eq!(menu.validate(limits(32, 64, 256)), Ok(()));
    assert!(
        !menu.allows_selection(1),
        "a checked flag cannot bypass its disabled parent"
    );
    assert!(!menu.allows_selection(2));
    assert!(menu.allows_selection(3));
    assert!(menu.allows_selection(4));
    assert!(!menu.allows_selection(999));
    menu.normalize();
    assert_eq!((menu.x, menu.y), (17, -3));
    assert_eq!(menu.entries.len(), 4);
    assert!(matches!(
        menu.entries[1],
        MenuItem::Separator { visible: true }
    ));
    let MenuItem::Submenu(disabled) = &menu.entries[0] else {
        panic!("group lost")
    };
    assert_eq!(disabled.entries.len(), 3);
    assert!(matches!(
        disabled.entries[1],
        MenuChild::Separator { visible: true }
    ));
    let MenuChild::Command(unchecked) = &disabled.entries[0] else {
        panic!("child lost")
    };
    assert_eq!(unchecked.checked, Some(false));
    let MenuChild::Command(checked) = &disabled.entries[2] else {
        panic!("child lost")
    };
    assert_eq!(checked.checked, Some(true));
    let MenuItem::Command(ordinary) = &menu.entries[3] else {
        panic!("command lost")
    };
    assert_eq!(ordinary.checked, None);
    let once = menu.clone();
    menu.normalize();
    assert_eq!(menu, once);
}

#[test]
fn raw_total_rows_labels_and_duplicate_ids_are_validated_across_levels() {
    let mut raw = model(vec![group(
        "G",
        true,
        vec![
            MenuChild::Separator { visible: true },
            MenuChild::Separator { visible: true },
            MenuChild::Separator { visible: true },
            MenuChild::Separator { visible: true },
            MenuChild::Command(command(1, "A", true, None)),
        ],
    )]);
    assert_eq!(
        raw.validate(limits(4, 8, 12)),
        Err(MenuModelError::TooManyEntries)
    );
    raw.normalize();
    assert_eq!(raw.validate(limits(4, 8, 12)), Ok(()));
    let cases = [
        (
            model(vec![MenuItem::Separator { visible: true }]),
            MenuModelError::Empty,
        ),
        (
            model(vec![group(
                "G",
                true,
                vec![MenuChild::Separator { visible: true }],
            )]),
            MenuModelError::EmptySubmenu,
        ),
        (
            model(vec![group(
                "",
                true,
                vec![MenuChild::Command(command(1, "A", true, None))],
            )]),
            MenuModelError::EmptyLabel,
        ),
        (
            model(vec![group(
                "G",
                true,
                vec![MenuChild::Command(command(0, "A", true, None))],
            )]),
            MenuModelError::InvalidCommandId,
        ),
        (
            model(vec![group(
                "G",
                true,
                vec![MenuChild::Command(command(1, "中文中文", true, None))],
            )]),
            MenuModelError::LabelTooLong,
        ),
        (
            model(vec![group(
                "ABCDEFG",
                true,
                vec![MenuChild::Command(command(1, "中文", true, None))],
            )]),
            MenuModelError::LabelsTooLarge,
        ),
        (
            model(vec![
                MenuItem::Command(command(1, "A", true, None)),
                group(
                    "G",
                    true,
                    vec![MenuChild::Command(command(1, "B", true, Some(true)))],
                ),
            ]),
            MenuModelError::DuplicateCommandId,
        ),
    ];
    for (menu, error) in cases {
        let original = menu.clone();
        assert_eq!(menu.validate(limits(8, 8, 12)), Err(error));
        assert_eq!(
            menu, original,
            "validation must not normalize or discard rows"
        );
    }
    let duplicate = model(vec![
        MenuItem::Command(command(1, "A", true, None)),
        group(
            "G",
            false,
            vec![MenuChild::Command(command(1, "B", true, None))],
        ),
    ]);
    assert!(!duplicate.allows_selection(1));
}

#[test]
fn legacy_conversion_is_explicit_and_new_debug_does_not_expose_label_text() {
    let legacy = ContextMenu {
        entries: vec![
            MenuEntry::Command {
                command_id: 10,
                label: "private-label-A".into(),
                enabled: true,
                checked: true,
            },
            MenuEntry::Separator,
            MenuEntry::Command {
                command_id: 11,
                label: "private-label-B".into(),
                enabled: false,
                checked: false,
            },
        ],
        x: 12,
        y: 34,
    };
    let mut menu: MenuModel = legacy.into();
    assert_eq!((menu.x, menu.y), (12, 34));
    let MenuItem::Command(first) = &menu.entries[0] else {
        panic!("legacy command lost")
    };
    assert_eq!(first.label, "private-label-A");
    assert_eq!(first.checked, Some(true));
    let MenuItem::Command(last) = &menu.entries[2] else {
        panic!("legacy command lost")
    };
    assert_eq!(last.checked, None);
    assert!(!last.enabled);
    menu.entries.push(group(
        "private-group",
        true,
        vec![MenuChild::Command(command(
            12,
            "private-child",
            true,
            Some(false),
        ))],
    ));
    let debug = format!("{menu:?}");
    for secret in [
        "private-label-A",
        "private-label-B",
        "private-group",
        "private-child",
    ] {
        assert!(!debug.contains(secret));
    }
    assert!(debug.contains("label_bytes"));
}
