//! Command admission, cancellation, consumption, and engine handoff.
use rust_cef_core::contracts::{
    BrowserConfig, BrowserId, RenderPreference, RuntimeGeneration, Viewport,
};
use rust_cef_core::ports::{
    BrowserCommand, BrowserEngine, CommandEnvelope, CommandId, EngineError,
};
use rust_cef_core::session::{COMMAND_CAPACITY, Session, SessionState};

fn browser(local: u64) -> BrowserId {
    BrowserId::new(RuntimeGeneration::new(1), local)
}

fn session(local: u64) -> Session {
    Session::begin_create(
        browser(local),
        BrowserConfig {
            initial_url: "data:text/html,fixture".into(),
            profile: None,
            render_preference: RenderPreference::SoftwareRequired,
            viewport: Viewport {
                width: 320,
                height: 240,
                scale: 1.0,
            },
            options: Default::default(),
        },
    )
    .unwrap()
}

fn alive(local: u64) -> Session {
    let mut session = session(local);
    session.after_created();
    assert_eq!(session.snapshot().state, SessionState::Alive);
    session
}

fn five_navigations() -> [BrowserCommand; 5] {
    [
        BrowserCommand::Navigate("data:text/html,nav".into()),
        BrowserCommand::Back,
        BrowserCommand::Forward,
        BrowserCommand::Reload,
        BrowserCommand::StopLoading,
    ]
}

#[derive(Default)]
struct RecordingEngine {
    commands: Vec<BrowserCommand>,
}

impl BrowserEngine for RecordingEngine {
    fn execute(&mut self, command: CommandEnvelope) -> Result<(), EngineError> {
        self.commands.push(command.command);
        Ok(())
    }
}

#[test]
fn creation_acceptance_is_not_ready_but_after_created_is_immediate() {
    let mut creating = session(1);
    creating.accept_create_request();
    assert_eq!(creating.snapshot().state, SessionState::Creating);
    assert!(!creating.snapshot().is_ready());
    assert_eq!(
        creating.admit_command(BrowserCommand::Reload),
        Err(EngineError::NotReady)
    );

    creating.after_created();
    assert_eq!(creating.snapshot().state, SessionState::Alive);
    assert!(creating.snapshot().is_ready());
    assert!(creating.admit_command(BrowserCommand::Reload).is_ok());
}

#[test]
fn five_navigation_commands_execute_through_accepted_tokens() {
    let mut session = alive(1);
    let mut engine = RecordingEngine::default();
    let mut ids = Vec::new();

    for command in five_navigations() {
        let envelope = session.admit_command(command).unwrap();
        let accepted = session.consume_command(envelope).unwrap();
        ids.push(accepted.id());
        accepted.execute(&mut engine).unwrap();
    }

    assert_eq!(engine.commands, five_navigations());
    assert_eq!(session.pending_commands(), 0);
    let sequences: Vec<u64> = ids.iter().map(|id| id.sequence).collect();
    assert_eq!(sequences, [1, 2, 3, 4, 5]);
    assert!(
        ids.iter()
            .all(|id| id.browser == session.snapshot().browser)
    );
}

#[test]
fn capacity_is_exact_and_consumption_releases_a_slot() {
    let mut session = alive(1);
    let mut admitted = Vec::new();
    for _ in 0..COMMAND_CAPACITY {
        admitted.push(session.admit_command(BrowserCommand::Reload).unwrap());
    }
    assert_eq!(session.pending_commands(), COMMAND_CAPACITY);
    assert_eq!(
        session.admit_command(BrowserCommand::Reload),
        Err(EngineError::Backpressure)
    );

    let first = admitted.remove(0);
    let accepted = session.consume_command(first).unwrap();
    assert_eq!(session.pending_commands(), COMMAND_CAPACITY - 1);
    let replacement = session.admit_command(BrowserCommand::Reload).unwrap();
    assert_eq!(replacement.id.sequence, COMMAND_CAPACITY as u64 + 1);
    assert_eq!(session.pending_commands(), COMMAND_CAPACITY);
    accepted.execute(&mut RecordingEngine::default()).unwrap();
}

