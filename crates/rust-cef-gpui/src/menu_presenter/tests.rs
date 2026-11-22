use super::*;
use rust_cef_core::contracts::{BrowserId, RuntimeGeneration};
#[test]
fn last_prepaint_layout_is_per_opening_visible_and_keeps_disabled_parent_state() {
    let id = ContextMenuRequestId {
        browser: BrowserId::new(RuntimeGeneration::new(9), 3),
        sequence: 40,
    };
    let command = MenuCommand {
        command_id: 116,
        label: "select".into(),
        enabled: true,
        visible: true,
        checked: None,
        radio_group: None,
    };
    let mut hidden = command.clone();
    hidden.command_id = 117;
    hidden.visible = false;
    let mut child = command.clone();
    child.command_id = 118;
    let model = MenuModel {
        x: 0,
        y: 0,
        entries: vec![
            MenuItem::Command(command),
            MenuItem::Command(hidden),
            MenuItem::Submenu(MenuSubmenu {
                label: "child".into(),
                enabled: false,
                visible: true,
                entries: vec![MenuChild::Command(child)],
            }),
        ],
    };
    let mut a = MenuPresenterLayout::new(id, &model);
    let b = MenuPresenterLayout::new(ContextMenuRequestId { sequence: 41, ..id }, &model);
    assert_eq!(a.commands.len(), 2);
    assert!(a.commands[0].enabled);
    assert!(!a.commands[1].enabled);
    let bounds = gpui::Bounds::new(gpui::point(px(20.), px(30.)), gpui::size(px(100.), px(24.)));
    a.command(116, bounds);
    assert!(a.commands[0].bounds.is_some());
    assert!(b.commands[0].bounds.is_none());
    assert_ne!(a.request, b.request);
    a.command(
        116,
        gpui::Bounds::new(
            gpui::point(px(f32::NAN), px(0.)),
            gpui::size(px(100.), px(24.)),
        ),
    );
    assert!(a.commands[0].bounds.is_none());
}

use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn replacement_open_retains_a_reply_slot_with_one_old_deferred_cancel_not_two() {
    assert!(can_open(0, false));
    assert!(can_open(1, false));
    assert!(can_open(0, true));
    assert!(!can_open(1, true));
    assert!(!can_open(2, false));
    assert!(!can_open(2, true));
    assert!(!can_open(usize::MAX, true));
}

