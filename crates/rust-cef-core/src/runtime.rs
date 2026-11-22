//! Pure lifecycle decisions; native ownership and calls live in the adapter.
use crate::contracts::{BrowserId, BrowserSnapshot, SessionState};
pub use crate::contracts::{InitializeOutcome, RuntimeState, ThreadMode};
use std::time::Instant;

/// Both initialize's return and ContextReady are required, in either order.
#[derive(Default)]
pub struct StartupState {
    initialized: Option<InitializeOutcome>,
    ui_thread_id: Option<u64>,
}
impl StartupState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn context_initialized(&mut self, thread_id: u64) {
        self.ui_thread_id = Some(thread_id);
    }

    pub fn initialize_returned(&mut self, outcome: InitializeOutcome) {
        self.initialized = Some(outcome);
    }

    pub fn ui_thread_id(&self) -> Option<u64> {
        self.ui_thread_id
    }

    pub fn state(&self) -> RuntimeState {
        match self.initialized {
            Some(InitializeOutcome::ProcessExitRequired { .. }) => {
                RuntimeState::ProcessExitRequired
            }
            Some(InitializeOutcome::Accepted) if self.ui_thread_id.is_some() => RuntimeState::Ready,
            _ => RuntimeState::Starting,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShutdownDecision {
    NotRequested,
    Draining {
        open_browsers: Vec<BrowserId>,
        pending_tasks: u64,
        pending_requests: u64,
    },
    Blocked {
        open_browsers: Vec<BrowserId>,
        pending_tasks: u64,
        pending_requests: u64,
    },
    Ready,
    Stopped,
    ProcessExitRequired {
        exit_code: Option<i32>,
    },
}

/// Policy over actual adapter observations, not a second native registry.
/// A deadline is diagnostic: it never authorizes teardown of unfinished work.
pub struct ShutdownBarrier {
    outcome: InitializeOutcome,
    deadline: Option<Instant>,
    state: RuntimeState,
    ready: bool,
}
impl ShutdownBarrier {
    pub fn new(outcome: InitializeOutcome) -> Self {
        let state = match outcome {
            InitializeOutcome::Accepted => RuntimeState::Ready,
            InitializeOutcome::ProcessExitRequired { .. } => RuntimeState::ProcessExitRequired,
        };
        Self {
            outcome,
            deadline: None,
            state,
            ready: false,
        }
    }
    pub fn request(&mut self, deadline: Instant) -> Result<(), &'static str> {
        if matches!(
            self.state,
            RuntimeState::Stopped | RuntimeState::ProcessExitRequired
        ) {
            return Err("runtime cannot begin shutdown");
        }
        self.deadline.get_or_insert(deadline);
        self.ready = false;
        self.state = RuntimeState::Draining;
        Ok(())
    }
    pub fn accepts_work(&self) -> bool {
        self.deadline.is_none() && self.state == RuntimeState::Ready
    }
    pub fn state(&self) -> RuntimeState {
        self.state
    }
    pub fn may_finish(&self) -> bool {
        self.ready && self.state != RuntimeState::Stopped
    }
    pub fn observe(
        &mut self,
        browsers: &[BrowserSnapshot],
        pending_tasks: u64,
        pending_requests: u64,
        now: Instant,
    ) -> ShutdownDecision {
        self.ready = false;
        if let InitializeOutcome::ProcessExitRequired { exit_code } = self.outcome {
            return ShutdownDecision::ProcessExitRequired { exit_code };
        }
        if self.state == RuntimeState::Stopped {
            return ShutdownDecision::Stopped;
        }
        let Some(deadline) = self.deadline else {
            return ShutdownDecision::NotRequested;
        };
        let open_browsers: Vec<_> = browsers
            .iter()
            .filter(|browser| {
                !matches!(
                    browser.state,
                    SessionState::Closed | SessionState::CreateFailed
                )
            })
            .map(|browser| browser.browser)
            .collect();
        if open_browsers.is_empty() && pending_tasks == 0 && pending_requests == 0 {
            self.ready = true;
            self.state = RuntimeState::Draining;
            return ShutdownDecision::Ready;
        }
        if now >= deadline {
            self.state = RuntimeState::ShutdownBlocked;
            ShutdownDecision::Blocked {
                open_browsers,
                pending_tasks,
                pending_requests,
            }
        } else {
            self.state = RuntimeState::Draining;
            ShutdownDecision::Draining {
                open_browsers,
                pending_tasks,
                pending_requests,
            }
        }
    }
    /// The adapter must still seal admission and release its native references.
    pub fn finish(&mut self) -> Result<(), &'static str> {
        if !self.may_finish() {
            return Err("shutdown barrier is not ready");
        }
        self.state = RuntimeState::Stopped;
        self.ready = false;
        Ok(())
    }
}