#[test]
fn closing_rejects_new_commands_but_consumes_admitted_ones() {
    let mut session = alive(1);
    let kept = session.admit_command(BrowserCommand::Reload).unwrap();
    let rejected_by_native = session.admit_command(BrowserCommand::Back).unwrap();
    session.close_requested();

    assert_eq!(
        session.admit_command(BrowserCommand::Forward),
        Err(EngineError::Closing)
    );
    assert!(session.cancel_command(rejected_by_native.id));
    assert!(!session.cancel_command(rejected_by_native.id));
    assert!(!session.cancel_command(CommandId {
        browser: browser(99),
        sequence: kept.id.sequence,
    }));
    assert_eq!(session.pending_commands(), 1);

    let accepted = session.consume_command(kept).unwrap();
    let mut engine = RecordingEngine::default();
    accepted.execute(&mut engine).unwrap();
    assert_eq!(engine.commands, [BrowserCommand::Reload]);
    assert_eq!(session.pending_commands(), 0);
}

#[test]
fn duplicate_and_foreign_envelopes_do_not_consume_local_state() {
    let mut session = alive(1);
    let local = session.admit_command(BrowserCommand::Reload).unwrap();
    let duplicate = local.clone();
    let foreign = CommandEnvelope {
        id: CommandId {
            browser: BrowserId::new(RuntimeGeneration::new(2), 1),
            sequence: local.id.sequence,
        },
        command: BrowserCommand::Back,
    };

    assert!(session.consume_command(foreign).is_none());
    assert_eq!(session.pending_commands(), 1);
    let accepted = session.consume_command(duplicate).unwrap();
    assert!(session.consume_command(local).is_none());
    assert_eq!(session.pending_commands(), 0);
    assert_eq!(accepted.id().browser, browser(1));
}

#[test]
fn terminal_sessions_never_execute() {
    let mut closed = alive(1);
    let admitted = closed.admit_command(BrowserCommand::Reload).unwrap();
    closed.closed();
    assert!(closed.consume_command(admitted).is_none());
    assert_eq!(closed.pending_commands(), 0);
    assert_eq!(
        closed.admit_command(BrowserCommand::Reload),
        Err(EngineError::Closed)
    );

    let mut failed = session(2);
    failed.create_rejected();
    assert_eq!(
        failed.admit_command(BrowserCommand::Reload),
        Err(EngineError::NotReady)
    );
}

#[test]
fn complete_mouse_click_is_one_bounded_admission_across_close_cutpoint() {
    use rust_cef_core::ports::{MouseClick, MouseModifiers};
    use std::num::NonZeroU8;

    let command = BrowserCommand::MouseClick(MouseClick {
        x: 37,
        y: 41,
        modifiers: MouseModifiers::default(),
        click_count: NonZeroU8::new(1).unwrap(),
    });
    let mut creating = session(3);
    assert_eq!(
        creating.admit_command(command.clone()),
        Err(EngineError::NotReady)
    );
    let mut state = alive(3);
    for _ in 0..COMMAND_CAPACITY - 1 {
        state.admit_command(BrowserCommand::Reload).unwrap();
    }
    let accepted_click = state.admit_command(command.clone()).unwrap();
    assert_eq!(accepted_click.id.sequence, COMMAND_CAPACITY as u64);
    assert_eq!(state.pending_commands(), COMMAND_CAPACITY);
    assert_eq!(
        state.admit_command(command.clone()),
        Err(EngineError::Backpressure)
    );
    assert_eq!(state.pending_commands(), COMMAND_CAPACITY);
    state.close_requested();
    assert_eq!(
        state.admit_command(command.clone()),
        Err(EngineError::Closing)
    );
    let accepted = state.consume_command(accepted_click).unwrap();
    assert_eq!(state.pending_commands(), COMMAND_CAPACITY - 1);
    let mut engine = RecordingEngine::default();
    accepted.execute(&mut engine).unwrap();
    assert_eq!(engine.commands, [command]);
}

