//! Native profile ownership: one `ProfileHandle` maps to exactly one
//! `CefRequestContext`, and the core catalog is the only readiness authority.
//!
//! Readiness invariants:
//! - Persistent defaults acquire the handlerless global on CEF UI. Memory
//!   defaults use a separately created, shared incognito context and wait for
//!   the same factory/handler merge used by explicit profiles.
//! - A named profile is created only with the explicit
//!   `request_context_create_context(Some(&settings), Some(&mut handler))`
//!   call (never the shared-context factory) and becomes Ready only after both
//!   the create return and its own `on_request_context_initialized` arrived.
//!   Either order is merged by the core catalog; a failed create stays Failed.
//! - No fallback exists: an unknown, foreign or pending handle is rejected and
//!   a browser is never silently bound to the default context.
//!
//! Native-reference invariants (G1 stays closed):
//! - Every durable CEF reference lives in `slots` and is counted by
//!   `references_retained`. Post payloads hold only plain data plus weak
//!   references, so a rejected post drops nothing native.
//! - All references are released on the owner thread before the single
//!   `cef::shutdown`, and only after the executor is sealed and no callback is
//!   in flight (`callbacks_in_flight`). There is no separate UI retirement task,
//!   so a seal can never beat a retirement post.
//! - Dropping this manager without a completed shutdown abandons (leaks) its
//!   CEF references instead of running Release while CEF is still live.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use cef::*;
use rust_cef_core::{
    contracts::{ProfileConfig, ProfileHandle, ProfileStorage, RuntimeGeneration},
    custom_schemes::{
        CustomSchemeValidationError, ProfileSchemeBinding, validate_profile_scheme_binding,
    },
    ports::{EngineError, UiDispatcher},
    profiles::{ProfileCatalog, ProfileError, ProfileRegistration, ProfileState},
};

use crate::{
    executor::{NativeAudit, TaskPayload},
    policy_native::NativePolicy,
    profile_paths::{ProfilePaths, StoragePathError},
};

/// Why a caller could not create or introspect a profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileCreateError {
    /// The pure catalog rejected the request (name, conflict, generation,
    /// unknown handle, transition, pending selection).
    Profile(ProfileError),
    /// No validated directory registry was supplied (CEF-free test harness).
    PersistentUnsupported,
    StoragePath(StoragePathError),
    CookieStoreUnavailable,
    InvalidSchemeBinding(CustomSchemeValidationError),
    UnregisteredCustomScheme(String),
    /// The runtime is not Ready (`initialize` + selected default not complete).
    NotReady,
    /// The UI task could not be admitted (draining, sealed, native post
    /// rejected).
    PostFailed(EngineError),
    /// The runtime already finished shutdown.
    Closed,
}

impl std::fmt::Display for ProfileCreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Profile(error) => write!(f, "profile error: {error}"),
            Self::PersistentUnsupported => {
                write!(f, "no validated persistent profile directory registry")
            }
            Self::StoragePath(error) => write!(f, "{error}"),
            Self::CookieStoreUnavailable => write!(f, "cookie manager/store cannot be accessed"),
            Self::InvalidSchemeBinding(error) => write!(f, "{error}"),
            Self::UnregisteredCustomScheme(scheme) => {
                write!(
                    f,
                    "custom scheme '{scheme}' was not configured at runtime startup"
                )
            }
            Self::NotReady => write!(f, "runtime is not ready for profiles"),
            Self::PostFailed(error) => write!(f, "profile task was not accepted: {error:?}"),
            Self::Closed => write!(f, "runtime is closed"),
        }
    }
}

impl std::error::Error for ProfileCreateError {}

/// Result of comparing two real request contexts on the CEF UI sequence.
/// Deliberately contains only owned booleans: no CEF object is handed out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileComparison {
    /// `RequestContext::is_same`: the very same context object.
    pub is_same: bool,
    /// `RequestContext::is_sharing_with`: same storage/context semantics.
    pub is_sharing: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ComparisonState {
    Pending,
    Complete(ProfileComparison),
    Failed(ProfileCreateError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SchemeRegistrationState {
    Pending,
    Complete,
    Failed(ProfileCreateError),
}

/// Nonblocking ticket for an asynchronous UI-thread identity comparison.
/// It owns only the manager's plain-data side, never a CEF reference, and is
/// the single owner of its result record: dropping the ticket releases that
/// plain record so repeated comparisons cannot accumulate forever.
pub struct ProfileComparisonTicket {
    runtime: RuntimeGeneration,
    sequence: u64,
    /// Weak on purpose: a ticket must never keep the native profile table (and
    /// its CEF references) alive past the runtime's normal release.
    manager: Weak<ProfileManager>,
}

impl ProfileComparisonTicket {
    pub fn runtime(&self) -> RuntimeGeneration {
        self.runtime
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns `None` while the UI comparison has not run yet; otherwise the
    /// owned boolean result or a plain-data error. Never calls CEF.
    pub fn poll(&self) -> Option<Result<ProfileComparison, ProfileCreateError>> {
        let Some(manager) = self.manager.upgrade() else {
            return Some(Err(ProfileCreateError::Closed));
        };
        if self.runtime != manager.runtime {
            return Some(Err(ProfileCreateError::Profile(ProfileError::WrongRuntime)));
        }
        manager.poll_comparison(self.sequence)
    }
}

impl Drop for ProfileComparisonTicket {
    fn drop(&mut self) {
        // Plain-data bookkeeping only: no CEF call, no lock ordering issue.
        if let Some(manager) = self.manager.upgrade() {
            manager.discard_comparison(self.sequence);
        }
    }
}

impl std::fmt::Debug for ProfileComparisonTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileComparisonTicket")
            .field("runtime", &self.runtime)
            .field("sequence", &self.sequence)
            .finish()
    }
}

/// Nonblocking ticket for dynamic custom-scheme factory registration and IO barrier readiness.
pub struct ProfileSchemeTicket {
    runtime: RuntimeGeneration,
    sequence: u64,
    manager: Weak<ProfileManager>,
}

impl ProfileSchemeTicket {
    pub fn runtime(&self) -> RuntimeGeneration {
        self.runtime
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn poll(&self) -> Option<Result<(), ProfileCreateError>> {
        let Some(manager) = self.manager.upgrade() else {
            return Some(Err(ProfileCreateError::Closed));
        };
        if self.runtime != manager.runtime {
            return Some(Err(ProfileCreateError::Profile(ProfileError::WrongRuntime)));
        }
        manager.poll_scheme_registration(self.sequence)
    }
}

impl Drop for ProfileSchemeTicket {
    fn drop(&mut self) {
        if let Some(manager) = self.manager.upgrade() {
            manager.discard_scheme_registration(self.sequence);
        }
    }
}

impl std::fmt::Debug for ProfileSchemeTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileSchemeTicket")
            .field("runtime", &self.runtime)
            .field("sequence", &self.sequence)
            .finish()
    }
}

