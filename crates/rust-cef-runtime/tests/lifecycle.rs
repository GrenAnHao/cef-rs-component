//! Pure lifecycle RED/GREEN contracts, not native thread evidence.
use rust_cef_core::{
    contracts::{
        BrowserConfig, BrowserId, InitializeOutcome, RenderPreference, RuntimeGeneration,
        RuntimeState, SessionState, Viewport,
    },
    runtime::StartupState,
    session::Session,
};

#[test]
fn context_ready_before_start_returns() {
    let mut startup = StartupState::new();
    startup.context_initialized(42);
    assert_eq!(startup.state(), RuntimeState::Starting);
    startup.initialize_returned(InitializeOutcome::Accepted);
    assert_eq!(startup.state(), RuntimeState::Ready);
    assert_eq!(startup.ui_thread_id(), Some(42));
    let mut late = StartupState::new();
    late.initialize_returned(InitializeOutcome::Accepted);
    assert_eq!(late.state(), RuntimeState::Starting);
    late.context_initialized(43);
    assert_eq!(late.state(), RuntimeState::Ready);
}

fn creating() -> Session {
    Session::begin_create(
        BrowserId::new(RuntimeGeneration::new(1), 1),
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

#[test]
fn create_request_accepted_not_alive() {
    let mut session = creating();
    session.accept_create_request();
    assert_eq!(session.snapshot().state, SessionState::Creating);
    assert!(!session.snapshot().is_ready());
    session.after_created();
    assert_eq!(session.snapshot().state, SessionState::Alive);
}

#[test]
fn async_create_rejected() {
    let mut session = creating();
    session.create_rejected();
    assert_eq!(session.snapshot().state, SessionState::CreateFailed);
    assert!(!session.snapshot().creation_accepted);
    assert!(!session.snapshot().is_ready());
}