#[test]
fn touch_accessibility_and_outbound_drag_finish_admission_contracts() {
    use rust_cef_core::contracts::CefFeatureState;
    use rust_cef_core::input::{BrowserTouchEvent, TouchEventType, TouchPointerType};
    use rust_cef_core::outbound_drag::{DragOperationMask, DragSessionId, OutboundDragFinish};
    use rust_cef_core::ports::MouseModifiers;

    let mut state = alive(5);

    // Touch validation in admit_command.
    let bad_touch = BrowserCommand::TouchEvent(BrowserTouchEvent {
        id: 0,
        x: f32::INFINITY,
        y: 10.0,
        radius_x: 1.0,
        radius_y: 1.0,
        rotation_angle: 0.0,
        pressure: 0.5,
        event_type: TouchEventType::Pressed,
        modifiers: MouseModifiers::default(),
        pointer_type: TouchPointerType::Touch,
    });
    assert_eq!(
        state.admit_command(bad_touch),
        Err(EngineError::InvalidRequest)
    );

    let good_touch = BrowserCommand::TouchEvent(BrowserTouchEvent {
        id: 2,
        x: 100.0,
        y: 200.0,
        radius_x: 4.0,
        radius_y: 4.0,
        rotation_angle: 0.0,
        pressure: 0.75,
        event_type: TouchEventType::Pressed,
        modifiers: MouseModifiers::default(),
        pointer_type: TouchPointerType::Touch,
    });
    let env_touch = state.admit_command(good_touch).unwrap();
    assert!(state.consume_command(env_touch).is_some());

    // Accessibility state command admitted cleanly.
    let env_a11y = state
        .admit_command(BrowserCommand::SetAccessibilityState(
            CefFeatureState::Enabled,
        ))
        .unwrap();
    assert!(state.consume_command(env_a11y).is_some());

    // FinishOutboundDrag validates browser identity and works across Closing on reply lane.
    let foreign_finish = OutboundDragFinish {
        drag_id: DragSessionId {
            browser: browser(99),
            sequence: 1,
        },
        x: 0,
        y: 0,
        operation: DragOperationMask::NONE,
    };
    assert_eq!(
        state.admit_command(BrowserCommand::FinishOutboundDrag(foreign_finish)),
        Err(EngineError::WrongRuntime)
    );
    assert_eq!(
        state.admit_outbound_drag_finish(foreign_finish),
        Err(EngineError::WrongRuntime)
    );

    let owned_finish = OutboundDragFinish {
        drag_id: DragSessionId {
            browser: browser(5),
            sequence: 1,
        },
        x: 15,
        y: 25,
        operation: DragOperationMask::COPY,
    };
    state.close_requested();
    assert_eq!(
        state.admit_command(BrowserCommand::FinishOutboundDrag(owned_finish)),
        Err(EngineError::Closing)
    );
    let reply_env = state.admit_outbound_drag_finish(owned_finish).unwrap();
    assert!(state.consume_command(reply_env).is_some());
}

