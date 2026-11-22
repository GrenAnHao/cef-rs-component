use rust_cef_core::{
    contracts::{
        BrowserConfig, BrowserId, RenderPreference, RendererState, RuntimeGeneration, Viewport,
    },
    ports::{BrowserCommand, BrowserEngine, BrowserEvent, CommandEnvelope, EngineError},
    session::{Session, SessionState},
};

fn session() -> Session {
    let mut s = Session::begin_create(
        BrowserId::new(RuntimeGeneration::new(1), 1),
        BrowserConfig {
            initial_url: "http://127.0.0.1/a".into(),
            profile: None,
            render_preference: RenderPreference::SoftwareRequired,
            viewport: Viewport {
                width: 320,
                height: 240,
                scale: 1.,
            },
            options: Default::default(),
        },
    )
    .unwrap();
    s.after_created();
    s
}

#[derive(Default)]
struct Engine(Vec<BrowserCommand>);
impl BrowserEngine for Engine {
    fn execute(&mut self, command: CommandEnvelope) -> Result<(), EngineError> {
        self.0.push(command.command);
        Ok(())
    }
}

#[test]
fn renderer_fault_is_not_closed_or_an_automatic_recovery() {
    let mut s = session();
    let mut engine = Engine::default();
    s.apply_renderer_event(&BrowserEvent::RenderViewReady);
    let event = BrowserEvent::RendererTerminated {
        native_status: 701,
        code: Some(-987),
    };
    assert!(s.apply_renderer_event(&event));
    let snapshot = s.snapshot();
    assert_eq!(snapshot.state, SessionState::Alive);
    assert!(
        snapshot.is_ready(),
        "readiness is native Browser lifetime, not healthy content"
    );
    assert_eq!(snapshot.renderer.state, RendererState::Terminated);
    assert_eq!(
        snapshot.renderer.last_termination.unwrap().native_status,
        701
    );
    assert!(engine.0.is_empty());
    let accepted = s.admit_command(BrowserCommand::Reload).unwrap();
    assert_eq!(
        s.snapshot().renderer.state,
        RendererState::Terminated,
        "admission is not recovery"
    );
    s.consume_command(accepted)
        .unwrap()
        .execute(&mut engine)
        .unwrap();
    assert!(matches!(engine.0.as_slice(), [BrowserCommand::Reload]));
    assert_eq!(
        s.snapshot().renderer.state,
        RendererState::Terminated,
        "dispatch is not IPC readiness"
    );
    assert!(s.apply_renderer_event(&BrowserEvent::RenderViewReady));
    assert_eq!(s.snapshot().renderer.state, RendererState::Running);
    assert_eq!(
        s.snapshot().renderer.last_termination.unwrap().code,
        Some(-987)
    );
}

#[test]
fn response_and_navigation_never_erase_a_terminated_renderer() {
    let mut s = session();
    assert!(s.apply_renderer_event(&BrowserEvent::Unresponsive));
    assert_eq!(s.snapshot().renderer.state, RendererState::Unresponsive);
    assert!(s.apply_renderer_event(&BrowserEvent::Responsive));
    assert_eq!(s.snapshot().renderer.state, RendererState::Running);
    s.apply_renderer_event(&BrowserEvent::RendererTerminated {
        native_status: 2,
        code: Some(1),
    });
    assert!(!s.apply_renderer_event(&BrowserEvent::Responsive));
    assert!(!s.apply_renderer_event(&BrowserEvent::Unresponsive));
    assert!(s.apply_navigation_event(&BrowserEvent::LoadStarted {
        is_main_frame: true
    }));
    assert_eq!(s.snapshot().renderer.state, RendererState::Terminated);
}

#[test]
fn static_page_navigation_and_no_frame_observation_are_not_hang_signals() {
    let mut s = session();
    s.apply_renderer_event(&BrowserEvent::RenderViewReady);
    for event in [
        BrowserEvent::LoadingChanged {
            loading: false,
            can_go_back: false,
            can_go_forward: false,
        },
        BrowserEvent::TitleChanged("static".into()),
    ] {
        s.apply_navigation_event(&event);
    }
    // No frame timer or capture cadence enters this reducer at all.
    assert_eq!(s.snapshot().renderer.state, RendererState::Running);
    s.closed();
    assert!(!s.apply_renderer_event(&BrowserEvent::Unresponsive));
    assert!(!s.apply_renderer_event(&BrowserEvent::RenderViewReady));
    assert_eq!(s.snapshot().state, SessionState::Closed);
}
