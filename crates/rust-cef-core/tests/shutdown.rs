//! Pure shutdown policy contracts. Native lifetime evidence is separate.
use rust_cef_core::{
    contracts::*,
    runtime::{ShutdownBarrier, ShutdownDecision},
    session::Session,
};
use std::time::{Duration, Instant};

fn session(local: u64) -> Session {
    Session::begin_create(
        BrowserId::new(RuntimeGeneration::new(1), local),
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
fn barrier() -> ShutdownBarrier {
    ShutdownBarrier::new(InitializeOutcome::Accepted)
}

#[test]
fn late_create_after_close_is_drained() {
    let now = Instant::now();
    let mut barrier = barrier();
    let mut browser = session(1);
    barrier.request(now + Duration::from_secs(1)).unwrap();
    browser.close_requested();
    browser.after_created();
    assert_eq!(browser.snapshot().state, SessionState::Closing);
    assert!(matches!(
        barrier.observe(&[browser.snapshot()], 1, 0, now),
        ShutdownDecision::Draining {
            pending_tasks: 1,
            ..
        }
    ));
    browser.closed();
    assert_eq!(
        barrier.observe(&[browser.snapshot()], 0, 0, now),
        ShutdownDecision::Ready
    );
}

#[test]
fn all_browsers_must_close() {
    let now = Instant::now();
    let mut barrier = barrier();
    barrier.request(now + Duration::from_secs(1)).unwrap();
    let mut first = session(1);
    let mut second = session(2);
    first.closed();
    let observed = barrier.observe(&[first.snapshot(), second.snapshot()], 0, 0, now);
    assert!(
        matches!(observed, ShutdownDecision::Draining { ref open_browsers, .. } if open_browsers == &[second.snapshot().browser])
    );
    second.closed();
    assert!(matches!(
        barrier.observe(&[first.snapshot(), second.snapshot()], 1, 0, now),
        ShutdownDecision::Draining {
            pending_tasks: 1,
            ..
        }
    ));
    assert!(!barrier.may_finish());
    assert!(barrier.finish().is_err());
    assert!(matches!(
        barrier.observe(&[first.snapshot(), second.snapshot()], 0, 1, now),
        ShutdownDecision::Draining {
            pending_requests: 1,
            ..
        }
    ));
    assert_eq!(
        barrier.observe(&[first.snapshot(), second.snapshot()], 0, 0, now),
        ShutdownDecision::Ready
    );
}

#[test]
fn timeout_never_shutdowns() {
    let now = Instant::now();
    let mut barrier = barrier();
    barrier.request(now).unwrap();
    let browser = session(1);
    let decision = barrier.observe(&[browser.snapshot()], 2, 1, now);
    assert!(matches!(
        decision,
        ShutdownDecision::Blocked {
            pending_tasks: 2,
            pending_requests: 1,
            ..
        }
    ));
    assert!(!barrier.may_finish());
    assert!(barrier.finish().is_err());
    assert_eq!(barrier.state(), RuntimeState::ShutdownBlocked);
    // Expiry is diagnostic, not cancellation; the host may continue waiting.
    assert_eq!(
        barrier.observe(&[], 0, 0, now + Duration::from_secs(1)),
        ShutdownDecision::Ready
    );
    barrier.finish().unwrap();
    assert_eq!(barrier.state(), RuntimeState::Stopped);
}

#[test]
fn initialize_false_requires_exit() {
    let mut barrier = ShutdownBarrier::new(InitializeOutcome::ProcessExitRequired {
        exit_code: Some(37),
    });
    assert!(!barrier.accepts_work());
    assert!(barrier.request(Instant::now()).is_err());
    assert_eq!(
        barrier.observe(&[], 0, 0, Instant::now()),
        ShutdownDecision::ProcessExitRequired {
            exit_code: Some(37)
        }
    );
    assert!(barrier.finish().is_err());
    assert_eq!(barrier.state(), RuntimeState::ProcessExitRequired);
}

#[test]
fn post_after_drain_rejected() {
    let mut barrier = barrier();
    assert!(barrier.accepts_work());
    barrier
        .request(Instant::now() + Duration::from_secs(1))
        .unwrap();
    assert!(!barrier.accepts_work());
    assert_eq!(barrier.state(), RuntimeState::Draining);
    assert_eq!(
        barrier.observe(&[], 0, 0, Instant::now()),
        ShutdownDecision::Ready
    );
    barrier.finish().unwrap();
    assert!(!barrier.accepts_work());
}