#[test]
fn native_child_hwnd_command_and_runtime_guards() {
    use rust_cef_core::contracts::{
        BrowserOptions, BrowserSurfaceMode, CreateError, MAX_NATIVE_BOUND, NativeChildHwndConfig,
        PixelRect,
    };
    use rust_cef_core::input::{BrowserTouchEvent, TouchEventType, TouchPointerType};
    use rust_cef_core::outbound_drag::{DragOperationMask, DragSessionId, OutboundDragFinish};
    use rust_cef_core::ports::{MouseClick, MouseModifiers};
    use std::num::{NonZeroU8, NonZeroUsize};

    // Offscreen session refuses NativeChildHwnd-specific commands and Offscreen fails when windowless=false.
    let mut osr = alive(10);
    assert_eq!(
        osr.admit_command(BrowserCommand::SetNativeBounds(PixelRect {
            x: 0,
            y: 0,
            width: 300,
            height: 200,
        })),
        Err(EngineError::InvalidRequest)
    );
    assert_eq!(
        osr.admit_command(BrowserCommand::NotifyParentMoved),
        Err(EngineError::InvalidRequest)
    );

    let offscreen_cfg = BrowserConfig {
        initial_url: "app://demo/index.html".into(),
        profile: None,
        render_preference: RenderPreference::SoftwareRequired,
        viewport: Viewport {
            width: 320,
            height: 240,
            scale: 1.0,
        },
        options: Default::default(),
    };
    assert!(matches!(
        Session::begin_create_for_runtime(browser(11), offscreen_cfg, false),
        Err(CreateError::InvalidOptions)
    ));

    // NativeChildHwnd session admits SetNativeBounds/NotifyParentMoved and shared commands,
    // rejects invalid PixelRect and OSR-only input/viewport commands.
    let hwnd_cfg = NativeChildHwndConfig {
        parent_hwnd: NonZeroUsize::new(0xABCD).unwrap(),
        initial_bounds: PixelRect {
            x: 0,
            y: 86,
            width: 960,
            height: 554,
        },
        activate_on_creation: false,
    };
    let mut native = Session::begin_create_for_runtime(
        browser(12),
        BrowserConfig {
            initial_url: "app://demo/index.html".into(),
            profile: None,
            render_preference: RenderPreference::D3d11Required,
            viewport: Viewport {
                width: 960,
                height: 554,
                scale: 1.0,
            },
            options: BrowserOptions::default()
                .with_surface_mode(BrowserSurfaceMode::NativeChildHwnd(hwnd_cfg)),
        },
        false,
    )
    .unwrap();
    assert!(native.surface_mode().is_native_child_hwnd());
    native.after_created();

    assert!(
        native
            .admit_command(BrowserCommand::SetNativeBounds(PixelRect {
                x: 0,
                y: 86,
                width: 1200,
                height: 700,
            }))
            .is_ok()
    );
    assert!(
        native
            .admit_command(BrowserCommand::NotifyParentMoved)
            .is_ok()
    );
    assert!(
        native
            .admit_command(BrowserCommand::SetVisible(false))
            .is_ok()
    );
    assert!(native.admit_command(BrowserCommand::SetFocus(true)).is_ok());
    assert!(
        native
            .admit_command(BrowserCommand::Navigate("app://demo/page2.html".into()))
            .is_ok()
    );

    assert_eq!(
        native.admit_command(BrowserCommand::SetNativeBounds(PixelRect {
            x: 0,
            y: 0,
            width: MAX_NATIVE_BOUND + 1,
            height: 200,
        })),
        Err(EngineError::InvalidRequest)
    );
    assert_eq!(
        native.admit_command(BrowserCommand::SetViewport(Viewport {
            width: 500,
            height: 400,
            scale: 1.0,
        })),
        Err(EngineError::InvalidRequest)
    );
    assert_eq!(
        native.admit_command(BrowserCommand::MouseClick(MouseClick {
            x: 10,
            y: 10,
            modifiers: MouseModifiers::default(),
            click_count: NonZeroU8::new(1).unwrap(),
        })),
        Err(EngineError::InvalidRequest)
    );
    assert_eq!(
        native.admit_command(BrowserCommand::TouchEvent(BrowserTouchEvent {
            id: 1,
            x: 10.0,
            y: 10.0,
            radius_x: 1.0,
            radius_y: 1.0,
            rotation_angle: 0.0,
            pressure: 0.5,
            event_type: TouchEventType::Pressed,
            modifiers: MouseModifiers::default(),
            pointer_type: TouchPointerType::Touch,
        })),
        Err(EngineError::InvalidRequest)
    );
    let drag_finish = OutboundDragFinish {
        drag_id: DragSessionId {
            browser: browser(12),
            sequence: 1,
        },
        x: 0,
        y: 0,
        operation: DragOperationMask::NONE,
    };
    assert_eq!(
        native.admit_command(BrowserCommand::FinishOutboundDrag(drag_finish)),
        Err(EngineError::InvalidRequest)
    );
    assert_eq!(
        native.admit_outbound_drag_finish(drag_finish),
        Err(EngineError::InvalidRequest)
    );
}
