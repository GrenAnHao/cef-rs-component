use rust_cef_core::{
    contracts::{
        BrowserConfig, BrowserId, ProfileConfig, ProfileStorage, RenderPreference,
        RuntimeGeneration, Viewport,
    },
    ports::{BrowserCommand, CloseResult, CloseTicket, EngineError, RequestId},
    profiles::{ProfileCatalog, ProfileState},
    session::{Session, SessionState},
};

fn session() -> Session {
    let mut session = Session::begin_create(
        BrowserId::new(RuntimeGeneration::new(1), 1),
        BrowserConfig {
            initial_url: "data:text/html,host".into(),
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
    .unwrap();
    session.after_created();
    session
}

#[test]
fn dialog_reply_has_independent_capacity_but_unique_command_sequence() {
    let mut session = session();
    for _ in 0..256 {
        session.admit_command(BrowserCommand::Reload).unwrap();
    }
    session.close_requested();
    let request = RequestId {
        browser: session.snapshot().browser,
        sequence: 12,
    };
    let reply = session.admit_dialog_response(request, false).unwrap();
    assert_eq!(reply.id.sequence, 257);
    assert_eq!(session.pending_commands(), 256);
    assert_eq!(
        session.admit_command(BrowserCommand::Reload),
        Err(EngineError::Closing)
    );
    assert!(session.consume_command(reply.clone()).is_some());
    assert!(session.consume_command(reply).is_none());
    let foreign = RequestId {
        browser: BrowserId::new(RuntimeGeneration::new(9), 1),
        sequence: 12,
    };
    assert_eq!(
        session.admit_dialog_response(foreign, false),
        Err(EngineError::WrongRuntime)
    );
}

#[test]
fn cancelled_close_ticket_retains_its_result_after_a_new_force_attempt() {
    let mut session = session();
    let (cancelled, completion) = CloseTicket::pending(session.snapshot().browser, 1);
    session.close_requested();
    session.close_cancelled();
    completion.complete(CloseResult::Cancelled);
    assert_eq!(session.snapshot().state, SessionState::Alive);
    let (force, force_completion) = CloseTicket::pending(session.snapshot().browser, 2);
    session.close_requested();
    session.closed();
    force_completion.complete(CloseResult::Closed);
    completion.complete(CloseResult::Closed);
    assert_eq!(cancelled.result(), CloseResult::Cancelled);
    assert_eq!(force.result(), CloseResult::Closed);
    session.close_cancelled();
    assert_eq!(session.snapshot().state, SessionState::Closed);
}

#[test]
fn global_and_incognito_native_readiness_wait_for_policy_preparation() {
    for storage in [
        ProfileStorage::Memory,
        ProfileStorage::Persistent {
            directory: "root".into(),
        },
    ] {
        let memory = storage == ProfileStorage::Memory;
        let mut profiles = ProfileCatalog::new(RuntimeGeneration::new(1), storage);
        profiles.require_preparation().unwrap();
        let default = profiles.default_handle();
        if memory {
            profiles.context_initialized(default).unwrap();
            profiles.create_returned(default, true).unwrap();
        } else {
            profiles.default_context_ready().unwrap();
        }
        assert_eq!(profiles.native_initialized(default), Ok(true));
        assert_eq!(profiles.state(default), Ok(ProfileState::Pending));
        assert!(profiles.select(None).is_err());
        profiles.preparation_complete(default, true).unwrap();
        assert_eq!(profiles.state(default), Ok(ProfileState::Ready));
        assert!(profiles.require_preparation().is_err());
    }
}

#[test]
fn preparation_failure_is_terminal_and_new_profiles_inherit_the_gate() {
    let mut profiles = ProfileCatalog::new(RuntimeGeneration::new(1), ProfileStorage::Memory);
    profiles.require_preparation().unwrap();
    let named = profiles
        .reserve(ProfileConfig {
            name: "p1".into(),
            storage: ProfileStorage::Memory,
        })
        .unwrap()
        .handle;
    profiles.preparation_complete(named, true).unwrap();
    assert_eq!(profiles.state(named), Ok(ProfileState::Pending));
    profiles.create_returned(named, true).unwrap();
    profiles.context_initialized(named).unwrap();
    assert_eq!(profiles.state(named), Ok(ProfileState::Ready));
    profiles.preparation_complete(named, false).unwrap();
    assert!(profiles.preparation_complete(named, true).is_err());
    profiles.context_initialized(named).unwrap();
    assert_eq!(profiles.state(named), Ok(ProfileState::Failed));
}
