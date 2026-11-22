//! External, core-only adapter contract. No CEF, GPUI or window is created.
use std::{
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};

use rust_cef_core::{
    contracts::{BrowserId, RuntimeGeneration},
    menu::{
        ContextMenu, ContextMenuRequestId, ContextMenuResponse, MenuEntry, MenuLimits,
        MenuModelError,
    },
    menu_context::{MenuContext, MenuPresentation},
    menu_model::MenuModel,
    ports::{ContextMenuPresenter, ContextMenuResponseSink, EngineError, WindowBinding},
};

fn limits() -> MenuLimits {
    MenuLimits {
        max_entries: NonZeroUsize::new(4).unwrap(),
        max_label_bytes: NonZeroUsize::new(8).unwrap(),
        max_total_label_bytes: NonZeroUsize::new(12).unwrap(),
    }
}

fn command(id: i32, label: &str, enabled: bool, checked: bool) -> MenuEntry {
    MenuEntry::Command {
        command_id: id,
        label: label.into(),
        enabled,
        checked,
    }
}

fn menu(entries: Vec<MenuEntry>) -> ContextMenu {
    ContextMenu {
        entries,
        x: 17,
        y: 23,
    }
}

#[test]
fn bounded_owned_model_rejects_invalid_or_ambiguous_commands_without_mutating() {
    let good = menu(vec![
        command(100, "选择", true, true),
        MenuEntry::Separator,
        command(101, "Copy", false, false),
    ]);
    let original = good.clone();
    assert_eq!(good.validate(limits()), Ok(()));
    assert_eq!(good, original);
    assert!(good.allows_selection(100));
    assert!(!good.allows_selection(101));
    assert!(!good.allows_selection(102));

    let cases = [
        (menu(vec![MenuEntry::Separator]), MenuModelError::Empty),
        (
            menu(vec![MenuEntry::Separator; 5]),
            MenuModelError::TooManyEntries,
        ),
        (
            menu(vec![command(1, "", true, false)]),
            MenuModelError::EmptyLabel,
        ),
        (
            menu(vec![command(1, "中文中文", true, false)]),
            MenuModelError::LabelTooLong,
        ),
        (
            menu(vec![
                command(1, "12345678", true, false),
                command(2, "12345", true, false),
            ]),
            MenuModelError::LabelsTooLarge,
        ),
        (
            menu(vec![command(0, "None", true, false)]),
            MenuModelError::InvalidCommandId,
        ),
        (
            menu(vec![command(-1, "None", true, false)]),
            MenuModelError::InvalidCommandId,
        ),
        (
            menu(vec![
                command(1, "A", true, false),
                command(1, "B", true, false),
            ]),
            MenuModelError::DuplicateCommandId,
        ),
    ];
    for (model, expected) in cases {
        let before = model.clone();
        assert_eq!(model.validate(limits()), Err(expected));
        assert_eq!(model, before);
    }
    assert!(
        !menu(vec![
            command(1, "A", true, false),
            command(1, "B", false, false)
        ])
        .allows_selection(1)
    );
    assert!(!menu(vec![command(0, "None", true, false)]).allows_selection(0));
    assert!(!menu(vec![command(-1, "None", true, false)]).allows_selection(-1));
    // A disabled-only menu still displays its disabled item.
    assert_eq!(
        menu(vec![command(1, "Copy", false, false)]).validate(limits()),
        Ok(())
    );
}

#[derive(Default)]
struct RecordingEndpoint(Mutex<Vec<(ContextMenuRequestId, ContextMenuResponse)>>);
impl ContextMenuResponseSink for RecordingEndpoint {
    fn submit(
        &self,
        request: ContextMenuRequestId,
        response: ContextMenuResponse,
    ) -> Result<(), EngineError> {
        self.0.lock().unwrap().push((request, response));
        Ok(())
    }
}

struct Opening {
    presentation: MenuPresentation,
    reply: Arc<dyn ContextMenuResponseSink>,
}

/// A stand-in for a host's toolkit gateway, deliberately not a widget/render test.
#[derive(Default)]
struct ToolkitGateway(Option<Opening>);
impl ContextMenuPresenter for ToolkitGateway {
    fn present(
        &mut self,
        presentation: MenuPresentation,
        reply: Arc<dyn ContextMenuResponseSink>,
    ) -> Result<(), EngineError> {
        if self.0.is_some() {
            return Err(EngineError::Backpressure);
        }
        self.0 = Some(Opening {
            presentation,
            reply,
        });
        Ok(())
    }
    fn dismiss(&mut self, id: ContextMenuRequestId) {
        if self
            .0
            .as_ref()
            .is_some_and(|opening| opening.presentation.request == id)
        {
            self.0 = None;
        }
    }
}

fn presentation(request: ContextMenuRequestId, model: MenuModel) -> MenuPresentation {
    let (x, y) = (model.x, model.y);
    MenuPresentation {
        request,
        model,
        context: Arc::new(MenuContext {
            binding: WindowBinding {
                browser: request.browser,
                window: 9,
            },
            binding_revision: 1,
            x,
            y,
            frame_id: "frame-1".into(),
            selection: String::new(),
            misspelled_word: String::new(),
            dictionary_suggestions: Vec::new(),
        }),
    }
}

#[test]
fn external_toolkit_gateway_can_deliver_select_and_cancel_using_only_core_ports() {
    let id = ContextMenuRequestId {
        browser: BrowserId::new(RuntimeGeneration::new(71), 4),
        sequence: 1,
    };
    let next = ContextMenuRequestId { sequence: 2, ..id };
    let model: MenuModel = menu(vec![
        command(100, "Copy", true, true),
        MenuEntry::Separator,
        command(101, "Paste", false, false),
    ])
    .into();
    let endpoint = Arc::new(RecordingEndpoint::default());
    let mut gateway: Box<dyn ContextMenuPresenter> = Box::<ToolkitGateway>::default();
    gateway
        .present(presentation(id, model.clone()), endpoint.clone())
        .unwrap();
    assert_eq!(
        gateway.present(presentation(next, model.clone()), endpoint.clone()),
        Err(EngineError::Backpressure)
    );
    // A stale visual dismiss must not remove the current opening.
    gateway.dismiss(next);
    assert_eq!(
        gateway.present(presentation(next, model.clone()), endpoint.clone()),
        Err(EngineError::Backpressure)
    );
    gateway.dismiss(id);
    gateway
        .present(presentation(next, model.clone()), endpoint.clone())
        .unwrap();
    gateway.dismiss(next);
    assert!(
        endpoint.0.lock().unwrap().is_empty(),
        "visual dismiss alone must not pretend to answer CEF"
    );

    let mut gateway = ToolkitGateway::default();
    gateway
        .present(presentation(id, model.clone()), endpoint.clone())
        .unwrap();
    let opening = gateway.0.take().unwrap();
    assert_eq!(opening.presentation.model, model);
    assert!(opening.presentation.model.allows_selection(100));
    opening
        .reply
        .submit(
            opening.presentation.request,
            ContextMenuResponse::Select { command_id: 100 },
        )
        .unwrap();
    gateway
        .present(presentation(next, model), endpoint.clone())
        .unwrap();
    let opening = gateway.0.take().unwrap();
    opening
        .reply
        .submit(opening.presentation.request, ContextMenuResponse::Cancel)
        .unwrap();
    assert_eq!(
        *endpoint.0.lock().unwrap(),
        vec![
            (id, ContextMenuResponse::Select { command_id: 100 }),
            (next, ContextMenuResponse::Cancel),
        ]
    );
}
