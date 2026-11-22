//! Asynchronous cookie-store flushes. All native objects are UI-task locals;
//! tickets and the shutdown barrier retain only plain state.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, Weak},
};

use cef::*;
use rust_cef_core::{contracts::ProfileHandle, ports::UiDispatcher};

use crate::{
    executor::{Executor, NativeAudit, TaskPayload},
    profiles::{ProfileCreateError, ProfileManager},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CookieFlushStatus {
    Pending,
    Complete,
    Failed(ProfileCreateError),
}

/// Owns only the result, independently of the registry and native objects.
pub struct CookieFlushTicket {
    state: Arc<Mutex<CookieFlushStatus>>,
}

impl CookieFlushTicket {
    pub fn poll(&self) -> CookieFlushStatus {
        lock(&self.state).clone()
    }
}

impl std::fmt::Debug for CookieFlushTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CookieFlushTicket")
            .field("status", &self.poll())
            .finish()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Begin,
    Initializing,
    Flushing,
    Accepted,
}

#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CookieFlushMarkerProbe {
    pub profile: ProfileHandle,
    pub sequence: u64,
    /// Nonzero only if the original CEF completion callback was observed on UI.
    pub callback_thread: u64,
    pub release_reserved: bool,
    pub faulted: bool,
}

#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Default)]
enum MarkerProbe {
    #[default]
    Off,
    Armed(ProfileHandle),
    Tracking(CookieFlushMarkerProbe),
}

struct Record {
    handle: ProfileHandle,
    phase: Phase,
    state: Arc<Mutex<CookieFlushStatus>>,
}

#[derive(Default)]
struct Records {
    closed: bool,
    next: u64,
    pending: BTreeMap<u64, Record>,
    accepted: u64,
    completed: u64,
    #[cfg(feature = "test-support")]
    marker_probe: MarkerProbe,
}

impl Records {
    fn insert(
        &mut self,
        handle: ProfileHandle,
    ) -> Result<(u64, CookieFlushTicket), ProfileCreateError> {
        if self.closed {
            return Err(ProfileCreateError::Closed);
        }
        self.next = self.next.checked_add(1).ok_or(ProfileCreateError::Profile(
            rust_cef_core::profiles::ProfileError::IdExhausted,
        ))?;
        let state = Arc::new(Mutex::new(CookieFlushStatus::Pending));
        self.pending.insert(
            self.next,
            Record {
                handle,
                phase: Phase::Begin,
                state: state.clone(),
            },
        );
        #[cfg(feature = "test-support")]
        if matches!(self.marker_probe, MarkerProbe::Armed(profile) if profile == handle) {
            self.marker_probe = MarkerProbe::Tracking(CookieFlushMarkerProbe {
                profile: handle,
                sequence: self.next,
                callback_thread: 0,
                release_reserved: false,
                faulted: false,
            });
        }
        Ok((self.next, CookieFlushTicket { state }))
    }

    fn advance(&mut self, sequence: u64, handle: ProfileHandle, from: Phase, to: Phase) -> bool {
        let Some(record) = self.pending.get_mut(&sequence) else {
            return false;
        };
        if record.handle != handle || record.phase != from {
            return false;
        }
        record.phase = to;
        true
    }

    fn fail(&mut self, sequence: u64, error: ProfileCreateError) -> bool {
        let Some(record) = self.pending.remove(&sequence) else {
            return false;
        };
        *lock(&record.state) = CookieFlushStatus::Failed(error);
        true
    }

    fn complete(&mut self, sequence: u64) -> bool {
        if !matches!(self.pending.get(&sequence), Some(record) if record.phase == Phase::Accepted) {
            return false;
        }
        let record = self
            .pending
            .remove(&sequence)
            .expect("record checked above");
        *lock(&record.state) = CookieFlushStatus::Complete;
        self.completed += 1;
        true
    }
}