#[test]
fn marker_distinguishes_all_five_visual_states_without_toggling_native_model() {
    let mut command = MenuCommand {
        command_id: 1,
        label: "Example".into(),
        enabled: false,
        visible: true,
        checked: None,
        radio_group: None,
    };
    assert_eq!(marker(&command), "");
    command.checked = Some(false);
    assert_eq!(marker(&command), "☐");
    command.checked = Some(true);
    assert_eq!(marker(&command), "☑");
    command.radio_group = Some(77);
    assert_eq!(marker(&command), "●");
    command.checked = Some(false);
    assert_eq!(marker(&command), "○");
    assert_eq!(command.radio_group, Some(77));
    assert_eq!(command.checked, Some(false));
    assert!(!command.enabled);
}
#[test]
fn anchor_is_original_view_dip_plus_origin_once_and_refuses_stale_invalid_geometry() {
    let p = anchor((21., 34., 320., 240.), 20, 30).unwrap();
    assert_eq!((f32::from(p.x), f32::from(p.y)), (41., 64.));
    for geometry in [
        (0., 0., 0., 240.),
        (f32::NAN, 0., 320., 240.),
        (0., 0., 320., f32::INFINITY),
        (0., 0., -1., 240.),
    ] {
        assert!(anchor(geometry, 1, 1).is_none());
    }
    for (x, y) in [(-1, 0), (0, -1), (320, 0), (0, 240)] {
        assert!(anchor((21., 34., 320., 240.), x, y).is_none());
    }
}
struct Port {
    calls: AtomicUsize,
    received: Mutex<Vec<(ContextMenuRequestId, ContextMenuResponse)>>,
}
impl ContextMenuResponseSink for Port {
    fn submit(
        &self,
        id: ContextMenuRequestId,
        value: ContextMenuResponse,
    ) -> Result<(), EngineError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.received.lock().unwrap().push((id, value));
        Err(EngineError::PostRejected)
    }
}
#[test]
fn selected_then_widget_dismiss_claims_once_and_withdrawn_work_cancels_only_original_id() {
    let id = ContextMenuRequestId {
        browser: BrowserId::new(RuntimeGeneration::new(9), 3),
        sequence: 17,
    };
    let port = Arc::new(Port {
        calls: AtomicUsize::new(0),
        received: Mutex::new(Vec::new()),
    });
    let reply = Reply {
        id,
        port: RefCell::new(Some(port.clone())),
        choice: Cell::new(None),
        refused: Cell::new(false),
        withdrawn: Cell::new(false),
    };
    let selected = ContextMenuResponse::Select { command_id: 111 };
    let work = reply.claim(selected).unwrap();
    assert!(reply.claim(ContextMenuResponse::Cancel).is_none());
    assert_eq!(port.calls.load(Ordering::SeqCst), 0);
    assert!(reply.port.try_borrow_mut().is_ok());
    assert_eq!(reply.actual(selected, true), selected);
    assert_eq!(reply.actual(selected, false), ContextMenuResponse::Cancel);
    reply.withdrawn.set(true);
    let outcome = reply.attempt(work, selected, true);
    assert_eq!(outcome.request, id);
    assert_eq!(outcome.requested, selected);
    assert_eq!(outcome.submitted, ContextMenuResponse::Cancel);
    assert_eq!(outcome.result, Err(EngineError::PostRejected));
    assert_eq!(
        *port.received.lock().unwrap(),
        vec![(id, ContextMenuResponse::Cancel)]
    );
    assert_eq!(port.calls.load(Ordering::SeqCst), 1);
    assert!(reply.claim(selected).is_none());
}

#[test]
fn refused_selection_keeps_visual_slot_for_explicit_host_cleanup() {
    let id = ContextMenuRequestId {
        browser: BrowserId::new(RuntimeGeneration::new(9), 3),
        sequence: 18,
    };
    let port = Arc::new(Port {
        calls: AtomicUsize::new(0),
        received: Mutex::new(Vec::new()),
    });
    let reply = Reply {
        id,
        port: RefCell::new(Some(port.clone())),
        choice: Cell::new(None),
        refused: Cell::new(false),
        withdrawn: Cell::new(false),
    };
    let choice = ContextMenuResponse::Select { command_id: 111 };
    assert!(reply.presented());
    let work = reply.claim(choice).unwrap();
    assert!(!reply.presented());
    let outcome = reply.attempt(work, choice, true);
    assert!(outcome.result.is_err());
    assert!(reply.presented());
    assert!(reply.claim(ContextMenuResponse::Cancel).is_none());
    assert_eq!(port.calls.load(Ordering::SeqCst), 1);
    assert_eq!(*port.received.lock().unwrap(), vec![(id, choice)]);
}