struct NativeSlot {
    /// Set on the CEF UI task immediately before the one
    /// `request_context_create_context` call. A drain that observes
    /// `issued == false` may terminate the profile without touching CEF; an
    /// issued profile stays Pending until its own handler callback arrives.
    issued: bool,
    /// Our durable strong references. `None` until the native object exists.
    context: Option<RequestContext>,
    /// Our handler reference; the context also holds its own. Kept explicit so
    /// retirement can prove the handler is gone, not just the context.
    handler: Option<RequestContextHandler>,
    policy_issued: bool,
    factory: Option<SchemeHandlerFactory>,
    custom_factories: Vec<SchemeHandlerFactory>,
}

impl NativeSlot {
    fn empty() -> Self {
        Self {
            issued: false,
            context: None,
            handler: None,
            policy_issued: false,
            factory: None,
            custom_factories: Vec::new(),
        }
    }
}

pub(crate) struct ProfileManager {
    runtime: RuntimeGeneration,
    catalog: Mutex<ProfileCatalog>,
    paths: Mutex<Option<ProfilePaths>>,
    /// Locals known to this manager (default is 0). The core catalog is the
    /// source of truth; this set only lets us count pending entries without a
    /// second state map.
    known: Mutex<BTreeSet<u64>>,
    slots: Mutex<BTreeMap<u64, NativeSlot>>,
    comparisons: Mutex<BTreeMap<u64, ComparisonState>>,
    next_comparison: AtomicU64,
    default_scheme_bindings: Mutex<Vec<ProfileSchemeBinding>>,
    scheme_registrations: Mutex<BTreeMap<u64, SchemeRegistrationState>>,
    next_scheme_registration: AtomicU64,
    /// Real count of handler callbacks currently executing. Participates in
    /// the shutdown gate so a Ready profile cannot be finalized while its
    /// callback still holds a borrowed context.
    callbacks_in_flight: AtomicU64,
    /// Set by `seal_and_release`/`abandon_all`: no later mutation may enter CEF.
    sealed: AtomicBool,
    /// The UI executor, needed to defer readiness until the callback frame has
    /// returned. Weak so the profile table never keeps the executor alive.
    executor: OnceLock<Weak<crate::executor::Executor>>,
    policy: OnceLock<Arc<NativePolicy>>,
    ui: Arc<dyn UiDispatcher>,
    audit: Arc<NativeAudit>,
}

