use rust_cef_core::{
    contracts::{BrowserConfig, BrowserId, RenderPreference, RuntimeGeneration, Viewport},
    ports::BrowserEvent,
    session::{Session, SessionState},
};

fn session() -> Session {
    Session::begin_create(
        BrowserId::new(RuntimeGeneration::new(1), 1),
        BrowserConfig {
            initial_url: "http://127.0.0.1/a".into(),
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
fn navigation_before_created_does_not_fabricate_readiness_or_completion() {
    let mut session = session();
    assert!(
        session.apply_navigation_event(&BrowserEvent::LoadingChanged {
            loading: true,
            can_go_back: false,
            can_go_forward: false,
        })
    );
    assert!(
        session.apply_navigation_event(&BrowserEvent::AddressChanged("http://127.0.0.1/a".into()))
    );
    assert!(session.apply_navigation_event(&BrowserEvent::TitleChanged("A".into())));
    assert!(!session.snapshot().is_ready());
    session.after_created();
    assert!(session.snapshot().is_ready());
    assert!(session.snapshot().navigation.is_loading);
    session.apply_navigation_event(&BrowserEvent::LoadFinished {
        is_main_frame: true,
        http_status_code: 404,
    });
    let navigation = session.snapshot().navigation;
    assert_eq!(navigation.address, "http://127.0.0.1/a");
    assert_eq!(navigation.title, "A");
    assert_eq!(navigation.main_http_status, Some(404));
    assert_eq!(navigation.last_main_frame_error, None);
    assert!(
        navigation.is_loading,
        "one frame ending is not overall completion"
    );
    session.apply_navigation_event(&BrowserEvent::LoadingChanged {
        loading: false,
        can_go_back: true,
        can_go_forward: true,
    });
    let navigation = session.snapshot().navigation;
    assert!(!navigation.is_loading);
    assert!(navigation.can_go_back && navigation.can_go_forward);
    session.apply_navigation_event(&BrowserEvent::LoadFinished {
        is_main_frame: true,
        http_status_code: 0,
    });
    assert_eq!(
        session.snapshot().navigation.main_http_status,
        None,
        "no HTTP response is not HTTP 0"
    );
}

#[test]
fn frame_failures_cancellation_and_redirect_do_not_change_lifecycle() {
    let mut session = session();
    session.after_created();
    session.apply_navigation_event(&BrowserEvent::LoadFailed {
        is_main_frame: true,
        code: -324,
    });
    session.apply_navigation_event(&BrowserEvent::NavigationRequested {
        is_main_frame: true,
        is_redirect: true,
    });
    session.apply_navigation_event(&BrowserEvent::LoadStarted {
        is_main_frame: true,
    });
    session.apply_navigation_event(&BrowserEvent::LoadFinished {
        is_main_frame: true,
        http_status_code: 200,
    });
    assert_eq!(
        session.snapshot().navigation.last_main_frame_error,
        Some(-324)
    );
    assert_eq!(
        session.snapshot().navigation.main_http_status,
        None,
        "error document end cannot erase network error"
    );
    session.apply_navigation_event(&BrowserEvent::NavigationRequested {
        is_main_frame: true,
        is_redirect: false,
    });
    session.apply_navigation_event(&BrowserEvent::LoadFailed {
        is_main_frame: false,
        code: -101,
    });
    session.apply_navigation_event(&BrowserEvent::LoadFinished {
        is_main_frame: true,
        http_status_code: 200,
    });
    session.apply_navigation_event(&BrowserEvent::LoadCancelled {
        is_main_frame: true,
        code: -3,
    });
    let snapshot = session.snapshot();
    assert_eq!(snapshot.state, SessionState::Alive);
    assert_eq!(snapshot.navigation.last_main_frame_error, None);
    assert_eq!(snapshot.navigation.last_sub_frame_error, Some(-101));
    assert_eq!(snapshot.navigation.main_http_status, Some(200));
}

#[test]
fn previous_url_main_failure_and_status_cannot_overwrite_a_new_target() {
    let mut session = session();
    session.after_created();
    let request = BrowserEvent::NavigationRequested {
        is_main_frame: true,
        is_redirect: false,
    };
    session.apply_navigation_observation(&request, Some("http://127.0.0.1/a"));
    session.apply_navigation_observation(&request, Some("http://127.0.0.1/b"));
    assert!(!session.apply_navigation_observation(
        &BrowserEvent::LoadFailed {
            is_main_frame: true,
            code: -324
        },
        Some("http://127.0.0.1/a")
    ));
    assert!(!session.apply_navigation_observation(
        &BrowserEvent::LoadFinished {
            is_main_frame: true,
            http_status_code: 503
        },
        Some("http://127.0.0.1/a")
    ));
    assert!(!session.apply_navigation_observation(
        &BrowserEvent::LoadCancelled {
            is_main_frame: true,
            code: -3
        },
        Some("http://127.0.0.1/a"),
    ));
    assert_eq!(session.snapshot().navigation.last_main_frame_error, None);
    assert_eq!(session.snapshot().navigation.main_http_status, None);
    assert!(session.apply_navigation_observation(
        &BrowserEvent::LoadFinished {
            is_main_frame: true,
            http_status_code: 200
        },
        Some("http://127.0.0.1/b")
    ));
    assert_eq!(session.snapshot().navigation.main_http_status, Some(200));
}

#[test]
fn terminal_sessions_ignore_late_page_observations() {
    let mut session = session();
    session.after_created();
    session.apply_navigation_event(&BrowserEvent::TitleChanged("before".into()));
    session.closed();
    let before = session.snapshot();
    assert!(!session.apply_navigation_event(&BrowserEvent::TitleChanged("late".into())));
    assert!(
        !session.apply_navigation_event(&BrowserEvent::LoadingChanged {
            loading: true,
            can_go_back: true,
            can_go_forward: true,
        })
    );
    assert_eq!(session.snapshot(), before);
}