#[test]
fn captured_menu_revision_is_not_the_ordinary_paint_token_revision() {
    use rust_cef_core::{menu_lease::MenuLeaseOwner, ports::WindowBinding};
    use std::num::NonZeroUsize;
    let binding = WindowBinding {
        browser: BrowserId::new(RuntimeGeneration::new(9), 3),
        window: 21,
    };
    let owner = MenuLeaseOwner::new(binding, NonZeroUsize::new(1).unwrap()).unwrap();
    let stamp = owner.lease().capture().unwrap();
    let paint = WindowToken::new(binding);
    paint.advance().unwrap();
    let id = ContextMenuRequestId {
        browser: binding.browser,
        sequence: 19,
    };
    // Metadata qualification only; a real opening still needs raw validation
    // and MenuFocusRegistration::is_current's ORIGINAL allocation/live gate.
    let mut p = MenuPresentation {
        request: id,
        model: MenuModel::default(),
        context: Arc::new(MenuContext {
            binding,
            binding_revision: stamp.revision(),
            x: 0,
            y: 0,
            frame_id: "frame".into(),
            selection: String::new(),
            misspelled_word: String::new(),
            dictionary_suggestions: Vec::new(),
        }),
    };
    assert!(MenuFocusRegistration::captured_matches(id, &stamp, &p));
    paint.advance().unwrap();
    assert!(MenuFocusRegistration::captured_matches(id, &stamp, &p));
    Arc::make_mut(&mut p.context).binding_revision += 1;
    assert!(!MenuFocusRegistration::captured_matches(id, &stamp, &p));
    Arc::make_mut(&mut p.context).binding_revision = stamp.revision();
    p.request.sequence += 1;
    assert!(!MenuFocusRegistration::captured_matches(id, &stamp, &p));
}

#[test]
fn point_in_bounds_treats_outside_missing_and_non_finite_clicks_as_outside() {
    let rect = Some(MenuBounds {
        left: 20.,
        top: 30.,
        width: 180.,
        height: 120.,
    });
    assert!(point_in_bounds([20., 30.], rect));
    assert!(point_in_bounds([110., 90.], rect));
    assert!(point_in_bounds([200., 150.], rect));
    assert!(!point_in_bounds([19.9, 90.], rect));
    assert!(!point_in_bounds([200.1, 90.], rect));
    assert!(!point_in_bounds([110., 29.9], rect));
    assert!(!point_in_bounds([110., 150.1], rect));
    assert!(!point_in_bounds([f32::NAN, 90.], rect));
    assert!(!point_in_bounds([110., f32::INFINITY], rect));
    assert!(!point_in_bounds([110., 90.], None));
}

struct OkPort {
    calls: AtomicUsize,
    received: Mutex<Vec<(ContextMenuRequestId, ContextMenuResponse)>>,
}
impl ContextMenuResponseSink for OkPort {
    fn submit(
        &self,
        id: ContextMenuRequestId,
        value: ContextMenuResponse,
    ) -> Result<(), EngineError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.received.lock().unwrap().push((id, value));
        Ok(())
    }
}

#[test]
fn withdraw_on_focus_loss_claims_cancel_once_before_late_modal_unwind() {
    let id = ContextMenuRequestId {
        browser: BrowserId::new(RuntimeGeneration::new(9), 4),
        sequence: 22,
    };
    let port = Arc::new(OkPort {
        calls: AtomicUsize::new(0),
        received: Mutex::new(Vec::new()),
    });
    let reply = Reply {
        id,
        port: RefCell::new(Some(port.clone())),
        choice: Cell::new(None),
        refused: Cell::new(false),
        withdrawn: Cell::new(false),
    };

    // Simulate window deactivation / focus loss calling withdraw(): marks withdrawn
    // and claims the response port with Cancel.
    reply.withdrawn.set(true);
    let cancel_port = reply
        .claim(ContextMenuResponse::Cancel)
        .expect("withdraw claims port once");
    let cancel_outcome = reply.attempt(cancel_port, ContextMenuResponse::Cancel, false);
    assert_eq!(cancel_outcome.submitted, ContextMenuResponse::Cancel);
    assert_eq!(cancel_outcome.result, Ok(()));

    // When TrackPopupMenuEx subsequently unwinds and reports either a late
    // Selected item or Cancelled, claim() MUST return None so the native menu
    // callback is never answered twice or overwritten with Select.
    assert!(
        reply
            .claim(ContextMenuResponse::Select { command_id: 28500 })
            .is_none()
    );
    assert!(reply.claim(ContextMenuResponse::Cancel).is_none());
    assert_eq!(port.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *port.received.lock().unwrap(),
        vec![(id, ContextMenuResponse::Cancel)]
    );
}