impl ProfileManager {
    pub(crate) fn new(
        runtime: RuntimeGeneration,
        storage: ProfileStorage,
        ui: Arc<dyn UiDispatcher>,
        audit: Arc<NativeAudit>,
        paths: Option<ProfilePaths>,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            catalog: Mutex::new(ProfileCatalog::new(runtime, storage)),
            paths: Mutex::new(paths),
            known: Mutex::new(BTreeSet::from([0])),
            slots: Mutex::new(BTreeMap::from([(0, NativeSlot::empty())])),
            comparisons: Mutex::new(BTreeMap::new()),
            next_comparison: AtomicU64::new(1),
            default_scheme_bindings: Mutex::new(Vec::new()),
            scheme_registrations: Mutex::new(BTreeMap::new()),
            next_scheme_registration: AtomicU64::new(1),
            callbacks_in_flight: AtomicU64::new(0),
            sealed: AtomicBool::new(false),
            executor: OnceLock::new(),
            policy: OnceLock::new(),
            ui,
            audit,
        })
    }

    pub(crate) fn attach_scheme_bindings(&self, bindings: Vec<ProfileSchemeBinding>) {
        *self
            .default_scheme_bindings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = bindings;
    }

    pub(crate) fn attach_executor(&self, executor: &Arc<crate::executor::Executor>) {
        let _ = self.executor.set(Arc::downgrade(executor));
    }

    pub(crate) fn attach_policy(&self, policy: Arc<NativePolicy>) {
        self.lock_catalog()
            .require_preparation()
            .expect("policy attached before initialization");
        let _ = self.policy.set(policy);
    }

    pub(crate) fn policy(&self) -> Option<Arc<NativePolicy>> {
        self.policy.get().cloned()
    }

    fn executor(&self) -> Option<Arc<crate::executor::Executor>> {
        self.executor.get().and_then(Weak::upgrade)
    }

    pub(crate) fn default_handle(&self) -> ProfileHandle {
        ProfileHandle::new(self.runtime, 0)
    }

    pub(crate) fn default_ready(&self) -> bool {
        self.state_of(self.default_handle()).ok() == Some(ProfileState::Ready)
    }

    /// The Runtime owner serializes reservations. Validate names/config kinds
    /// before filesystem changes; native creation only receives validated paths.
    pub(crate) fn reserve_explicit(
        &self,
        mut config: ProfileConfig,
    ) -> Result<ProfileRegistration, ProfileCreateError> {
        ProfileCatalog::validate_name(&config.name).map_err(ProfileCreateError::Profile)?;
        {
            let catalog = self.lock_catalog();
            if let Some(existing) = catalog.named_handle(&config.name) {
                let previous = catalog
                    .config(existing)
                    .map_err(ProfileCreateError::Profile)?;
                if std::mem::discriminant(&previous.storage)
                    != std::mem::discriminant(&config.storage)
                {
                    return Err(ProfileCreateError::Profile(ProfileError::ConfigConflict));
                }
            }
        }
        if let ProfileStorage::Persistent { directory } = &config.storage {
            let mut paths = self
                .paths
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let paths = paths
                .as_mut()
                .ok_or(ProfileCreateError::PersistentUnsupported)?;
            let directory = paths
                .register(&config.name, directory)
                .map_err(ProfileCreateError::StoragePath)?;
            if let Some(policy) = self.policy() {
                policy.add_forbidden_root(directory.clone());
            }
            config.storage = ProfileStorage::Persistent { directory };
        }
        let registration = {
            let mut catalog = self.lock_catalog();
            catalog
                .reserve(config)
                .map_err(ProfileCreateError::Profile)?
        };
        if registration.is_new {
            self.lock_known().insert(registration.handle.local());
            self.lock_slots()
                .insert(registration.handle.local(), NativeSlot::empty());
        }
        Ok(registration)
    }

    pub(crate) fn state_of(
        &self,
        handle: ProfileHandle,
    ) -> Result<ProfileState, ProfileCreateError> {
        self.lock_catalog()
            .state(handle)
            .map_err(ProfileCreateError::Profile)
    }

    pub(crate) fn config_of(
        &self,
        handle: ProfileHandle,
    ) -> Result<ProfileConfig, ProfileCreateError> {
        self.lock_catalog()
            .config(handle)
            .cloned()
            .map_err(ProfileCreateError::Profile)
    }

    /// Selects the default for `None`; never falls back for a bad explicit
    /// handle. Only a Ready profile may be selected.
    pub(crate) fn select(
        &self,
        profile: Option<ProfileHandle>,
    ) -> Result<ProfileHandle, ProfileCreateError> {
        self.lock_catalog()
            .select(profile)
            .map_err(ProfileCreateError::Profile)
    }

    /// Called on the CEF UI sequence once the browser process context exists.
    /// Memory creates an independent incognito context; Persistent obtains the
    /// global. Runtime additionally gates readiness on the selected default.
    pub(crate) fn default_context_initialized(self: &Arc<Self>) -> bool {
        if self.is_sealed() {
            return false;
        }
        let handle = self.default_handle();
        if self.default_ready() {
            // Duplicate callback: no second native reference is taken.
            return true;
        }
        if self.state_of(handle) == Ok(ProfileState::Failed) {
            return false;
        }
        if matches!(
            self.config_of(handle),
            Ok(ProfileConfig {
                storage: ProfileStorage::Memory,
                ..
            })
        ) {
            self.create_profile_now(handle, false);
            return self.state_of(handle) != Ok(ProfileState::Failed);
        }
        if self
            .lock_slots()
            .get(&handle.local())
            .is_some_and(|slot| slot.context.is_some())
        {
            // Native context is already owned; policy preparation may still be
            // Pending. A duplicate process callback must not acquire/leak it again.
            return true;
        }
        self.audit.record();
        let Some(context) = request_context_get_global_context() else {
            self.lock_catalog().default_context_failed();
            self.wake();
            return false;
        };
        if self.is_sealed() {
            // Runtime abandoned concurrently: never Release a still-live object.
            std::mem::forget(context);
            return false;
        }
        {
            let mut slots = self.lock_slots();
            match slots.get_mut(&handle.local()) {
                Some(slot) if slot.context.is_none() => slot.context = Some(context),
                _ => std::mem::forget(context),
            }
        }
        let _ = self.lock_catalog().default_context_ready();
        self.maybe_install_policy(handle);
        self.wake();
        true
    }

    /// Executed only on the CEF UI sequence. `draining` is sampled on the UI
    /// thread from the runtime's registry so a queued create that never reached
    /// CEF terminates as Failed instead of starting a new context.
    pub(crate) fn create_profile_now(self: &Arc<Self>, handle: ProfileHandle, draining: bool) {
        if self.is_sealed() {
            self.fail_pending(handle);
            return;
        }
        if draining {
            // No native object was issued for this profile (the UI task never
            // reached the create call), so terminate it; the shutdown barrier
            // must not wait for a task that will never issue CEF work.
            self.fail_pending(handle);
            self.wake();
            return;
        }
        let mut missing = false;
        {
            let mut slots = self.lock_slots();
            match slots.get_mut(&handle.local()) {
                Some(slot) => {
                    if slot.issued {
                        return;
                    }
                    // Mark before the native call: a drain that observes this
                    // flag waits for the profile's own callback, exactly like a
                    // browser.
                    slot.issued = true;
                }
                None => missing = true,
            }
        }
        if missing {
            self.fail_pending(handle);
            return;
        }
        let config = match self.config_of(handle) {
            Ok(config) => config,
            Err(_) => {
                self.fail_pending(handle);
                self.wake();
                return;
            }
        };
        let mut handler = RuntimeRequestContextHandler::new(handle, Arc::downgrade(self));
        let settings = context_settings(&config.storage);
        self.audit.record();
        let created = request_context_create_context(Some(&settings), Some(&mut handler));
        let mut context = created;
        let mut handler = Some(handler);
        if self.is_sealed() {
            // The runtime abandoned the table concurrently: never Release a
            // still-live CEF object, and do not publish a transition.
            if let Some(context) = context.take() {
                std::mem::forget(context);
            }
            if let Some(handler) = handler.take() {
                std::mem::forget(handler);
            }
            return;
        }
        if context.is_none() {
            // Creation returned null. Drop our handler ref (CEF is live here)
            // and mark the failure; there is no fallback to the default.
            drop(handler.take());
            let _ = self.lock_catalog().create_returned(handle, false);
            self.wake();
            return;
        }
        let stored;
        {
            let mut slots = self.lock_slots();
            // The slot always exists for a reserved profile; a missing slot can
            // only mean the table was abandoned, in which case we keep the
            // references above instead of releasing them.
            stored = match slots.get_mut(&handle.local()) {
                Some(slot) if slot.context.is_none() => {
                    slot.context = context.take();
                    slot.handler = handler.take();
                    true
                }
                _ => false,
            };
        }
        if !stored {
            // Still-live CEF objects with no owning table: leak, never Release.
            if let Some(context) = context.take() {
                std::mem::forget(context);
            }
            if let Some(handler) = handler.take() {
                std::mem::forget(handler);
            }
            let _ = self.lock_catalog().create_returned(handle, false);
            self.wake();
            return;
        }
        // Readiness still requires this profile's own callback; core merges a
        // callback that already ran inside the create call.
        let _ = self.lock_catalog().create_returned(handle, true);
        self.maybe_install_policy(handle);
        self.wake();
    }

    /// Invoked by this profile's `RequestContextHandler` on the UI sequence.
    ///
    /// Readiness is not published here: the callback's borrowed
    /// `RequestContext` wrapper is still alive inside this frame, and marking
    /// Ready would let the main thread observe a profile whose callback-owned
    /// reference has not been returned yet. Instead the work is deferred to a
    /// follow-up UI task, which runs only after CEF returned from the callback.
    fn note_context_initialized(self: &Arc<Self>, handle: ProfileHandle) {
        if self.is_sealed() {
            return;
        }
        let Some(executor) = self.executor() else {
            // Without an executor no follow-up can run safely; leave the
            // profile Pending so the shutdown gate keeps blocking instead of
            // publishing readiness from inside the callback frame.
            return;
        };
        self.callbacks_in_flight.fetch_add(1, Ordering::AcqRel);
        let posted = executor
            .post_while_draining(TaskPayload::ProfileInitialized { handle })
            .is_ok();
        self.callbacks_in_flight.fetch_sub(1, Ordering::AcqRel);
        if !posted {
            // A sealed executor rejected the marker: the profile stays Pending
            // and the finish gate will report Blocked instead of pretending
            // readiness. Nothing native is touched here.
            self.wake();
        }
    }

    /// Runs on the CEF UI sequence after the callback frame returned.
    pub(crate) fn mark_initialized(&self, handle: ProfileHandle) {
        if self.is_sealed() {
            return;
        }
        let _ = self.lock_catalog().context_initialized(handle);
        self.maybe_install_policy(handle);
        self.wake();
    }

    fn maybe_install_policy(&self, handle: ProfileHandle) {
        let Some(policy) = self.policy() else { return };
        if self.is_sealed() || self.lock_catalog().native_initialized(handle) != Ok(true) {
            return;
        }
        let context = {
            let mut slots = self.lock_slots();
            let Some(slot) = slots.get_mut(&handle.local()) else {
                return;
            };
            if slot.policy_issued {
                return;
            }
            let Some(context) = slot.context.clone() else {
                return;
            };
            slot.policy_issued = true;
            context
        };
        let mut factory = policy.factory();
        let mut accepted = true;
        for scheme in ["http", "https"] {
            self.audit.record();
            accepted &= context.register_scheme_handler_factory(
                Some(&CefString::from(scheme)),
                None,
                Some(&mut factory),
            ) != 0;
        }
        let bindings = self
            .default_scheme_bindings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut custom_factories = Vec::with_capacity(bindings.len());
        for binding in &bindings {
            match crate::scheme_handlers::build_scheme_handler_factory(policy.clone(), binding) {
                Ok(mut custom_factory) => {
                    let domain = binding.domain.as_deref().map(CefString::from);
                    self.audit.record();
                    accepted &= context.register_scheme_handler_factory(
                        Some(&CefString::from(binding.scheme.as_str())),
                        domain.as_ref(),
                        Some(&mut custom_factory),
                    ) != 0;
                    custom_factories.push(custom_factory);
                }
                Err(_) => {
                    accepted = false;
                }
            }
        }
        drop(context);
        if let Some(slot) = self.lock_slots().get_mut(&handle.local()) {
            slot.factory = Some(factory);
            slot.custom_factories.extend(custom_factories);
        } else {
            std::mem::forget(factory);
            for custom_factory in custom_factories {
                std::mem::forget(custom_factory);
            }
            accepted = false;
        }
        // Both registrations enqueue IO work; this is an installation barrier,
        // not readiness inferred solely from the native bool return.
        if !accepted
            || self
                .executor()
                .is_none_or(|executor| executor.post_policy_io_barrier(handle).is_err())
        {
            let _ = self.lock_catalog().preparation_complete(handle, false);
            self.wake();
        }
    }

    pub(crate) fn policy_io_ready(&self, handle: ProfileHandle) {
        if self.is_sealed() {
            return;
        }
        if self.executor().is_none_or(|executor| {
            executor
                .post_while_draining(TaskPayload::ProfilePolicyReady { handle })
                .is_err()
        }) {
            let _ = self.lock_catalog().preparation_complete(handle, false);
            self.wake();
        }
    }

    pub(crate) fn policy_ready(&self, handle: ProfileHandle) {
        if !self.is_sealed() {
            let _ = self.lock_catalog().preparation_complete(handle, true);
            self.wake();
        }
    }

    /// A durable context for a Ready profile. Called on the CEF UI sequence
    /// only; the clone is a real strong reference and must stay task-local.
    pub(crate) fn context_for(&self, handle: ProfileHandle) -> Option<RequestContext> {
        let slots = self.lock_slots();
        slots.get(&handle.local())?.context.clone()
    }

    pub(crate) fn pending_profiles(&self) -> u64 {
        let catalog = self.lock_catalog();
        let known = self.lock_known();
        known
            .iter()
            .filter(|local| {
                catalog.state(ProfileHandle::new(self.runtime, **local))
                    == Ok(ProfileState::Pending)
            })
            .count() as u64
    }

    /// Real count of strong CEF references held by this manager (contexts plus
    /// explicit handler and scheme-factory references). Zero after `seal_and_release`.
    pub(crate) fn references_retained(&self) -> u64 {
        self.lock_slots()
            .values()
            .map(|slot| {
                slot.context.is_some() as u64
                    + slot.handler.is_some() as u64
                    + slot.factory.is_some() as u64
                    + slot.custom_factories.len() as u64
            })
            .sum()
    }

    pub(crate) fn callbacks_in_flight(&self) -> u64 {
        self.callbacks_in_flight.load(Ordering::Acquire)
    }

    pub(crate) fn is_sealed(&self) -> bool {
        self.sealed.load(Ordering::Acquire)
    }

    /// Starts an identity comparison without running any CEF call. The ticket
    /// is later resolved by a plain-data UI task.
    pub(crate) fn begin_comparison(
        self: &Arc<Self>,
        left: ProfileHandle,
        right: ProfileHandle,
    ) -> Result<ProfileComparisonTicket, ProfileCreateError> {
        if self.is_sealed() {
            return Err(ProfileCreateError::Closed);
        }
        for handle in [left, right] {
            let state = self.state_of(handle)?;
            if state != ProfileState::Ready {
                return Err(ProfileCreateError::Profile(ProfileError::NotReady));
            }
        }
        let sequence = self.next_comparison.fetch_add(1, Ordering::Relaxed);
        self.lock_comparisons()
            .insert(sequence, ComparisonState::Pending);
        Ok(ProfileComparisonTicket {
            runtime: self.runtime,
            sequence,
            manager: Arc::downgrade(self),
        })
    }

    /// Executed only on the CEF UI sequence. Copies the two durable contexts
    /// out under the lock, performs the real `is_same`/`is_sharing_with` calls
    /// without holding any lock, then keeps only the booleans.
    pub(crate) fn run_comparison(&self, sequence: u64, left: ProfileHandle, right: ProfileHandle) {
        if self.is_sealed() {
            self.record_comparison(sequence, Err(ProfileCreateError::Closed));
            return;
        }
        let pair = {
            let slots = self.lock_slots();
            (
                slots
                    .get(&left.local())
                    .and_then(|slot| slot.context.clone()),
                slots
                    .get(&right.local())
                    .and_then(|slot| slot.context.clone()),
            )
        };
        let (Some(left_context), Some(mut right_context)) = pair else {
            self.record_comparison(
                sequence,
                Err(ProfileCreateError::Profile(ProfileError::NotReady)),
            );
            return;
        };
        self.audit.record();
        let is_same = left_context.is_same(Some(&mut right_context)) != 0;
        self.audit.record();
        let is_sharing = left_context.is_sharing_with(Some(&mut right_context)) != 0;
        drop(left_context);
        drop(right_context);
        self.record_comparison(
            sequence,
            Ok(ProfileComparison {
                is_same,
                is_sharing,
            }),
        );
    }

    pub(crate) fn record_comparison(
        &self,
        sequence: u64,
        result: Result<ProfileComparison, ProfileCreateError>,
    ) {
        if self.is_sealed() {
            return;
        }
        let state = match result {
            Ok(comparison) => ComparisonState::Complete(comparison),
            Err(error) => ComparisonState::Failed(error),
        };
        if let Some(entry) = self.lock_comparisons().get_mut(&sequence) {
            *entry = state;
        }
        self.wake();
    }

    /// Removes the plain-data record of a ticket that was dropped or never
    /// posted. Deliberately no CEF call and no wake.
    pub(crate) fn discard_comparison(&self, sequence: u64) {
        self.lock_comparisons().remove(&sequence);
    }

    fn poll_comparison(
        &self,
        sequence: u64,
    ) -> Option<Result<ProfileComparison, ProfileCreateError>> {
        match self.lock_comparisons().get(&sequence) {
            Some(ComparisonState::Pending) => None,
            Some(ComparisonState::Complete(comparison)) => Some(Ok(*comparison)),
            Some(ComparisonState::Failed(error)) => Some(Err(error.clone())),
            None => Some(Err(ProfileCreateError::Profile(
                ProfileError::UnknownProfile,
            ))),
        }
    }

    pub(crate) fn begin_scheme_registration(
        self: &Arc<Self>,
        handle: ProfileHandle,
        binding: &ProfileSchemeBinding,
    ) -> Result<ProfileSchemeTicket, ProfileCreateError> {
        if self.is_sealed() {
            return Err(ProfileCreateError::Closed);
        }
        validate_profile_scheme_binding(binding)
            .map_err(ProfileCreateError::InvalidSchemeBinding)?;
        let Some(policy) = self.policy() else {
            return Err(ProfileCreateError::NotReady);
        };
        if !policy.rules.is_custom_scheme(&binding.scheme) {
            return Err(ProfileCreateError::UnregisteredCustomScheme(
                binding.scheme.clone(),
            ));
        }
        if self.state_of(handle)? != ProfileState::Ready {
            return Err(ProfileCreateError::Profile(ProfileError::NotReady));
        }
        let sequence = self
            .next_scheme_registration
            .fetch_add(1, Ordering::Relaxed);
        self.lock_scheme_registrations()
            .insert(sequence, SchemeRegistrationState::Pending);
        Ok(ProfileSchemeTicket {
            runtime: self.runtime,
            sequence,
            manager: Arc::downgrade(self),
        })
    }

    pub(crate) fn register_scheme_now(
        &self,
        sequence: u64,
        handle: ProfileHandle,
        binding: ProfileSchemeBinding,
    ) {
        if self.is_sealed() {
            self.fail_scheme_registration(sequence, ProfileCreateError::Closed);
            return;
        }
        let Some(policy) = self.policy() else {
            self.fail_scheme_registration(sequence, ProfileCreateError::NotReady);
            return;
        };
        let Some(context) = self
            .lock_slots()
            .get(&handle.local())
            .and_then(|slot| slot.context.clone())
        else {
            self.fail_scheme_registration(
                sequence,
                ProfileCreateError::Profile(ProfileError::NotReady),
            );
            return;
        };
        let mut factory =
            match crate::scheme_handlers::build_scheme_handler_factory(policy, &binding) {
                Ok(factory) => factory,
                Err(error) => {
                    self.fail_scheme_registration(
                        sequence,
                        ProfileCreateError::InvalidSchemeBinding(error),
                    );
                    return;
                }
            };
        let domain = binding.domain.as_deref().map(CefString::from);
        self.audit.record();
        let accepted = context.register_scheme_handler_factory(
            Some(&CefString::from(binding.scheme.as_str())),
            domain.as_ref(),
            Some(&mut factory),
        ) != 0;
        drop(context);
        if let Some(slot) = self.lock_slots().get_mut(&handle.local()) {
            slot.custom_factories.push(factory);
        } else {
            std::mem::forget(factory);
            self.fail_scheme_registration(sequence, ProfileCreateError::Closed);
            return;
        }
        if !accepted {
            self.fail_scheme_registration(
                sequence,
                ProfileCreateError::PostFailed(EngineError::Backend { code: None }),
            );
            return;
        }
        if self
            .executor()
            .is_none_or(|executor| executor.post_scheme_io_barrier(sequence).is_err())
        {
            self.fail_scheme_registration(
                sequence,
                ProfileCreateError::PostFailed(EngineError::PostRejected),
            );
        }
    }

    pub(crate) fn scheme_io_ready(&self, sequence: u64) {
        if self.is_sealed() {
            return;
        }
        if self.executor().is_none_or(|executor| {
            executor
                .post_while_draining(TaskPayload::ProfileSchemeReady { sequence })
                .is_err()
        }) {
            self.fail_scheme_registration(
                sequence,
                ProfileCreateError::PostFailed(EngineError::PostRejected),
            );
        }
    }

    pub(crate) fn scheme_ready(&self, sequence: u64) {
        if self.is_sealed() {
            return;
        }
        if let Some(entry) = self.lock_scheme_registrations().get_mut(&sequence) {
            *entry = SchemeRegistrationState::Complete;
        }
        self.wake();
    }

    pub(crate) fn fail_scheme_registration(&self, sequence: u64, error: ProfileCreateError) {
        if self.is_sealed() {
            return;
        }
        if let Some(entry) = self.lock_scheme_registrations().get_mut(&sequence) {
            *entry = SchemeRegistrationState::Failed(error);
        }
        self.wake();
    }

    pub(crate) fn discard_scheme_registration(&self, sequence: u64) {
        self.lock_scheme_registrations().remove(&sequence);
    }

    fn poll_scheme_registration(&self, sequence: u64) -> Option<Result<(), ProfileCreateError>> {
        match self.lock_scheme_registrations().get(&sequence) {
            Some(SchemeRegistrationState::Pending) => None,
            Some(SchemeRegistrationState::Complete) => Some(Ok(())),
            Some(SchemeRegistrationState::Failed(error)) => Some(Err(error.clone())),
            None => Some(Err(ProfileCreateError::Profile(
                ProfileError::UnknownProfile,
            ))),
        }
    }

    /// Seals profile mutation and releases every durable native reference on
    /// the caller's (owner) thread, before `cef::shutdown`. Returns how many
    /// references were released.
    pub(crate) fn seal_and_release(&self) -> u64 {
        self.sealed.store(true, Ordering::Release);
        // Unresolved comparison and dynamic-scheme tickets must not stay Pending
        // forever now that no UI task can ever run; resolve with Closed.
        for entry in self.lock_comparisons().values_mut() {
            if matches!(entry, ComparisonState::Pending) {
                *entry = ComparisonState::Failed(ProfileCreateError::Closed);
            }
        }
        for entry in self.lock_scheme_registrations().values_mut() {
            if matches!(entry, SchemeRegistrationState::Pending) {
                *entry = SchemeRegistrationState::Failed(ProfileCreateError::Closed);
            }
        }
        let slots = std::mem::take(&mut *self.lock_slots());
        let mut released = 0;
        for slot in slots.into_values() {
            if slot.context.is_some() {
                released += 1;
            }
            if slot.handler.is_some() {
                released += 1;
            }
            if slot.factory.is_some() {
                released += 1;
            }
            released += slot.custom_factories.len() as u64;
            // Contexts are dropped before handlers/factories so CEF's own
            // ownership disappears with its context first.
            drop(slot.context);
            drop(slot.handler);
            drop(slot.factory);
            drop(slot.custom_factories);
        }
        released
    }

    /// Drop-without-finish path: never Release a wrapper while CEF may still be
    /// live. The process is expected to exit.
    pub(crate) fn abandon_all(&self) {
        self.sealed.store(true, Ordering::Release);
        let slots = std::mem::take(&mut *self.lock_slots());
        for slot in slots.into_values() {
            if let Some(context) = slot.context {
                std::mem::forget(context);
            }
            if let Some(handler) = slot.handler {
                std::mem::forget(handler);
            }
            if let Some(factory) = slot.factory {
                std::mem::forget(factory);
            }
            for factory in slot.custom_factories {
                std::mem::forget(factory);
            }
        }
    }

    fn fail_pending(&self, handle: ProfileHandle) {
        let _ = self.lock_catalog().create_returned(handle, false);
    }

    /// Plain-data failure transition for a create task that will never issue
    /// native work (wrong thread or a sealed table). No CEF entry point is
    /// touched, so it is safe from any thread.
    pub(crate) fn fail_creation(&self, handle: ProfileHandle) {
        self.fail_pending(handle);
        self.wake();
    }

    /// A rejected UI post can never be followed by a native create return, so
    /// the profile becomes Failed through the normal core transition instead of
    /// remaining Pending and blocking the shutdown gate forever.
    pub(crate) fn create_returned_failed_for_post(
        &self,
        handle: ProfileHandle,
    ) -> Result<(), ProfileCreateError> {
        self.lock_catalog()
            .create_returned(handle, false)
            .map_err(ProfileCreateError::Profile)
    }

    fn wake(&self) {
        let _ = self.ui.wake();
    }

    fn lock_catalog(&self) -> MutexGuard<'_, ProfileCatalog> {
        self.catalog
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_known(&self) -> MutexGuard<'_, BTreeSet<u64>> {
        self.known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_slots(&self) -> MutexGuard<'_, BTreeMap<u64, NativeSlot>> {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_comparisons(&self) -> MutexGuard<'_, BTreeMap<u64, ComparisonState>> {
        self.comparisons
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_scheme_registrations(&self) -> MutexGuard<'_, BTreeMap<u64, SchemeRegistrationState>> {
        self.scheme_registrations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for ProfileManager {
    fn drop(&mut self) {
        if self.is_sealed() {
            return;
        }
        // A manager dropped without the explicit release/abandon path must not
        // run Release on CEF wrappers whose native peers may still be alive.
        let slots = std::mem::take(&mut *self.lock_slots());
        for slot in slots.into_values() {
            if let Some(context) = slot.context {
                std::mem::forget(context);
            }
            if let Some(handler) = slot.handler {
                std::mem::forget(handler);
            }
            if let Some(factory) = slot.factory {
                std::mem::forget(factory);
            }
            for factory in slot.custom_factories {
                std::mem::forget(factory);
            }
        }
    }
}

fn context_settings(storage: &ProfileStorage) -> RequestContextSettings {
    let path = match storage {
        ProfileStorage::Memory => String::new(),
        // Directory registry has already rejected non-Unicode/reparse paths.
        ProfileStorage::Persistent { directory } => crate::runtime::dos_path(directory),
    };
    RequestContextSettings {
        cache_path: CefString::from(path.as_str()),
        persist_session_cookies: 0,
        ..Default::default()
    }
}

wrap_request_context_handler! {
    // One handler per explicit profile. It carries only the plain handle and a
    // weak manager reference, so dropping it can never keep the manager alive.
    struct RuntimeRequestContextHandler {
        handle: ProfileHandle,
        profiles: Weak<ProfileManager>,
    }

    impl RequestContextHandler {
        fn on_request_context_initialized(
            &self,
            request_context: Option<&mut RequestContext>,
        ) {
            // A null callback argument is not evidence of a real initialized context.
            if request_context.is_none() {
                return;
            }
            let Some(profiles) = self.profiles.upgrade() else {
                return;
            };
            profiles.note_context_initialized(self.handle);
        }

        fn resource_request_handler(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _is_navigation: ::std::os::raw::c_int,
            _is_download: ::std::os::raw::c_int,
            request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<ResourceRequestHandler> {
            // DevTools top-level navigations (browser == None, opaque/"null"
            // initiator in CEF 156 Chrome runtime) and workers (e.g.
            // formatter_worker, initiator "devtools://devtools") both arrive on
            // the IO callback without a Browser (CefSharp IRequestContextHandler.cs:27).
            // Allow only bundled offline devtools:// assets; all webpage and
            // service-worker requests stay under TrustedContentPolicy.
            if browser.is_none()
                && crate::devtools::native::policy::allows_context_devtools_request(
                    request_initiator,
                    _is_navigation != 0,
                )
                && request.is_some_and(|r| {
                    crate::devtools::native::policy::is_internal_devtools_resource(
                        &CefString::from(&r.url()).to_string(),
                    )
                })
            {
                return Some(crate::devtools::native::policy::auxiliary_resource_handler());
            }
            self.profiles.upgrade().and_then(|profiles| profiles.policy())
                .map(crate::client::context_resource_handler)
        }
    }
}

/// CEF-free test seam for the runtime crate and its integration tests. It
/// drives the same core-catalog merge and comparison storage the native path
/// uses, but performs no CEF call and intentionally offers no way to fabricate
/// a native context reference.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::*;

    struct NoopUi;
    impl UiDispatcher for NoopUi {
        fn wake(&self) -> Result<(), EngineError> {
            Ok(())
        }
    }

    pub struct ProfileBook {
        manager: Arc<ProfileManager>,
    }

    impl ProfileBook {
        pub fn new(runtime: RuntimeGeneration) -> Self {
            Self::with_storage(runtime, ProfileStorage::Memory)
        }

        pub fn with_storage(runtime: RuntimeGeneration, storage: ProfileStorage) -> Self {
            Self {
                manager: ProfileManager::new(
                    runtime,
                    storage,
                    Arc::new(NoopUi),
                    NativeAudit::new(),
                    None,
                ),
            }
        }

        pub fn runtime_generation(&self) -> RuntimeGeneration {
            self.manager.runtime
        }

        pub fn default_handle(&self) -> ProfileHandle {
            self.manager.default_handle()
        }

        pub fn reserve(
            &self,
            config: ProfileConfig,
        ) -> Result<ProfileRegistration, ProfileCreateError> {
            self.manager.reserve_explicit(config)
        }

        pub fn state(&self, handle: ProfileHandle) -> Result<ProfileState, ProfileCreateError> {
            self.manager.state_of(handle)
        }

        pub fn config(&self, handle: ProfileHandle) -> Result<ProfileConfig, ProfileCreateError> {
            self.manager.config_of(handle)
        }

        pub fn select(
            &self,
            profile: Option<ProfileHandle>,
        ) -> Result<ProfileHandle, ProfileCreateError> {
            self.manager.select(profile)
        }

        /// Simulates the successful global-context registration. This only
        /// exists for state-machine tests: the native path additionally holds a
        /// real `RequestContext`.
        pub fn default_ready_for_test(&self) {
            let mut catalog = self.manager.lock_catalog();
            let handle = catalog.default_handle();
            if catalog.state(handle) == Ok(ProfileState::Ready) {
                return;
            }
            match catalog.config(handle).unwrap().storage {
                ProfileStorage::Memory => {
                    catalog.create_returned(handle, true).unwrap();
                    catalog.context_initialized(handle).unwrap();
                }
                ProfileStorage::Persistent { .. } => {
                    catalog.default_context_ready().unwrap();
                }
            }
        }

        pub fn default_failed_for_test(&self) {
            self.manager.lock_catalog().default_context_failed();
        }

        pub fn create_returned(
            &self,
            handle: ProfileHandle,
            accepted: bool,
        ) -> Result<(), ProfileCreateError> {
            self.manager
                .lock_catalog()
                .create_returned(handle, accepted)
                .map_err(ProfileCreateError::Profile)
        }

        pub fn context_initialized(&self, handle: ProfileHandle) -> Result<(), ProfileCreateError> {
            self.manager
                .lock_catalog()
                .context_initialized(handle)
                .map_err(ProfileCreateError::Profile)
        }

        pub fn pending_profiles(&self) -> u64 {
            self.manager.pending_profiles()
        }

        pub fn references_retained(&self) -> u64 {
            self.manager.references_retained()
        }

        pub fn begin_comparison(
            &self,
            left: ProfileHandle,
            right: ProfileHandle,
        ) -> Result<ProfileComparisonTicket, ProfileCreateError> {
            self.manager.begin_comparison(left, right)
        }

        pub fn record_comparison(
            &self,
            ticket: &ProfileComparisonTicket,
            result: Result<ProfileComparison, ProfileCreateError>,
        ) {
            self.manager.record_comparison(ticket.sequence(), result);
        }

        pub fn discard_comparison(&self, ticket: &ProfileComparisonTicket) {
            self.manager.discard_comparison(ticket.sequence());
        }

        pub fn seal_and_release(&self) -> u64 {
            self.manager.seal_and_release()
        }

        pub fn is_sealed(&self) -> bool {
            self.manager.is_sealed()
        }

        pub fn callbacks_in_flight(&self) -> u64 {
            self.manager.callbacks_in_flight()
        }

        pub fn abandon_all(&self) {
            self.manager.abandon_all();
        }

        /// Runs the queued-create body with an explicit drain observation. The
        /// harness has no CEF, so this only exercises the no-native-issue path.
        pub fn create_profile_now(&self, handle: ProfileHandle, draining: bool) {
            assert!(
                draining,
                "CEF-free profile harness only permits the drain path"
            );
            self.manager.create_profile_now(handle, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::ProfileBook;
    use super::*;

    fn memory(name: &str) -> ProfileConfig {
        ProfileConfig {
            name: name.into(),
            storage: ProfileStorage::Memory,
        }
    }

    fn book() -> ProfileBook {
        ProfileBook::new(RuntimeGeneration::new(91))
    }

    fn ready(book: &ProfileBook, name: &str) -> ProfileHandle {
        let handle = book.reserve(memory(name)).unwrap().handle;
        book.create_returned(handle, true).unwrap();
        book.context_initialized(handle).unwrap();
        handle
    }

    #[test]
    #[should_panic(expected = "CEF-free profile harness only permits the drain path")]
    fn cef_free_harness_refuses_native_creation() {
        let book = book();
        let handle = book.reserve(memory("P1")).unwrap().handle;
        book.create_profile_now(handle, false);
    }

    #[test]
    fn comparison_is_pending_then_complete_without_cef() {
        let book = book();
        book.default_ready_for_test();
        let first = book.default_handle();
        let second = ready(&book, "P1");
        let ticket = book.begin_comparison(first, second).unwrap();
        assert_eq!(ticket.poll(), None);
        book.record_comparison(
            &ticket,
            Ok(ProfileComparison {
                is_same: false,
                is_sharing: false,
            }),
        );
        assert_eq!(
            ticket.poll(),
            Some(Ok(ProfileComparison {
                is_same: false,
                is_sharing: false,
            }))
        );
        // A failed comparison keeps its plain-data error.
        let second_ticket = book.begin_comparison(first, second).unwrap();
        book.record_comparison(
            &second_ticket,
            Err(ProfileCreateError::Profile(ProfileError::NotReady)),
        );
        assert_eq!(
            second_ticket.poll(),
            Some(Err(ProfileCreateError::Profile(ProfileError::NotReady)))
        );
    }

    #[test]
    fn comparison_requires_both_profiles_ready() {
        let book = book();
        let pending = book.reserve(memory("P1")).unwrap().handle;
        assert_eq!(
            book.begin_comparison(book.default_handle(), pending)
                .unwrap_err(),
            ProfileCreateError::Profile(ProfileError::NotReady)
        );
        book.default_ready_for_test();
        assert_eq!(
            book.begin_comparison(book.default_handle(), pending)
                .unwrap_err(),
            ProfileCreateError::Profile(ProfileError::NotReady)
        );
        let foreign = ProfileHandle::new(RuntimeGeneration::new(92), 0);
        assert_eq!(
            book.begin_comparison(book.default_handle(), foreign)
                .unwrap_err(),
            ProfileCreateError::Profile(ProfileError::WrongRuntime)
        );
    }

    #[test]
    fn discard_leaves_no_unresolved_ticket() {
        let book = book();
        book.default_ready_for_test();
        let ticket = book
            .begin_comparison(book.default_handle(), book.default_handle())
            .unwrap();
        book.discard_comparison(&ticket);
        assert_eq!(
            ticket.poll(),
            Some(Err(ProfileCreateError::Profile(
                ProfileError::UnknownProfile
            )))
        );
    }

    #[test]
    fn drain_terminates_unqueued_creation_without_native_work() {
        let book = book();
        book.default_ready_for_test();
        let handle = book.reserve(memory("P1")).unwrap().handle;
        assert_eq!(book.state(handle), Ok(ProfileState::Pending));
        // No context object is issued here; the drain path must mark Failed.
        book.create_profile_now(handle, true);
        assert_eq!(book.state(handle), Ok(ProfileState::Failed));
        assert_eq!(book.pending_profiles(), 0);
        assert_eq!(book.references_retained(), 0);
    }

    #[test]
    fn pending_count_tracks_the_catalog() {
        let book = book();
        assert_eq!(book.pending_profiles(), 1, "default starts pending");
        book.default_ready_for_test();
        let first = book.reserve(memory("P1")).unwrap().handle;
        book.reserve(memory("P2")).unwrap();
        assert_eq!(book.pending_profiles(), 2);
        book.create_returned(first, true).unwrap();
        assert_eq!(
            book.pending_profiles(),
            2,
            "the create return alone is not readiness"
        );
        book.context_initialized(first).unwrap();
        assert_eq!(book.pending_profiles(), 1);
    }

    #[test]
    fn none_uses_default_and_is_rejected_until_its_context_is_ready() {
        let book = book();
        assert_eq!(
            book.select(None),
            Err(ProfileCreateError::Profile(ProfileError::NotReady))
        );
        book.default_ready_for_test();
        let implicit = book.select(None).unwrap();
        assert_eq!(implicit, book.default_handle());
        assert_eq!(book.select(Some(implicit)), Ok(implicit));
    }

    #[test]
    fn default_readiness_needs_no_handler_but_a_failure_stays_terminal() {
        let book = ProfileBook::with_storage(
            RuntimeGeneration::new(91),
            ProfileStorage::Persistent {
                directory: std::env::temp_dir(),
            },
        );
        let default = book.default_handle();
        book.default_ready_for_test();
        assert_eq!(book.state(default), Ok(ProfileState::Ready));
        assert_eq!(
            book.context_initialized(default),
            Err(ProfileCreateError::Profile(ProfileError::InvalidTransition))
        );
        let failed = ProfileBook::new(RuntimeGeneration::new(93));
        failed.default_failed_for_test();
        assert_eq!(
            failed.state(failed.default_handle()),
            Ok(ProfileState::Failed)
        );
        assert_eq!(
            failed.select(None),
            Err(ProfileCreateError::Profile(ProfileError::NotReady))
        );
    }

    #[test]
    fn same_profile_is_one_handle_and_pending_is_not_ready() {
        let book = book();
        book.default_ready_for_test();
        let first = book.reserve(memory("P1")).unwrap();
        let second = book.reserve(memory("P1")).unwrap();
        assert!(first.is_new);
        assert!(!second.is_new);
        assert_eq!(first.handle, second.handle);
        assert_eq!(
            book.select(Some(first.handle)),
            Err(ProfileCreateError::Profile(ProfileError::NotReady))
        );
        book.create_returned(first.handle, true).unwrap();
        book.context_initialized(first.handle).unwrap();
        assert_eq!(book.select(Some(first.handle)), Ok(first.handle));
    }

    #[test]
    fn foreign_handle_is_never_replaced_by_the_default() {
        let book = book();
        book.default_ready_for_test();
        let foreign = ProfileHandle::new(RuntimeGeneration::new(94), 0);
        for error in [
            book.select(Some(foreign)).unwrap_err(),
            book.state(foreign).unwrap_err(),
            book.config(foreign).unwrap_err(),
        ] {
            assert_eq!(
                error,
                ProfileCreateError::Profile(ProfileError::WrongRuntime)
            );
        }
        let unknown = ProfileHandle::new(RuntimeGeneration::new(91), 777);
        assert_eq!(
            book.select(Some(unknown)),
            Err(ProfileCreateError::Profile(ProfileError::UnknownProfile))
        );
    }

    #[test]
    fn sealing_releases_and_blocks_later_comparisons() {
        let book = book();
        book.default_ready_for_test();
        assert_eq!(book.references_retained(), 0);
        assert_eq!(book.seal_and_release(), 0);
        assert!(book.is_sealed());
        assert_eq!(
            book.begin_comparison(book.default_handle(), book.default_handle())
                .unwrap_err(),
            ProfileCreateError::Closed
        );
    }
}
