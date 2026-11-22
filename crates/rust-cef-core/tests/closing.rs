use rust_cef_core::{contracts::*, session::Session};

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
fn late_created_session_preserves_close_intent() {
    let mut session = creating();
    session.close_requested();
    session.close_requested();
    assert_eq!(session.snapshot().state, SessionState::Closing);
    session.after_created();
    assert_eq!(session.snapshot().state, SessionState::Closing);
    assert!(session.snapshot().creation_accepted);
    assert!(!session.snapshot().is_ready());
    session.closed();
    session.closed();
    session.close_requested();
    assert_eq!(session.snapshot().state, SessionState::Closed);
}
