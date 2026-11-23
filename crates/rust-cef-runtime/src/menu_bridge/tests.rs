use super::*;
#[test]
fn fixed_cef_edit_command_ids_keep_paste_match_delete_and_select_all_distinct() {
    use cef::MenuId;
    for (id, native, command) in [
        (110, MenuId::UNDO, EditCommand::Undo),
        (111, MenuId::REDO, EditCommand::Redo),
        (112, MenuId::CUT, EditCommand::Cut),
        (113, MenuId::COPY, EditCommand::Copy),
        (114, MenuId::PASTE, EditCommand::Paste),
        (115, MenuId::PASTE_MATCH_STYLE, EditCommand::PasteMatchStyle),
        (116, MenuId::DELETE, EditCommand::Delete),
        (117, MenuId::SELECT_ALL, EditCommand::SelectAll),
    ] {
        assert_eq!(native.get_raw(), id);
        assert_eq!(edit_command(native.get_raw()), Some(command));
    }
    assert_eq!(edit_command(109), None);
    assert_eq!(edit_command(118), None);
}
use rust_cef_core::{
    menu::MenuLimits,
    menu_context::{MenuContextLimits, MenuPresentation},
    menu_lease::MenuLeaseOwner,
    menu_model::{MenuCommand, MenuSubmenu},
    ports::{MenuDeliveryError, WindowBinding},
};
fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}
fn bid(local: u64) -> BrowserId {
    BrowserId::new(rust_cef_core::contracts::RuntimeGeneration::new(7), local)
}
fn config() -> ContextMenuConfig {
    ContextMenuConfig {
        capacity: nz(1),
        model_limits: MenuLimits {
            max_entries: nz(16),
            max_label_bytes: nz(64),
            max_total_label_bytes: nz(256),
        },
        context_limits: MenuContextLimits {
            max_frame_id_bytes: nz(128),
            max_text_bytes: nz(512),
            max_suggestions: nz(5),
            max_total_bytes: nz(2048),
        },
    }
}
struct Target {
    lease: MenuLease,
}
impl ContextMenuDeliverySink for Target {
    fn capture(&self) -> Result<MenuLeaseStamp, MenuDeliveryError> {
        self.lease.capture().map_err(MenuDeliveryError::Lease)
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
fn target(owner: &MenuLeaseOwner) -> ContextMenuTarget {
    ContextMenuTarget {
        lease: owner.lease(),
        delivery: Arc::new(Target {
            lease: owner.lease(),
        }),
    }
}
fn owner(browser: BrowserId) -> MenuLeaseOwner {
    MenuLeaseOwner::new(WindowBinding { browser, window: 9 }, nz(1)).unwrap()
}
fn scope(b: &Arc<Binding>) -> Scope {
    Scope {
        browser: b.browser,
        native_id: 1,
        frame_id: "fixture-frame".into(),
        context: Arc::new(rust_cef_core::menu_context::MenuContext {
            binding: b.target.lease.snapshot().unwrap().binding,
            binding_revision: 1,
            x: 20,
            y: 20,
            frame_id: "fixture-frame".into(),
            selection: String::new(),
            misspelled_word: String::new(),
            dictionary_suggestions: Vec::new(),
        }),
        lease: b.target.lease.clone(),
        stamp: b.target.lease.capture().unwrap(),
        registry: Weak::new(),
        registration: Arc::downgrade(b),
        serial: b.serial,
        epoch: b.epoch.load(Ordering::Acquire),
        spelling_opening: b.spelling.opening(),
        delivery: Arc::downgrade(&b.target.delivery),
        target: TargetMenuContext::default(),
        synthesized_target: false,
    }
}
#[test]
fn configured_find_keeps_other_required_capabilities_as_whole_model_diagnostics() {
    let mut model = MenuModel {
        entries: vec![MenuItem::Command(MenuCommand {
            command_id: 130,
            label: "Find".into(),
            enabled: true,
            visible: true,
            checked: None,
            radio_group: None,
        })],
        x: 0,
        y: 0,
    };
    assert_eq!(unsupported_with(&model, false, false), Some((130, true)));
    assert_eq!(unsupported_with(&model, false, true), None);
    model.entries.push(MenuItem::Command(MenuCommand {
        command_id: 131,
        label: "Print".into(),
        enabled: false,
        visible: false,
        checked: None,
        radio_group: None,
    }));
    assert_eq!(unsupported_with(&model, true, true), Some((131, true)));
}
#[test]
fn source_ui_registration_requires_original_delivery_and_lease_allocation() {
    let bridge = MenuBridge::new();
    let table = crate::menu_native::NativeMenus::new();
    let c = config();
    bridge
        .configure(c, || table.configure(c.capacity, c.model_limits))
        .unwrap();
    let mut first_owner = owner(bid(1));
    let target = target(&first_owner);
    let (binding, old) = bridge.bind(bid(1), &target, true).unwrap();
    drop(old);
    let registration = ContextMenuRegistration {
        binding: binding.clone(),
        registry: Weak::new(),
    };
    assert!(registration.matches_target(&target).unwrap());
    let other_delivery = ContextMenuTarget {
        lease: target.lease.clone(),
        delivery: Arc::new(Target {
            lease: target.lease.clone(),
        }),
    };
    assert!(!registration.matches_target(&other_delivery).unwrap());
    let imposter = owner(bid(1));
    let other_lease = ContextMenuTarget {
        lease: imposter.lease(),
        delivery: target.delivery.clone(),
    };
    assert!(!registration.matches_target(&other_lease).unwrap());
    first_owner.invalidate().unwrap();
    assert!(registration.matches_target(&target).unwrap());
    binding.retired.store(true, Ordering::Release);
    assert!(!registration.matches_target(&target).unwrap());
}
#[test]
fn bounded_registration_replacement_preserves_old_identity_and_returns_cleanup() {
    let bridge = MenuBridge::new();
    let table = crate::menu_native::NativeMenus::new();
    let c = config();
    bridge
        .configure(c, || table.configure(c.capacity, c.model_limits))
        .unwrap();
    assert!(
        bridge
            .configure(c, || panic!("second configure must not touch book"))
            .is_err()
    );
    let first_owner = owner(bid(1));
    let (first, old) = bridge.bind(bid(1), &target(&first_owner), true).unwrap();
    assert!(old.is_none());
    assert!(bridge.bind(bid(1), &target(&first_owner), true).is_err());
    let peer = owner(bid(2));
    assert!(bridge.bind(bid(2), &target(&peer), true).is_err());
    let old_scope = scope(&first);
    first.retired.store(true, Ordering::Release);
    assert!(bridge.bind(bid(1), &target(&first_owner), false).is_err());
    let next_owner = owner(bid(1));
    let (next, old) = bridge.bind(bid(1), &target(&next_owner), true).unwrap();
    assert!(Arc::ptr_eq(old.as_ref().unwrap(), &first));
    assert!(next.serial > first.serial);
    assert!(!old_scope.current());
    assert!(scope(&next).current());
    // Numerically equal binding/revision still cannot borrow original allocation.
    assert!(!next.target.lease.is_current(&old_scope.stamp).unwrap());
    drop(old);
    bridge.remove(bid(1));
    assert!(next.retired.load(Ordering::Acquire));
    assert!(bridge.get(bid(1)).is_none());
}
#[test]
fn native_epoch_and_ui_lease_both_revoke_without_metadata_substitution() {
    let bridge = MenuBridge::new();
    bridge.configure(config(), || Ok(())).unwrap();
    let mut owner = owner(bid(1));
    let (binding, _) = bridge.bind(bid(1), &target(&owner), true).unwrap();
    let first = scope(&binding);
    assert!(first.admission());
    binding.advance();
    assert!(!first.current());
    let second = scope(&binding);
    assert!(second.admission());
    assert_eq!(first.stamp.revision(), second.stamp.revision());
    owner.invalidate().unwrap();
    assert!(!second.admission());
    assert_eq!(binding.snapshot().invalidated, 1);
    assert!(binding.snapshot().first_failure.is_none());
}
#[test]
fn source_capability_does_not_hide_other_required_provider_failures() {
    let command = |id| {
        MenuItem::Command(MenuCommand {
            command_id: id,
            label: "fixture".into(),
            enabled: true,
            visible: true,
            checked: None,
            radio_group: None,
        })
    };
    let mut model = MenuModel {
        entries: vec![command(132)],
        x: 20,
        y: 20,
    };
    assert_eq!(unsupported_for(&model, false), Some((132, true)));
    assert_eq!(unsupported_for(&model, true), None);
    model.entries.push(command(131));
    assert_eq!(unsupported_for(&model, true), Some((131, true)));
    assert_eq!(model.entries.len(), 2);
}
#[test]
fn whole_model_missing_capability_is_diagnosed_without_hiding_raw_rows() {
    let command = |id| MenuCommand {
        command_id: id,
        label: "fixture".into(),
        enabled: true,
        visible: true,
        checked: None,
        radio_group: None,
    };
    let mut model = MenuModel {
        entries: vec![MenuItem::Command(command(116))],
        x: 20,
        y: 20,
    };
    assert_eq!(unsupported(&model), None);
    let mut print = command(131);
    print.enabled = false;
    print.visible = false;
    model.entries.push(MenuItem::Submenu(MenuSubmenu {
        label: "raw child".into(),
        enabled: false,
        visible: false,
        entries: vec![MenuChild::Command(print)],
    }));
    assert_eq!(unsupported(&model), Some((131, true)));
    assert_eq!(model.entries.len(), 2);
    model.entries = vec![MenuItem::Command(command(26501))];
    assert_eq!(unsupported(&model), Some((26501, false)));
}

#[test]
fn devtools_items_are_appended_only_when_the_host_opted_in() {
    let mut model = MenuModel {
        entries: vec![MenuItem::Command(MenuCommand {
            command_id: 117,
            label: "Select all".into(),
            enabled: true,
            visible: true,
            checked: None,
            radio_group: None,
        })],
        x: 0,
        y: 0,
        ..Default::default()
    };
    append_capability_items(
        &mut model,
        MenuCapabilities {
            devtools: true,
            ..Default::default()
        },
    );
    let ids: Vec<i32> = model
        .entries
        .iter()
        .filter_map(|e| match e {
            MenuItem::Command(c) => Some(c.command_id),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec![117, 28440, 28441]);
    assert!(matches!(
        model.entries[1],
        MenuItem::Separator { visible: true }
    ));
    // 28440/28441 are refused while nothing has configured DevTools capacity,
    // and accepted once the host opted in, exactly like CefSharp's flag.
    assert_eq!(unsupported(&model), Some((28440, true)));
    assert_eq!(
        unsupported_capabilities(&model, false, false, true, false, |_| false),
        None
    );
}

#[test]
fn capability_items_are_offered_only_for_configured_host_services() {
    let base = MenuModel {
        entries: vec![MenuItem::Command(MenuCommand {
            command_id: 117,
            label: "Select all".into(),
            enabled: true,
            visible: true,
            checked: None,
            radio_group: None,
        })],
        ..Default::default()
    };
    let ids = |caps: MenuCapabilities| {
        let mut m = base.clone();
        append_capability_items(&mut m, caps);
        m.entries
            .iter()
            .filter_map(|e| match e {
                MenuItem::Command(c) => Some(c.command_id),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(MenuCapabilities::default()), vec![117]);
    assert_eq!(
        ids(MenuCapabilities {
            find: true,
            source: true,
            devtools: true,
            structure: false,
            navigation: false,
            ..Default::default()
        }),
        vec![117, 130, 132, 28440, 28441]
    );
    // Nothing is appended twice: the provider runs once per capture.
    let mut once = base.clone();
    append_capability_items(
        &mut once,
        MenuCapabilities {
            source: true,
            ..Default::default()
        },
    );
    let n = once.entries.len();
    assert_eq!(n, 3);
}

/// The structural forms CEF's windowless edit model lacks must be reachable as
/// host items, and the checkable one must really be checkable.
#[test]
fn structure_items_are_appended_as_a_checked_command_and_one_level_submenu() {
    let mut model = MenuModel {
        entries: vec![MenuItem::Command(MenuCommand {
            command_id: 117,
            label: "Select all".into(),
            enabled: true,
            visible: true,
            checked: None,
            radio_group: None,
        })],
        ..Default::default()
    };
    append_capability_items(
        &mut model,
        MenuCapabilities {
            structure: true,
            ..Default::default()
        },
    );
    let checked = model.entries.iter().find_map(|e| match e {
        MenuItem::Command(c) if c.command_id == MENU_CHECKED_ID => Some(c.checked),
        _ => None,
    });
    assert_eq!(checked, Some(Some(true)));
    let submenu = model.entries.iter().find_map(|e| match e {
        MenuItem::Submenu(s) => Some(s),
        _ => None,
    });
    let submenu = submenu.expect("one-level submenu appended");
    assert_eq!(submenu.entries.len(), 1);
    assert!(matches!(
        submenu.entries[0],
        MenuChild::Command(MenuCommand {
            command_id: MENU_SUBMENU_CHILD_ID,
            ..
        })
    ));
    // Depth stays one level: the child carries no submenu of its own.
    assert!(!matches!(submenu.entries[0], MenuChild::Separator { .. }));
}

/// The structural ids are a host capability, not a CEF command: with the opt-in
/// off they must be refused as a whole model rather than silently forwarded.
#[test]
fn structural_ids_are_refused_unless_the_host_opted_in() {
    let model = MenuModel {
        entries: vec![
            MenuItem::Command(MenuCommand {
                command_id: MENU_CHECKED_ID,
                label: "Show bookmarks bar".into(),
                enabled: true,
                visible: true,
                checked: Some(true),
                radio_group: None,
            }),
            MenuItem::Command(MenuCommand {
                command_id: MENU_SUBMENU_CHILD_ID,
                label: "Host submenu item".into(),
                enabled: true,
                visible: true,
                checked: None,
                radio_group: None,
            }),
        ],
        ..Default::default()
    };
    assert_eq!(
        unsupported_capabilities(&model, false, false, false, false, |_| false),
        Some((MENU_CHECKED_ID, true))
    );
    assert_eq!(
        unsupported_capabilities(&model, false, false, false, true, |_| false),
        None
    );
}

#[test]
fn blank_area_navigation_items_are_prepended_only_when_non_editable() {
    let mut blank = MenuModel::default();
    // With navigation opt-in off, blank model stays empty.
    append_capability_items(&mut blank, MenuCapabilities::default());
    assert!(blank.entries.is_empty());

    // Non-editable blank area gets Back/Forward/Reload/Select All (+ Copy when selected).
    prepend_navigation_items(&mut blank, false, true, false, false);
    let ids: Vec<i32> = blank
        .entries
        .iter()
        .filter_map(|e| match e {
            MenuItem::Command(c) => Some(c.command_id),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec![100, 101, 102, 117]);
    assert!(
        matches!(blank.entries[0], MenuItem::Command(ref c) if c.command_id == 100 && c.enabled)
    );
    assert!(
        matches!(blank.entries[1], MenuItem::Command(ref c) if c.command_id == 101 && !c.enabled)
    );
    assert_eq!(unsupported(&blank), None);
    assert!(blank.validate(config().model_limits).is_ok());

    let mut with_selection = MenuModel::default();
    prepend_navigation_items(&mut with_selection, false, false, true, true);
    let sel_ids: Vec<i32> = with_selection
        .entries
        .iter()
        .filter_map(|e| match e {
            MenuItem::Command(c) => Some(c.command_id),
            _ => None,
        })
        .collect();
    assert_eq!(sel_ids, vec![100, 101, 102, 113, 117]);
    assert!(with_selection.validate(config().model_limits).is_ok());

    // Editable fields or models already carrying edit commands (110..=116) stay untouched.
    let mut editable_empty = MenuModel::default();
    prepend_navigation_items(&mut editable_empty, true, true, true, true);
    assert!(editable_empty.entries.is_empty());

    let mut editable_cut = MenuModel {
        entries: vec![MenuItem::Command(MenuCommand {
            command_id: 112,
            label: "Cut".into(),
            enabled: true,
            visible: true,
            checked: None,
            radio_group: None,
        })],
        ..Default::default()
    };
    prepend_navigation_items(&mut editable_cut, false, true, true, true);
    assert_eq!(editable_cut.entries.len(), 1);
}

#[test]
fn filter_supported_and_prepend_handles_cef_raw_blank_and_editable_models() {
    let cmd = |id: i32, label: &str| {
        MenuItem::Command(MenuCommand {
            command_id: id,
            label: label.into(),
            enabled: true,
            visible: true,
            checked: None,
            radio_group: None,
        })
    };
    let mut raw_blank = MenuModel {
        entries: vec![
            cmd(100, "Back"),
            cmd(101, "Forward"),
            cmd(102, "Reload"),
            MenuItem::Separator { visible: true },
            cmd(131, "Print..."),
            cmd(132, "View source"),
        ],
        ..Default::default()
    };
    let caps = MenuCapabilities {
        source: true,
        devtools: true,
        navigation: true,
        ..Default::default()
    };
    filter_supported_items(&mut raw_blank, caps, |_| false);
    prepend_navigation_items(&mut raw_blank, false, false, false, false);
    append_capability_items(&mut raw_blank, caps);
    let ids: Vec<i32> = raw_blank
        .entries
        .iter()
        .filter_map(|e| match e {
            MenuItem::Command(c) => Some(c.command_id),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec![100, 101, 102, 117, 132, 28440, 28441]);
    assert!(raw_blank.validate(config().model_limits).is_ok());
    assert_eq!(
        unsupported_capabilities(&raw_blank, true, false, true, false, |_| false),
        None
    );

    let mut raw_editable = MenuModel {
        entries: vec![
            cmd(205, "No spelling suggestions"),
            MenuItem::Separator { visible: true },
            cmd(110, "Undo"),
            cmd(113, "Copy"),
            cmd(114, "Paste"),
            cmd(117, "Select all"),
        ],
        ..Default::default()
    };
    filter_supported_items(&mut raw_editable, caps, |_| false);
    prepend_navigation_items(&mut raw_editable, true, true, true, true);
    let edit_ids: Vec<i32> = raw_editable
        .entries
        .iter()
        .filter_map(|e| match e {
            MenuItem::Command(c) => Some(c.command_id),
            _ => None,
        })
        .collect();
    assert_eq!(edit_ids, vec![110, 113, 114, 117]);
    assert!(!matches!(
        raw_editable.entries.first(),
        Some(MenuItem::Separator { .. })
    ));
}

#[test]
fn link_and_image_chromium_commands_survive_filter_with_download_gating_and_no_blank_nav() {
    let cmd = |id: i32, label: &str| {
        MenuItem::Command(MenuCommand {
            command_id: id,
            label: label.into(),
            enabled: true,
            visible: true,
            checked: None,
            radio_group: None,
        })
    };
    let make_model = || MenuModel {
        entries: vec![
            cmd(50100, "Open link in new tab"),
            cmd(50104, "Copy link address"),
            cmd(50120, "Save image as..."),
            cmd(50121, "Copy image address"),
            cmd(50122, "Copy image"),
            cmd(50162, "Inspect"), // stripped because managed 28440 is appended
        ],
        ..Default::default()
    };

    let no_dl = MenuCapabilities {
        devtools: true,
        navigation: true,
        download: false,
        ..Default::default()
    };
    let mut m1 = make_model();
    filter_supported_items(&mut m1, no_dl, |_| false);
    prepend_navigation_items(&mut m1, false, true, true, false);
    append_capability_items(&mut m1, no_dl);
    let ids1: Vec<i32> = m1
        .entries
        .iter()
        .filter_map(|e| match e {
            MenuItem::Command(c) => Some(c.command_id),
            _ => None,
        })
        .collect();
    assert_eq!(ids1, vec![50100, 50104, 50121, 50122, 28440, 28441]);
    assert_eq!(
        unsupported_capabilities_with_caps(&m1, no_dl, |_| false),
        None
    );

    let with_dl = MenuCapabilities {
        download: true,
        ..no_dl
    };
    let mut m2 = make_model();
    filter_supported_items(&mut m2, with_dl, |_| false);
    prepend_navigation_items(&mut m2, false, true, true, false);
    append_capability_items(&mut m2, with_dl);
    let ids2: Vec<i32> = m2
        .entries
        .iter()
        .filter_map(|e| match e {
            MenuItem::Command(c) => Some(c.command_id),
            _ => None,
        })
        .collect();
    assert_eq!(ids2, vec![50100, 50104, 50120, 50121, 50122, 28440, 28441]);
    assert_eq!(
        unsupported_capabilities_with_caps(&m2, with_dl, |_| false),
        None
    );
}

#[test]
fn alloy_target_items_synthesize_for_links_and_images_respecting_popup_and_download_caps() {
    let cmd = |id: i32, label: &str| {
        MenuItem::Command(MenuCommand {
            command_id: id,
            label: label.into(),
            enabled: true,
            visible: true,
            checked: None,
            radio_group: None,
        })
    };
    let extract_ids = |m: &MenuModel| -> Vec<i32> {
        m.entries
            .iter()
            .filter_map(|e| match e {
                MenuItem::Command(c) => Some(c.command_id),
                _ => None,
            })
            .collect()
    };

    let full_caps = MenuCapabilities {
        devtools: true,
        navigation: true,
        download: true,
        popup: true,
        ..Default::default()
    };

    // Link only in Alloy model (starts with Back/Forward/Reload/Select All).
    let mut link_model = MenuModel {
        entries: vec![
            cmd(100, "Back"),
            cmd(101, "Forward"),
            cmd(102, "Reload"),
            cmd(117, "Select All"),
        ],
        ..Default::default()
    };
    let link_target = TargetMenuContext {
        link_url: Some("https://cn.bing.com/".into()),
        ..Default::default()
    };
    assert!(prepend_target_items(
        &mut link_model,
        &link_target,
        full_caps
    ));
    prepend_navigation_items(&mut link_model, false, false, false, false);
    append_capability_items(&mut link_model, full_caps);
    assert_eq!(
        extract_ids(&link_model),
        vec![50100, 50103, 50104, 117, 28440, 28441]
    );
    assert_eq!(
        unsupported_capabilities_with_caps(&link_model, full_caps, |_| false),
        None
    );

    // Image only (data: URL, has_image_contents = true), download & popup gated.
    let mut img_model = MenuModel {
        entries: vec![cmd(102, "Reload"), cmd(117, "Select All")],
        ..Default::default()
    };
    let img_target = TargetMenuContext {
        source_url: Some("data:image/png;base64,iVBOR==".into()),
        is_image: true,
        has_image_contents: true,
        ..Default::default()
    };
    assert!(prepend_target_items(&mut img_model, &img_target, full_caps));
    assert_eq!(
        extract_ids(&img_model),
        vec![50123, 50120, 50122, 50121, 117]
    );

    let limited_caps = MenuCapabilities {
        navigation: true,
        download: false,
        popup: false,
        ..Default::default()
    };
    let mut combined = MenuModel::default();
    let both_target = TargetMenuContext {
        link_url: Some("https://cn.bing.com/".into()),
        source_url: Some("https://cn.bing.com/logo.png".into()),
        is_image: true,
        has_image_contents: false,
    };
    assert!(prepend_target_items(
        &mut combined,
        &both_target,
        limited_caps
    ));
    assert_eq!(extract_ids(&combined), vec![50104, 50121]);
}