pub(crate) struct CookieFlushRegistry {
    profiles: Weak<ProfileManager>,
    executor: Weak<Executor>,
    ui: Arc<dyn UiDispatcher>,
    audit: Arc<NativeAudit>,
    records: Mutex<Records>,
}

impl CookieFlushRegistry {
    pub(crate) fn new(
        profiles: &Arc<ProfileManager>,
        executor: &Arc<Executor>,
        ui: Arc<dyn UiDispatcher>,
        audit: Arc<NativeAudit>,
    ) -> Arc<Self> {
        Arc::new(Self {
            profiles: Arc::downgrade(profiles),
            executor: Arc::downgrade(executor),
            ui,
            audit,
            records: Mutex::new(Records::default()),
        })
    }

    pub(crate) fn request(
        self: &Arc<Self>,
        handle: ProfileHandle,
    ) -> Result<CookieFlushTicket, ProfileCreateError> {
        let profiles = self.profiles.upgrade().ok_or(ProfileCreateError::Closed)?;
        profiles.select(Some(handle))?;
        let (sequence, ticket) = lock(&self.records).insert(handle)?;
        let result = self
            .executor
            .upgrade()
            .ok_or(ProfileCreateError::Closed)
            .and_then(|executor| {
                executor
                    .post(TaskPayload::BeginCookieFlush { sequence, handle })
                    .map(|_| ())
                    .map_err(ProfileCreateError::PostFailed)
            });
        if let Err(error) = result {
            self.fail(sequence, error.clone());
            return Err(error);
        }
        Ok(ticket)
    }

    // Called only on the CEF UI sequence. Callback posts a marker, never a
    // completion transition while CEF still borrows its wrapper.
    pub(crate) fn begin_now(self: &Arc<Self>, sequence: u64, handle: ProfileHandle) {
        if !lock(&self.records).advance(sequence, handle, Phase::Begin, Phase::Initializing) {
            return;
        }
        let Some(profiles) = self.profiles.upgrade() else {
            self.fail(sequence, ProfileCreateError::Closed);
            return;
        };
        let Some(context) = profiles.context_for(handle) else {
            self.fail(sequence, ProfileCreateError::NotReady);
            return;
        };
        let mut callback = CookieManagerInitialized::new(sequence, handle, Arc::downgrade(self));
        self.audit.record();
        let manager = context.cookie_manager(Some(&mut callback));
        let ok = manager.is_some();
        drop(manager);
        drop(callback);
        drop(context);
        if !ok {
            self.fail(sequence, ProfileCreateError::CookieStoreUnavailable);
        }
    }

    pub(crate) fn manager_ready(self: &Arc<Self>, sequence: u64, handle: ProfileHandle) {
        if !lock(&self.records).advance(sequence, handle, Phase::Initializing, Phase::Flushing) {
            return;
        }
        let Some(profiles) = self.profiles.upgrade() else {
            self.fail(sequence, ProfileCreateError::Closed);
            return;
        };
        let Some(context) = profiles.context_for(handle) else {
            self.fail(sequence, ProfileCreateError::NotReady);
            return;
        };
        self.audit.record();
        let manager = context.cookie_manager(None);
        let Some(manager) = manager else {
            drop(context);
            self.fail(sequence, ProfileCreateError::CookieStoreUnavailable);
            return;
        };
        let mut callback = CookieFlushDone::new(sequence, Arc::downgrade(self));
        self.audit.record();
        let accepted = manager.flush_store(Some(&mut callback)) == 1;
        drop(callback);
        drop(manager);
        drop(context);
        if accepted {
            let mut records = lock(&self.records);
            if records.advance(sequence, handle, Phase::Flushing, Phase::Accepted) {
                records.accepted += 1;
            }
        } else {
            self.fail(sequence, ProfileCreateError::CookieStoreUnavailable);
        }
    }

    pub(crate) fn completed(&self, sequence: u64) {
        let completed = lock(&self.records).complete(sequence);
        if completed {
            let _ = self.ui.wake();
        }
    }

    pub(crate) fn pending(&self) -> u64 {
        lock(&self.records).pending.len() as u64
    }
    pub(crate) fn accepted(&self) -> u64 {
        lock(&self.records).accepted
    }
    pub(crate) fn completed_count(&self) -> u64 {
        lock(&self.records).completed
    }

    // Caller guarantees pending == 0. Never manufactures a completion.
    pub(crate) fn close(&self) {
        lock(&self.records).closed = true;
    }

    fn fail(&self, sequence: u64, error: ProfileCreateError) {
        let failed = lock(&self.records).fail(sequence, error);
        if failed {
            let _ = self.ui.wake();
        }
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn probe_arm_marker(
        &self,
        profile: ProfileHandle,
    ) -> Result<(), rust_cef_core::ports::EngineError> {
        let mut records = lock(&self.records);
        if records.closed {
            return Err(rust_cef_core::ports::EngineError::Closed);
        }
        if !matches!(records.marker_probe, MarkerProbe::Off) || !records.pending.is_empty() {
            return Err(rust_cef_core::ports::EngineError::InvalidRequest);
        }
        records.marker_probe = MarkerProbe::Armed(profile);
        Ok(())
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn probe_marker(&self) -> Option<CookieFlushMarkerProbe> {
        match lock(&self.records).marker_probe {
            MarkerProbe::Tracking(probe) => Some(probe),
            _ => None,
        }
    }

    /// Samples the precise held marker while the registry is admitting Close.
    /// Owned ledger data only; no native invocation or transition here.
    #[cfg(feature = "test-support")]
    pub(crate) fn probe_marker_pending_at_close(
        &self,
        profile: ProfileHandle,
        sequence: u64,
        ui_thread: u64,
    ) -> bool {
        let records = lock(&self.records);
        ui_thread != 0
            && records.pending.len() == 1
            && records.accepted == 1
            && records.completed == 0
            && records
                .pending
                .get(&sequence)
                .is_some_and(|r| r.handle == profile && r.phase == Phase::Accepted)
            && matches!(records.marker_probe, MarkerProbe::Tracking(probe)
                if probe.profile == profile && probe.sequence == sequence && probe.callback_thread == ui_thread
                    && !probe.release_reserved && !probe.faulted)
    }

    #[cfg(feature = "test-support")]
    fn hold_original_marker(&self, sequence: u64) -> bool {
        let Some(target) = self.probe_marker().filter(|p| p.sequence == sequence) else {
            return false;
        };
        // The fixed SDK promises UI for this completion. Qualify the observed
        // invocation outside the ledger lock; wrong-thread evidence fails closed.
        let thread = crate::executor::current_thread_id();
        let on_ui = cef::currently_on(cef::ThreadId::UI) != 0;
        {
            let mut records = lock(&self.records);
            let eligible = records.pending.get(&sequence).is_some_and(|r| {
                r.handle == target.profile && matches!(r.phase, Phase::Flushing | Phase::Accepted)
            });
            let MarkerProbe::Tracking(probe) = &mut records.marker_probe else {
                return false;
            };
            if !eligible
                || !on_ui
                || thread == 0
                || probe.callback_thread != 0
                || probe.release_reserved
            {
                probe.faulted = true;
            } else {
                probe.callback_thread = thread;
            }
        }
        let _ = self.ui.wake();
        true
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn probe_release_marker(
        &self,
        profile: ProfileHandle,
        sequence: u64,
    ) -> Result<rust_cef_core::ports::TaskId, rust_cef_core::ports::EngineError> {
        use rust_cef_core::ports::EngineError;
        let executor = self.executor.upgrade().ok_or(EngineError::Closed)?;
        {
            let mut records = lock(&self.records);
            if !records
                .pending
                .get(&sequence)
                .is_some_and(|r| r.handle == profile && r.phase == Phase::Accepted)
            {
                return Err(EngineError::InvalidRequest);
            }
            let MarkerProbe::Tracking(probe) = &mut records.marker_probe else {
                return Err(EngineError::InvalidRequest);
            };
            if probe.profile != profile
                || probe.sequence != sequence
                || probe.callback_thread == 0
                || probe.faulted
                || probe.release_reserved
            {
                return Err(EngineError::InvalidRequest);
            }
            probe.release_reserved = true;
        }
        // The original native callback was observed, but only the real UI
        // completion task may consume the record and publish its result.
        // An explicit rejected post leaves the reservation and gate intact.
        executor.post_while_draining(TaskPayload::CookieFlushCompleted { sequence })
    }

    fn completion_callback(&self, sequence: u64) {
        #[cfg(feature = "test-support")]
        if self.hold_original_marker(sequence) {
            return;
        }
        self.post_marker(TaskPayload::CookieFlushCompleted { sequence });
    }

    fn post_marker(&self, payload: TaskPayload) {
        let posted = self
            .executor
            .upgrade()
            .is_some_and(|executor| executor.post_while_draining(payload).is_ok());
        if !posted {
            let _ = self.ui.wake();
        }
        // Rejection leaves pending intact, deliberately blocking shutdown.
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

wrap_completion_callback! {
    struct CookieManagerInitialized {
        sequence: u64,
        handle: ProfileHandle,
        registry: Weak<CookieFlushRegistry>,
    }
    impl CompletionCallback {
        fn on_complete(&self) {
            if let Some(registry) = self.registry.upgrade() {
                registry.post_marker(TaskPayload::CookieManagerReady { sequence: self.sequence, handle: self.handle });
            }
        }
    }
}

wrap_completion_callback! {
    struct CookieFlushDone {
        sequence: u64,
        registry: Weak<CookieFlushRegistry>,
    }
    impl CompletionCallback {
        fn on_complete(&self) {
            if let Some(registry) = self.registry.upgrade() {
                registry.completion_callback(self.sequence);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::contracts::RuntimeGeneration;
    use rust_cef_core::ports::EngineError;

    fn handle() -> ProfileHandle {
        ProfileHandle::new(RuntimeGeneration::new(1), 0)
    }

    #[test]
    fn dropped_pending_ticket_does_not_remove_barrier() {
        let mut records = Records::default();
        let (_, ticket) = records.insert(handle()).unwrap();
        drop(ticket);
        assert_eq!(records.pending.len(), 1);
    }

    #[test]
    fn rejected_post_retires_pending_with_failure() {
        let mut records = Records::default();
        let (id, ticket) = records.insert(handle()).unwrap();
        let error = ProfileCreateError::PostFailed(EngineError::PostRejected);
        assert!(records.fail(id, error.clone()));
        assert_eq!(ticket.poll(), CookieFlushStatus::Failed(error));
        assert!(records.pending.is_empty());
    }

    #[test]
    fn completion_is_once_and_never_overwrites_failure() {
        let mut records = Records::default();
        let (id, ticket) = records.insert(handle()).unwrap();
        assert!(!records.complete(id));
        assert!(records.advance(id, handle(), Phase::Begin, Phase::Accepted));
        assert!(records.complete(id));
        assert!(!records.complete(id));
        assert_eq!(records.completed, 1);
        assert_eq!(ticket.poll(), CookieFlushStatus::Complete);
        let (id, failed) = records.insert(handle()).unwrap();
        assert!(records.fail(id, ProfileCreateError::NotReady));
        assert!(!records.complete(id));
        assert_eq!(
            failed.poll(),
            CookieFlushStatus::Failed(ProfileCreateError::NotReady)
        );
    }

    #[test]
    fn close_never_completes_pending_records() {
        let mut records = Records::default();
        let (_, ticket) = records.insert(handle()).unwrap();
        records.closed = true;
        assert_eq!(ticket.poll(), CookieFlushStatus::Pending);
        assert_eq!(records.pending.len(), 1);
        assert!(matches!(
            records.insert(handle()),
            Err(ProfileCreateError::Closed)
        ));
    }
}
