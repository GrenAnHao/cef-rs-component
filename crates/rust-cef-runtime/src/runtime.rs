//! Native Runtime owner. Initialization and final shutdown always run on the
//! host application thread that called `start`; only the CEF UI sequence touches
//! Browser objects.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::{
    ffi::c_void,
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::Ordering},
    time::Instant,
};

use cef::*;
#[cfg(feature = "test-support")]
use rust_cef_core::ports::RequestId;
use rust_cef_core::{
    contracts::{
        BrowserConfig, BrowserId, BrowserSurfaceMode, CreateError, InitializeOutcome,
        ProfileConfig, ProfileHandle, ProfileStorage, RenderPreference, RuntimeConfig,
        RuntimeGeneration, RuntimeState, ThreadMode,
    },
    ports::{
        BrowserEventSink, BrowserHandle, CloseMode, EngineError, FrameSink, FrameSource, TaskId,
        UiDispatcher,
    },
    runtime::{ShutdownBarrier, ShutdownDecision, StartupState},
};

// Public profile vocabulary re-exported so callers of `Runtime` need only one
// module path. The core definitions stay the single authority.
pub use rust_cef_core::profiles::{ProfileError, ProfileRegistration, ProfileState};

use crate::{
    app::{
        RuntimeAppBuilder, SINGLE_PROCESS_DISABLED, SINGLE_PROCESS_ENABLED,
        SINGLE_PROCESS_UNOBSERVED, SingleProcessObservation,
    },
    attachment::{AttachmentError, BrowserAttachment},
    deployment::{self, DeploymentError},
    executor::{Executor, ExecutorStats, NativeAudit, TaskPayload, current_thread_id},
    policy::{CompiledPolicy, PolicyConfigError},
    policy_native::NativePolicy,
    profile_flush::CookieFlushRegistry,
    profile_paths::{ProfilePaths, RootLease, StoragePathError, paths_equal, prepare_directory},
    profiles::ProfileManager,
    registry::{RuntimeBrowserControl, UiRegistry},
};

pub use crate::capture::CaptureDiagnostics;
pub use crate::executor::ExecutorStats as RuntimeExecutorStats;
pub use crate::profile_flush::{CookieFlushStatus, CookieFlushTicket};
pub use crate::profiles::{
    ProfileComparison, ProfileComparisonTicket, ProfileCreateError,
    ProfileCreateError as CreateProfileError,
};
pub use crate::resize::FrameRecoveryDiagnostics;
pub use rust_cef_core::frame_budget::BudgetStats as FrameBudgetStats;

/// Explicit ports supplied by the composition root. No service locator and no
/// hidden globals: the Runtime owns exactly these Arcs.
pub struct RuntimeServices {
    pub events: Arc<dyn BrowserEventSink>,
    /// Lock-external owned-frame observation. UI presentation reads the Runtime's
    /// authoritative per-Browser FrameSource; tap rejection cannot drop its slot.
    pub frames: Arc<dyn FrameSink>,
    pub ui: Arc<dyn UiDispatcher>,
}

#[derive(Debug)]
pub enum StartError {
    /// Another live Runtime already owns this process's CEF instance.
    AlreadyActive,
    Deployment(DeploymentError),
    InvalidRootCachePath(PathBuf),
    InvalidDefaultProfile(String),
    StoragePath(StoragePathError),
    /// Legacy partial-stage error retained for source compatibility. Validated
    /// Persistent storage now reports path/lease failures through StoragePath.
    PersistentProfileUnsupported,
    /// The real process arguments contain `--single-process` (or
    /// `--single-process=...`), which would collapse every profile onto one
    /// request context. Detected before any CEF call.
    SingleProcessUnsupported,
    ConflictingStorageSwitch,
    ConflictingPolicySwitch,
    InvalidContentPolicy(PolicyConfigError),
    InvalidSettings(rust_cef_core::contracts::ConfigError),
    HostExecutableUnavailable(String),
    CefDllMismatch(String),
    ApiHashUnavailable,
    LeaseFailed {
        code: u32,
    },
    /// CEF initialize returned false. The caller must end the process; no
    /// shutdown or other CEF call is allowed afterwards.
    ProcessExitRequired {
        exit_code: Option<i32>,
    },
    InitializeFailed {
        exit_code: Option<i32>,
    },
}

impl StartError {
    pub fn requires_process_exit(&self) -> bool {
        matches!(
            self,
            Self::ProcessExitRequired { .. } | Self::InitializeFailed { .. }
        )
    }
}

#[derive(Debug)]
pub enum BrowserCreateError {
    Core(CreateError),
    UnsupportedBackend(RenderPreference),
    UnsupportedProfile,
    /// The selected profile is foreign, unknown or not Ready. Never replaced by
    /// the default context.
    Profile(ProfileError),
    InvalidConfig(&'static str),
    PolicyDenied,
    PostFailed(EngineError),
    RuntimeDropped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShutdownTicket {
    runtime: RuntimeGeneration,
    sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShutdownStatus {
    NotRequested,
    Draining {
        open_browsers: Vec<BrowserId>,
        pending_tasks: u64,
        pending_requests: u64,
        /// Profile initializations plus handler callbacks still executing. Kept
        /// separate from `pending_tasks` so neither count is misreported.
        pending_profiles: u64,
        pending_cookie_flushes: u64,
        post_failures: u64,
    },
    /// The host deadline elapsed while work was still live. This is diagnostic:
    /// it never authorizes destroying unfinished instances.
    Blocked {
        open_browsers: Vec<BrowserId>,
        pending_tasks: u64,
        pending_requests: u64,
        pending_profiles: u64,
        pending_cookie_flushes: u64,
        post_failures: u64,
    },
    Ready,
    Stopped,
    ProcessExitRequired {
        exit_code: Option<i32>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShutdownError {
    NotRequested,
    Blocked {
        open_browsers: Vec<BrowserId>,
        pending_tasks: u64,
        pending_requests: u64,
        pending_profiles: u64,
        pending_cookie_flushes: u64,
        post_failures: u64,
    },
    WrongThread {
        expected: u64,
        actual: u64,
    },
    ProcessExitRequired {
        exit_code: Option<i32>,
    },
    AlreadyFinished,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShutdownCompletion {
    pub shutdown_thread_id: u64,
    pub released_browsers: usize,
}

#[derive(Clone, Debug)]
pub struct RuntimeDiagnostics {
    pub mode: ThreadMode,
    pub state: RuntimeState,
    pub main_thread_id: u64,
    pub init_thread_id: u64,
    pub context_thread_id: Option<u64>,
    pub cef_ui_thread_id: Option<u64>,
    pub shutdown_thread_id: Option<u64>,
    pub manual_pump_count: u64,
    /// Only set after a real OnPaint; the requested preference is not evidence.
    pub observed_backend: Option<RenderPreference>,
    /// Our own App wrapper reference. False only after the normal
    /// `finish_shutdown` released it before the single CEF shutdown call.
    pub app_reference_retained: bool,
    /// Incremented immediately before the one real `cef::shutdown` call. Task
    /// or native-post counts are never presented as CEF shutdown evidence.
    pub cef_shutdown_calls: u64,
    /// Accepted tasks observed at the first drain transition, before close posts.
    pub pending_tasks_at_shutdown: u64,
    /// Real native entries recorded through the shared audit.
    pub native_calls: u64,
    /// Native entries observed after the single `cef::shutdown` returned. A
    /// correctly rejected public API contributes nothing here.
    pub native_calls_after_shutdown: u64,
    /// Native continuations still registered in the private request table.
    pub pending_requests: u64,
    pub cancelled_requests: u64,
    pub before_unload_requested: u64,
    pub before_unload_answered: u64,
    pub before_unload_invalidated: u64,
    pub before_unload_timeouts: u64,
    /// Profiles whose native initialization has not reached a terminal state.
    /// Included in the shutdown gate alongside tasks and requests.
    pub pending_profiles: u64,
    /// Real count of strong CEF references held by the profile table. Must be
    /// zero after a completed shutdown.
    pub profile_references_retained: u64,
    pub pending_cookie_flushes: u64,
    pub cookie_flushes_accepted: u64,
    pub cookie_flushes_completed: u64,
    pub root_lease_held: bool,
    pub policy_denied: [u64; 12],
    pub policy_unattributed_denials: u64,
    pub policy_factory_without_browser: u64,
    /// `Some(true)` only when the real CEF command line was observed and
    /// `--single-process` was present; `Some(false)` when observed absent;
    /// `None` when the command line was never observed (never fake "false").
    pub single_process_enabled: Option<bool>,
    /// Explicit inverse alias of `single_process_enabled` for hosts that read
    /// the property as "disabled". Both come from the same single observation.
    pub single_process_disabled: Option<bool>,
    pub tasks: ExecutorStats,
}

/// Startup merge point shared with the App/BrowserProcessHandler. Both the
/// initialize return and OnContextInitialized are recorded regardless of order.
pub(crate) struct StartupEndpoint {
    state: Mutex<StartupState>,
    /// Readiness notification for Threaded hosts that do not pump. The wake is
    /// plain owned data and is always sent after the state lock is released.
    ui: Arc<dyn UiDispatcher>,
}

impl StartupEndpoint {
    pub(crate) fn new(ui: Arc<dyn UiDispatcher>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(StartupState::new()),
            ui,
        })
    }

    /// Wake only on the transition into Ready: a duplicate ContextReady or a
    /// second initialize record is not new readiness, and the host may already
    /// have consumed the first notification. The wake is sent after the state
    /// lock is released.
    fn wake_if_newly_ready(before: RuntimeState, state: &StartupState) -> bool {
        before != RuntimeState::Ready && state.state() == RuntimeState::Ready
    }

    pub(crate) fn context_initialized(&self, thread_id: u64) {
        let wake = match self.state.lock() {
            Ok(mut state) => {
                let before = state.state();
                state.context_initialized(thread_id);
                Self::wake_if_newly_ready(before, &state)
            }
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                let before = state.state();
                state.context_initialized(thread_id);
                Self::wake_if_newly_ready(before, &state)
            }
        };
        if wake {
            let _ = self.ui.wake();
        }
    }

    fn initialize_returned(&self, outcome: InitializeOutcome) {
        let wake = match self.state.lock() {
            Ok(mut state) => {
                let before = state.state();
                state.initialize_returned(outcome);
                Self::wake_if_newly_ready(before, &state)
            }
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                let before = state.state();
                state.initialize_returned(outcome);
                Self::wake_if_newly_ready(before, &state)
            }
        };
        if wake {
            let _ = self.ui.wake();
        }
    }

    fn snapshot(&self) -> (RuntimeState, Option<u64>) {
        match self.state.lock() {
            Ok(state) => (state.state(), state.ui_thread_id()),
            Err(poisoned) => {
                let state = poisoned.into_inner();
                (state.state(), state.ui_thread_id())
            }
        }
    }
}

/// Pre-initialize process lease. This is a Windows kernel mutex handle owned by
/// this call, not a component global registry. Until `commit` the guard is
/// ordinary RAII: a preflight/API-hash failure releases the name so a later
/// `start` in the same process is still possible.
struct ProcessLease {
    handle: *mut c_void,
}

impl ProcessLease {
    fn acquire() -> Result<Self, StartError> {
        Self::acquire_named(&format!("rust-cef-gpui-runtime-{}", std::process::id()))
    }

    /// Test-only naming hook. Production callers always use `acquire`, whose
    /// name is fixed per PID; a committed test token must never collide with it.
    fn acquire_named(name: &str) -> Result<Self, StartError> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // Clear a stale error so ERROR_ALREADY_EXISTS is meaningful below.
        unsafe { SetLastError(0) };
        // SAFETY: no security attributes, initially unowned, NUL-terminated name.
        let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, wide.as_ptr()) };
        if handle.is_null() {
            return Err(StartError::LeaseFailed {
                code: unsafe { GetLastError() },
            });
        }
        // ERROR_ALREADY_EXISTS means a live Runtime already owns the name.
        if unsafe { GetLastError() } == 183 {
            unsafe { CloseHandle(handle) };
            return Err(StartError::AlreadyActive);
        }
        Ok(Self { handle })
    }

    /// Called only immediately before `initialize`. The committed OS handle is
    /// kept until process exit whether initialize succeeds or returns false,
    /// because CEF cannot be re-initialized in the same process.
    fn commit(mut self) -> ProcessToken {
        let handle = std::mem::replace(&mut self.handle, std::ptr::null_mut());
        ProcessToken { handle }
    }
}

impl Drop for ProcessLease {
    fn drop(&mut self) {
        // Pre-init failure reclaims the name. After `commit` the handle has
        // moved to the process token, which never closes it.
        if !self.handle.is_null() {
            unsafe { CloseHandle(self.handle) };
            self.handle = std::ptr::null_mut();
        }
    }
}

/// A process token consumed by a real initialize attempt. It deliberately never
/// releases the OS handle: doing so would allow a second `Runtime::start` in
/// this process to pass the lease and re-enter unsupported CEF state.
struct ProcessToken {
    handle: *mut c_void,
}

impl Drop for ProcessToken {
    fn drop(&mut self) {
        let _ = self.handle;
    }
}

/// Owns the strong reference this Runtime created for the CEF App wrapper. CEF
/// takes its own reference during `initialize`; this owner only tracks ours.
/// Released explicitly before `cef::shutdown` on the normal path. If the owner
/// is dropped before the barrier, the reference is leaked instead of running a
/// wrapper release while native peers may still be live.
struct AppOwner {
    app: Option<cef::App>,
}

impl AppOwner {
    fn new(app: cef::App) -> Self {
        Self { app: Some(app) }
    }

    fn is_retained(&self) -> bool {
        self.app.is_some()
    }

    fn release_before_cef_shutdown(&mut self) {
        self.app.take();
    }
}

impl Drop for AppOwner {
    fn drop(&mut self) {
        if let Some(app) = self.app.take() {
            std::mem::forget(app);
        }
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateMutexW(attributes: *mut c_void, initial_owner: i32, name: *const u16) -> *mut c_void;
    fn CloseHandle(object: *mut c_void) -> i32;
    fn GetLastError() -> u32;
    fn SetLastError(code: u32);
}

pub struct Runtime {
    config: RuntimeConfig,
    services: RuntimeServices,
    startup: Arc<StartupEndpoint>,
    registry: Arc<UiRegistry>,
    executor: Arc<Executor>,
    profiles: Arc<ProfileManager>,
    policy: Arc<NativePolicy>,
    flushes: Arc<CookieFlushRegistry>,
    profile_cookies: Arc<crate::profile_cookies::ProfileCookieRegistry>,
    url_requests: Arc<crate::url_request::UrlRequestManager>,
    root_lease: RootLease,
    single_process: SingleProcessObservation,
    /// Committed once a real initialize attempt starts; never reopened in this
    /// process.
    lease: ProcessToken,
    /// Our App reference; released by `finish_shutdown` before CEF shutdown and
    /// leaked by `Drop` when the barrier never completed.
    app: AppOwner,
    main_thread_id: u64,
    init_thread_id: u64,
    manual_pump_count: u64,
    shutdown_thread_id: Option<u64>,
    shutdown_ticket: Option<ShutdownTicket>,
    /// First requested deadline, kept only so the native profile/callback gate
    /// can distinguish Draining from Blocked without changing core semantics.
    shutdown_deadline: Option<Instant>,
    /// Pure lifecycle policy over actual adapter observations. Production
    /// request/poll/finish all consume this instead of a parallel model.
    barrier: ShutdownBarrier,
    finished: bool,
    cef_shutdown_calls: u64,
    pending_tasks_at_shutdown: u64,
    audit: Arc<NativeAudit>,
    next_local: u64,
    /// The owner is deliberately !Send: initialize/shutdown/pump belong to the
    /// host application thread that called `start`.
    _owner_thread: PhantomData<*mut ()>,
}

impl Runtime {
    pub fn start(mut config: RuntimeConfig, services: RuntimeServices) -> Result<Self, StartError> {
        if !config.root_cache_path.is_absolute() || config.root_cache_path.as_os_str().is_empty() {
            return Err(StartError::InvalidRootCachePath(
                config.root_cache_path.clone(),
            ));
        }
        // Reject the real arguments before ANY CEF bootstrap call. Single
        // process mode would force every profile onto the first request
        // context, so the isolation guarantee cannot be claimed.
        if contains_single_process_switch(std::env::args_os().skip(1)) {
            return Err(StartError::SingleProcessUnsupported);
        }
        if contains_storage_override_switch(std::env::args_os().skip(1)) {
            return Err(StartError::ConflictingStorageSwitch);
        }
        if contains_policy_override_switch(std::env::args_os().skip(1)) {
            return Err(StartError::ConflictingPolicySwitch);
        }
        config
            .settings
            .validate()
            .map_err(StartError::InvalidSettings)?;
        let compiled_policy = Arc::new(
            CompiledPolicy::compile_with_custom_schemes(
                &config.content_policy,
                &config.settings.custom_schemes,
            )
            .map_err(StartError::InvalidContentPolicy)?,
        );
        if config.files.helper_executable.as_os_str().is_empty() {
            return Err(StartError::Deployment(DeploymentError::EmptyHelperPath));
        }
        let host = std::env::current_exe()
            .map_err(|error| StartError::HostExecutableUnavailable(error.to_string()))?;
        // Preflight file validation is pure I/O and may run before the lease.
        let validated =
            deployment::validate_files(&config.files, &host).map_err(StartError::Deployment)?;

        // The process token must be acquired before ANY CEF bootstrap call.
        // `cef_version_info`/`api_hash`/thread helpers already touch libcef; if a
        // previous initialize returned false or a shutdown completed, a second
        // `start` must be rejected here without entering CEF at all.
        let lease = ProcessLease::acquire()?;
        // Validate storage and take the cooperative root lease before any CEF
        // call. Pre-initialize failures release both guards by ordinary RAII.
        let mut root_lease =
            RootLease::acquire(&config.root_cache_path).map_err(StartError::StoragePath)?;
        let root_cache = root_lease.root().to_path_buf();
        if let ProfileStorage::Persistent { directory } = &config.default_profile {
            // A missing directory cannot name the already-created runtime root.
            // Refuse it without creating an unrelated directory outside root.
            if !directory.is_dir() {
                return Err(StartError::InvalidDefaultProfile(
                    "default persistent directory must name the runtime root".into(),
                ));
            }
            let directory = prepare_directory(directory).map_err(StartError::StoragePath)?;
            if !paths_equal(&directory, &root_cache) {
                return Err(StartError::InvalidDefaultProfile(
                    "default persistent directory must equal the real runtime root".into(),
                ));
            }
            config.default_profile = ProfileStorage::Persistent {
                directory: root_cache.clone(),
            };
        }
        config.root_cache_path = root_cache.clone();
        crate::subprocess::check_loaded_cef()
            .map_err(|error| StartError::CefDllMismatch(error.to_string()))?;

        // This cef-rs revision requires the API hash before any other CEF call.
        // Thread identity is OS-backed, so diagnostics need no libcef entry.
        if api_hash(sys::CEF_API_VERSION_LAST, 0).is_null() {
            return Err(StartError::ApiHashUnavailable);
        }
        let main_thread_id = current_thread_id();

        let generation = RuntimeGeneration::new(1);
        let audit = NativeAudit::new();
        let registry = UiRegistry::new(
            generation,
            services.events.clone(),
            services.frames.clone(),
            services.ui.clone(),
            audit.clone(),
        );
        registry.set_windowless_rendering_enabled(config.settings.windowless_rendering_enabled);
        let policy = NativePolicy::new(compiled_policy, Arc::downgrade(&registry));
        policy.add_forbidden_root(root_cache.clone());
        if let ProfileStorage::Persistent { directory } = &config.default_profile {
            policy.add_forbidden_root(directory.clone());
        }
        registry.attach_policy(policy.clone());
        let executor = Executor::new(generation, &registry, audit.clone());
        registry.attach_executor(&executor);
        let startup = StartupEndpoint::new(services.ui.clone());
        let profiles = ProfileManager::new(
            generation,
            config.default_profile.clone(),
            services.ui.clone(),
            audit.clone(),
            Some(ProfilePaths::new(root_cache.clone())),
        );
        profiles.attach_policy(policy.clone());
        let startup_scheme_bindings: Vec<rust_cef_core::custom_schemes::ProfileSchemeBinding> =
            config
                .settings
                .custom_schemes
                .iter()
                .filter_map(|scheme| {
                    scheme.asset_source.as_ref().map(|source| {
                        rust_cef_core::custom_schemes::ProfileSchemeBinding {
                            scheme: scheme.scheme.clone(),
                            domain: scheme.domain.clone(),
                            source: source.clone(),
                        }
                    })
                })
                .collect();
        profiles.attach_scheme_bindings(startup_scheme_bindings);
        let flushes =
            CookieFlushRegistry::new(&profiles, &executor, services.ui.clone(), audit.clone());
        let profile_cookies = crate::profile_cookies::ProfileCookieRegistry::new(
            &profiles,
            &executor,
            services.ui.clone(),
            audit.clone(),
        );
        let url_requests = crate::url_request::UrlRequestManager::new(
            generation,
            policy.clone(),
            &profiles,
            services.ui.clone(),
            audit.clone(),
        );
        registry.attach_flushes(&flushes);
        registry.attach_profile_cookies(&profile_cookies);
        registry.attach_url_requests(&url_requests);
        registry.attach_profiles(&profiles);
        profiles.attach_executor(&executor);
        let single_process: SingleProcessObservation =
            Arc::new(std::sync::atomic::AtomicU8::new(0));

        let custom_schemes_switch = if config.settings.custom_schemes.is_empty() {
            None
        } else {
            let encoded = rust_cef_core::custom_schemes::encode_custom_schemes_switch(
                &config.settings.custom_schemes,
            )
            .map_err(|_| {
                StartError::InvalidSettings(rust_cef_core::contracts::ConfigError::ForbiddenSwitch)
            })?;
            Some(encoded)
        };
        let args = cef::args::Args::new();
        let mut app = RuntimeAppBuilder::new(
            startup.clone(),
            Arc::downgrade(&profiles),
            single_process.clone(),
            config.settings.allowlisted_switches.clone(),
            config.settings.custom_schemes.clone(),
            custom_schemes_switch,
        );
        let settings = build_settings(
            &config,
            &root_cache,
            validated.directory(),
            validated.helper_executable(),
        );
        // Recorded immediately before the actual initialize call so the probe
        // reports the thread that really invoked it.
        let init_thread_id = current_thread_id();
        // From here CEF may be initialized, so the process token must stay
        // consumed even if initialize returns false.
        let lease = lease.commit();
        audit.record();
        let initialized = initialize(
            Some(args.as_main_args()),
            Some(&settings),
            Some(&mut app),
            std::ptr::null_mut(),
        );
        if initialized != 1 {
            // Native initialize requested process exit. Record the outcome and
            // return without any shutdown or further CEF call. The App wrapper
            // is leaked: releasing it would itself be a CEF call after the
            // failed initialize. Any profile reference a very early
            // ContextReady already stored is abandoned (leaked), never released.
            let exit_code = Some(get_exit_code());
            startup.initialize_returned(InitializeOutcome::ProcessExitRequired { exit_code });
            profiles.abandon_all();
            root_lease.abandon();
            std::mem::forget(app);
            return Err(StartError::ProcessExitRequired { exit_code });
        }
        startup.initialize_returned(InitializeOutcome::Accepted);

        Ok(Self {
            config,
            services,
            startup,
            registry,
            executor,
            profiles,
            policy,
            flushes,
            profile_cookies,
            url_requests,
            root_lease,
            single_process,
            lease,
            app: AppOwner::new(app),
            main_thread_id,
            init_thread_id,
            manual_pump_count: 0,
            shutdown_thread_id: None,
            shutdown_ticket: None,
            shutdown_deadline: None,
            barrier: ShutdownBarrier::new(InitializeOutcome::Accepted),
            finished: false,
            cef_shutdown_calls: 0,
            pending_tasks_at_shutdown: 0,
            audit,
            next_local: 0,
            _owner_thread: PhantomData,
        })
    }

    pub fn state(&self) -> RuntimeState {
        if self.finished {
            return RuntimeState::Stopped;
        }
        let startup = self.startup.snapshot().0;
        if startup != RuntimeState::Ready {
            return startup;
        }
        // Process ContextReady and default-profile readiness are distinct.
        // Memory additionally waits for its independent context's own handler;
        // Persistent becomes ready after acquiring the global context.
        if !self.profiles.default_ready() {
            return RuntimeState::Starting;
        }
        self.barrier.state()
    }

    pub fn mode(&self) -> ThreadMode {
        self.config.thread_mode
    }

    pub fn is_ready(&self) -> bool {
        self.state() == RuntimeState::Ready
    }

    /// The one default profile handle for this runtime generation. Passing it
    /// explicitly and omitting `BrowserConfig::profile` select the same stored
    /// default context (Memory incognito or Persistent global), not a second profile.
    pub fn default_profile_handle(&self) -> ProfileHandle {
        self.profiles.default_handle()
    }

    pub fn profile_state(&self, handle: ProfileHandle) -> Result<ProfileState, ProfileCreateError> {
        self.profiles.state_of(handle)
    }

    pub fn profile_config(
        &self,
        handle: ProfileHandle,
    ) -> Result<ProfileConfig, ProfileCreateError> {
        self.profiles.config_of(handle)
    }

    /// Reserves (or reuses) a named profile and schedules its one native context
    /// creation on CEF UI. Persistent paths must be validated direct children
    /// of root. Pending becomes Ready only after factory return and its handler.
    pub fn create_profile(
        &mut self,
        config: ProfileConfig,
    ) -> Result<ProfileRegistration, ProfileCreateError> {
        if self.finished {
            return Err(ProfileCreateError::Closed);
        }
        if self.state() != RuntimeState::Ready || self.registry.is_draining() {
            return Err(ProfileCreateError::NotReady);
        }
        if self.profiles.is_sealed() {
            return Err(ProfileCreateError::Closed);
        }
        if self.executor.is_stopped() {
            return Err(ProfileCreateError::Closed);
        }
        let registration = self.profiles.reserve_explicit(config)?;
        if !registration.is_new {
            // A repeated reservation never retries a profile id. A Failed
            // handle is reported instead of being presented as an in-progress
            // creation; Ready and Pending handles are returned for sharing.
            return match self.profiles.state_of(registration.handle) {
                Ok(ProfileState::Failed) => {
                    Err(ProfileCreateError::Profile(ProfileError::NotReady))
                }
                Ok(ProfileState::Ready | ProfileState::Pending) => Ok(registration),
                Err(error) => Err(error),
            };
        }
        if let Err(error) = self.executor.post(TaskPayload::CreateProfile {
            handle: registration.handle,
        }) {
            // A rejected post leaves the profile Pending forever otherwise;
            // the core transition merges it into Failed without CEF work.
            let _ = self
                .profiles
                .create_returned_failed_for_post(registration.handle);
            return Err(ProfileCreateError::PostFailed(error));
        }
        Ok(registration)
    }

    /// Flushes the selected cookie backing store only; this is not an all-storage flush.
    pub fn flush_profile_cookies(
        &self,
        handle: ProfileHandle,
    ) -> Result<CookieFlushTicket, ProfileCreateError> {
        if self.finished {
            return Err(ProfileCreateError::Closed);
        }
        if self.state() != RuntimeState::Ready || self.registry.is_draining() {
            return Err(ProfileCreateError::NotReady);
        }
        self.flushes.request(handle)
    }

    /// Test only: correlate a single named request with its genuine callback,
    /// retaining only the normal UI completion marker, never a CEF wrapper.
    #[cfg(feature = "test-support")]
    pub fn probe_hold_cookie_flush_marker(
        &self,
        profile: ProfileHandle,
    ) -> Result<(), EngineError> {
        if profile.runtime() != self.generation() {
            return Err(EngineError::WrongRuntime);
        }
        if self.finished || self.registry.is_draining() {
            return Err(EngineError::Closing);
        }
        self.profiles
            .select(Some(profile))
            .map_err(|_| EngineError::NotReady)?;
        self.flushes.probe_arm_marker(profile)
    }

    #[cfg(feature = "test-support")]
    pub fn probe_cookie_flush_marker(&self) -> Option<crate::test_support::CookieFlushMarkerProbe> {
        self.flushes.probe_marker()
    }

    #[cfg(feature = "test-support")]
    pub fn probe_release_cookie_flush_marker(
        &self,
        profile: ProfileHandle,
        sequence: u64,
    ) -> Result<TaskId, EngineError> {
        if self.finished || !self.registry.is_draining() {
            return Err(EngineError::InvalidRequest);
        }
        self.flushes.probe_release_marker(profile, sequence)
    }

    /// Starts an asynchronous identity comparison on the CEF UI sequence. The
    /// ticket only ever holds owned booleans; no CEF object leaves the UI
    /// thread. `poll` is nonblocking and never calls CEF.
    pub fn compare_profiles(
        &self,
        left: ProfileHandle,
        right: ProfileHandle,
    ) -> Result<ProfileComparisonTicket, ProfileCreateError> {
        if self.finished {
            return Err(ProfileCreateError::Closed);
        }
        if self.executor.is_stopped() {
            return Err(ProfileCreateError::Closed);
        }
        let ticket = self.profiles.begin_comparison(left, right)?;
        if let Err(error) = self.executor.post(TaskPayload::CompareContexts {
            sequence: ticket.sequence(),
            left,
            right,
        }) {
            self.profiles.discard_comparison(ticket.sequence());
            return Err(ProfileCreateError::PostFailed(error));
        }
        Ok(ticket)
    }

    fn generation(&self) -> RuntimeGeneration {
        self.registry.runtime_generation()
    }

    /// Same owned mailbox used by native callbacks; no native/GPUI object escapes.
    /// Retained sources become permanently closed at OnBeforeClose. `latest` is
    /// a query; hosts consume `take_update` after a wake to re-arm notifications.
    pub fn frame_source(&self, browser: BrowserId) -> Result<Arc<dyn FrameSource>, EngineError> {
        self.registry.frame_source(browser)
    }

    /// Submit a caller-held frame to the two actual mailbox gates for the
    /// explicit windowless replay test. No CEF call or callback is performed.
    #[cfg(feature = "test-support")]
    pub fn probe_replay_retired_view(
        &self,
        live: BrowserId,
        frame: &rust_cef_core::ports::OwnedFrame,
    ) -> Result<[Option<String>; 2], EngineError> {
        if self.finished {
            return Err(EngineError::Closed);
        }
        self.registry.probe_replay_retired_view(live, frame)
    }

    /// Claim the one-shot view attachment for the Browser behind `handle`.
    ///
    /// The returned token keeps the caller's own `BrowserHandle`, so a later
    /// drop requests the ordinary Graceful close through the original control
    /// and close-ticket sequence; no second runtime control is constructed.
    /// The claim itself is sticky in the registry: this Browser can be attached
    /// once, even if the returned token is dropped unused. `Creating` is valid
    /// because creation was already requested; this call never creates or
    /// initializes anything.
    ///
    /// Only the full `BrowserId` carried by the handle is authoritative for the
    /// claim, and that check runs against this Runtime's own registry entry. A
    /// host is trusted to pass the handle returned by this Runtime's
    /// `create_browser`; `BrowserHandle::new` is a public core constructor, so a
    /// forged `BrowserControl` reporting the same `BrowserId` still consumes the
    /// real one-shot claim and would be the control closed on token drop. The
    /// adapter therefore never treats its own return value as proof of the
    /// caller's control identity.
    pub fn attach_browser(
        &self,
        handle: &BrowserHandle,
    ) -> Result<BrowserAttachment, AttachmentError> {
        // The full identity and lifecycle gate lives in one place: the claim is
        // decided under the registry state lock. This only reads the owned
        // BrowserId from the caller's own control.
        let browser = handle.snapshot().browser;
        let (source, surface_mode) = self.registry.claim_view_attachment(browser)?;
        let ax_tree = self.registry.ax_tree_slot(browser).unwrap_or_default();
        Ok(BrowserAttachment::new(browser, handle.clone(), source)
            .with_ax_tree(ax_tree)
            .with_surface_mode(surface_mode))
    }

    pub fn native_browser_hwnd(&self, browser: BrowserId) -> Option<std::num::NonZeroUsize> {
        self.registry.native_browser_hwnd(browser)
    }

    pub fn browser_surface_mode(&self, browser: BrowserId) -> Option<BrowserSurfaceMode> {
        self.registry.surface_mode(browser)
    }

    pub fn windowless_rendering_enabled(&self) -> bool {
        self.config.settings.windowless_rendering_enabled
    }

    /// CPU pixel allocations for both surfaces, all retained snapshots and
    /// capture/presentation scratch. Not process RSS or GPU/atlas allocation.
    pub fn frame_budget_stats(&self, browser: BrowserId) -> Result<FrameBudgetStats, EngineError> {
        self.registry.frame_budget_stats(browser)
    }

    /// Explicit UI-neutral menu opt-in. Does not install a presenter/provider
    /// or assert native window authentication. Missing managed capabilities
    /// reject the entire opening with the registration's first diagnostic.
    /// Request managed DevTools for one Browser. Default closed: unconfigured
    /// capacity and any existing auxiliary are refused by pure bookkeeping
    /// before CEF is touched; no Show service is installed yet.
    pub fn open_devtools(&self, browser: BrowserId) -> Result<(), EngineError> {
        self.registry.open_devtools(browser)
    }
    pub fn devtools_tails(&self, browser: BrowserId) -> Option<String> {
        self.registry.devtools_tails(browser)
    }
    /// Explicit host opt-in for confirmed printing. Until this is called the
    /// print handler cancels every dialog and never queues a callback.
    pub fn enable_print_confirmation(&self) -> Result<(), EngineError> {
        crate::print_provider::enable();
        Ok(())
    }
    /// Pre-approve exactly one print job for this Browser and document name.
    pub fn approve_print_job(
        &self,
        browser: BrowserId,
        document: &str,
        registration: std::num::NonZeroU64,
    ) -> Result<bool, EngineError> {
        // Snapshot the real main-frame epoch at approval: a later navigation
        // must invalidate this token instead of inheriting it.
        let epoch = self
            .registry
            .document_epoch(browser)
            .ok_or(EngineError::Closed)?;
        Ok(crate::print_provider::approve_job(
            browser,
            document,
            registration,
            epoch,
        ))
    }
    /// Continue or cancel a pending print dialog. The callback itself stays on
    /// the CEF UI thread; this only posts the decision.
    pub fn resolve_print_dialog(
        &self,
        browser: BrowserId,
        accept: bool,
    ) -> Result<(), EngineError> {
        self.registry.resolve_print_dialog(browser, accept)
    }
    #[cfg(feature = "test-support")]
    /// Test-only print trigger through the real BrowserHost.
    pub fn probe_print(&self, browser: BrowserId) -> Result<(), EngineError> {
        self.registry.probe_print(browser)
    }
    /// True while a print dialog callback is stored and awaiting resolution.
    pub fn print_dialog_pending(&self, browser: BrowserId) -> bool {
        self.registry.print_dialog_pending(browser)
    }
    /// Monotonic main-frame document epoch for this Browser. Real LoadStarted
    /// events increment it; unknown Browsers return None.
    pub fn document_epoch(&self, browser: BrowserId) -> Option<u64> {
        self.registry.document_epoch(browser)
    }
    /// Number of print operations still awaiting a terminal result. A shutdown
    /// gate must see zero; the ledger is empty until a real provider exists.
    pub fn print_pending(&self) -> u64 {
        self.registry.print_pending()
    }
    /// Current auxiliary phase for diagnostics: reserved / issued / created /
    /// before-close, or None when no slot exists. Read-only.
    pub fn devtools_phase(&self, browser: BrowserId) -> Option<&'static str> {
        self.registry.devtools_phase(browser)
    }
    /// Explicitly close the associated DevTools browser (CefSharp CloseDevTools).
    pub fn close_dev_tools(&self, browser: BrowserId) -> Result<(), EngineError> {
        self.registry.close_devtools(browser)
    }
    /// Association sampled right after an explicit close_dev_tools.
    pub fn devtools_associated_after_close(&self, browser: BrowserId) -> Option<bool> {
        self.registry.devtools_associated_after_close(browser)
    }
    /// Association reported by the browser host's own HasDevTools() after
    /// ShowDevTools: Some(true) means a real DevTools browser exists for it.
    pub fn devtools_associated(&self, browser: BrowserId) -> Option<bool> {
        self.registry.devtools_associated(browser)
    }
    /// Number of auxiliary DevTools slots still held for this Browser. Zero is
    /// the only value a shutdown may treat as fully retired.
    pub fn devtools_pending(&self, browser: BrowserId) -> u64 {
        self.registry.devtools_pending(browser)
    }
    /// Explicit host opt-in for managed DevTools capacity. Default stays
    /// closed; this records the bound but does not itself Show a DevTools
    /// browser or accept any auxiliary window as retired.
    pub fn configure_devtools(&self, capacity: std::num::NonZeroUsize) -> Result<(), EngineError> {
        self.registry.configure_devtools(capacity)
    }
    /// Host opt-in for the structural menu forms CEF's windowless model lacks:
    /// a checkable item and a one-level submenu, appended as host-managed items.
    pub fn configure_menu_structure(&self, enabled: bool) -> Result<(), EngineError> {
        self.registry.configure_menu_structure(enabled)
    }
    /// The host-managed structural command the last real selection landed on.
    pub fn host_menu_item(&self) -> Option<i32> {
        self.registry.host_menu_item()
    }
    /// `(CEF asked for a context-menu handler, one was bound)` counts. Host-facing
    /// diagnostic for "the right click never reached the component".
    pub fn context_menu_handler_counts(&self) -> (u64, u64) {
        self.registry.menu_handler_counts()
    }
    /// Whether a menu bridge binding currently answers this Browser.
    pub fn context_menu_bound(&self, browser: BrowserId) -> bool {
        self.registry.menu_bridge.get(browser).is_some()
    }
    pub fn configure_finds(
        &self,
        limits: rust_cef_core::find::FindLimits,
    ) -> Result<(), EngineError> {
        self.registry.configure_finds(limits)
    }
    pub fn find_diagnostics(&self) -> crate::FindDiagnostics {
        self.registry.finds.snapshot()
    }
    pub fn js_bridge_diagnostics(&self) -> (u64, u64, u64, u64, u64) {
        self.registry.js_bridge_stats()
    }
    pub fn set_resource_interceptor(
        &self,
        interceptor: Option<Arc<dyn rust_cef_core::custom_schemes::ResourceInterceptor>>,
    ) -> Result<(), EngineError> {
        if self.finished {
            return Err(EngineError::Closed);
        }
        if self.registry.is_draining() {
            return Err(EngineError::Closing);
        }
        self.policy.set_interceptor(interceptor);
        Ok(())
    }
    pub fn register_profile_scheme_handler(
        &self,
        profile: ProfileHandle,
        binding: rust_cef_core::custom_schemes::ProfileSchemeBinding,
    ) -> Result<crate::profiles::ProfileSchemeTicket, ProfileCreateError> {
        if self.finished {
            return Err(ProfileCreateError::Closed);
        }
        if self.state() != RuntimeState::Ready || self.registry.is_draining() {
            return Err(ProfileCreateError::NotReady);
        }
        let ticket = self.profiles.begin_scheme_registration(profile, &binding)?;
        let sequence = ticket.sequence();
        if let Err(error) = self.executor.post(TaskPayload::RegisterProfileScheme {
            sequence,
            handle: profile,
            binding,
        }) {
            self.profiles
                .fail_scheme_registration(sequence, ProfileCreateError::PostFailed(error.clone()));
            return Err(ProfileCreateError::PostFailed(error));
        }
        Ok(ticket)
    }

    pub fn set_host_capabilities(
        &self,
        capabilities: rust_cef_core::host_capabilities::HostCapabilities,
    ) -> Result<(), EngineError> {
        if self.finished {
            return Err(EngineError::Closed);
        }
        if self.registry.is_draining() {
            return Err(EngineError::Closing);
        }
        self.policy.set_host_capabilities(capabilities);
        Ok(())
    }

    pub fn host_capability_pending_count(&self) -> u64 {
        self.registry.host_requests.pending_count()
    }

    fn ensure_capability_active(
        &self,
        browser: BrowserId,
    ) -> Result<(), rust_cef_core::host_capabilities::HostCapabilityError> {
        use rust_cef_core::host_capabilities::HostCapabilityError;
        if self.finished || self.registry.is_draining() {
            return Err(HostCapabilityError::BrowserClosed);
        }
        if browser.runtime() != self.generation() {
            return Err(HostCapabilityError::WrongRuntime);
        }
        if self.state() != RuntimeState::Ready {
            return Err(HostCapabilityError::NotReady);
        }
        Ok(())
    }

    fn post_host_resolution(
        &self,
        browser: BrowserId,
        claim: impl FnOnce() -> Result<(), rust_cef_core::host_capabilities::HostCapabilityError>,
        payload: impl FnOnce() -> TaskPayload,
    ) -> Result<(), rust_cef_core::host_capabilities::HostCapabilityError> {
        use rust_cef_core::host_capabilities::HostCapabilityError;
        self.ensure_capability_active(browser)?;
        claim()?;
        self.executor
            .post(payload())
            .map(|_| ())
            .map_err(|_| HostCapabilityError::PostFailed)
    }

    pub fn start_download(
        &self,
        browser: BrowserId,
        url: impl Into<String>,
    ) -> Result<(), rust_cef_core::host_capabilities::HostCapabilityError> {
        use rust_cef_core::host_capabilities::HostCapabilityError;
        self.ensure_capability_active(browser)?;
        if self.policy.host_capabilities().download.is_none() {
            return Err(HostCapabilityError::CapabilityDisabled);
        }
        let url = url.into();
        if !self.policy.rules.allows_network(&url) {
            self.policy
                .deny(Some(browser), rust_cef_core::ports::PolicyAction::Download);
            return Err(HostCapabilityError::CapabilityDisabled);
        }
        if !self.registry.snapshot(browser).is_ready() {
            return Err(HostCapabilityError::NotReady);
        }
        self.executor
            .post(TaskPayload::StartDownload { browser, url })
            .map(|_| ())
            .map_err(|_| HostCapabilityError::PostFailed)
    }

    pub fn confirm_download(
        &self,
        id: rust_cef_core::host_capabilities::DownloadId,
        destination: PathBuf,
    ) -> Result<(), rust_cef_core::host_capabilities::HostCapabilityError> {
        let roots = self.policy.forbidden_roots();
        let validated =
            rust_cef_core::host_capabilities::validate_download_destination(&destination, &roots)?;
        self.post_host_resolution(
            id.browser,
            || self.registry.host_requests.claim_confirm_download(id),
            move || TaskPayload::ConfirmDownload {
                id,
                destination: validated,
            },
        )
    }

    pub fn cancel_download(
        &self,
        id: rust_cef_core::host_capabilities::DownloadId,
    ) -> Result<(), rust_cef_core::host_capabilities::HostCapabilityError> {
        self.post_host_resolution(
            id.browser,
            || self.registry.host_requests.claim_cancel_download(id),
            move || TaskPayload::CancelDownload { id },
        )
    }

    pub fn resolve_file_dialog(
        &self,
        id: rust_cef_core::host_capabilities::HostRequestId,
        decision: rust_cef_core::host_capabilities::FileDialogDecision,
    ) -> Result<(), rust_cef_core::host_capabilities::HostCapabilityError> {
        self.ensure_capability_active(id.browser)?;
        self.registry
            .host_requests
            .claim_file_dialog(id, &decision)?;
        self.executor
            .post(TaskPayload::ResolveFileDialog { id, decision })
            .map(|_| ())
            .map_err(|_| rust_cef_core::host_capabilities::HostCapabilityError::PostFailed)
    }

    pub fn resolve_js_dialog(
        &self,
        id: rust_cef_core::host_capabilities::HostRequestId,
        decision: rust_cef_core::host_capabilities::JsDialogDecision,
    ) -> Result<(), rust_cef_core::host_capabilities::HostCapabilityError> {
        self.post_host_resolution(
            id.browser,
            || self.registry.host_requests.claim_js_dialog(id),
            move || TaskPayload::ResolveJsDialog { id, decision },
        )
    }

    pub fn resolve_permission(
        &self,
        id: rust_cef_core::host_capabilities::HostRequestId,
        decision: rust_cef_core::host_capabilities::PermissionDecision,
    ) -> Result<(), rust_cef_core::host_capabilities::HostCapabilityError> {
        self.post_host_resolution(
            id.browser,
            || self.registry.host_requests.claim_permission(id),
            move || TaskPayload::ResolvePermission { id, decision },
        )
    }

    pub fn resolve_auth(
        &self,
        id: rust_cef_core::host_capabilities::HostRequestId,
        decision: rust_cef_core::host_capabilities::AuthDecision,
    ) -> Result<(), rust_cef_core::host_capabilities::HostCapabilityError> {
        self.post_host_resolution(
            id.browser,
            || self.registry.host_requests.claim_auth(id),
            move || TaskPayload::ResolveAuth { id, decision },
        )
    }

    pub fn resolve_certificate(
        &self,
        id: rust_cef_core::host_capabilities::HostRequestId,
        decision: rust_cef_core::host_capabilities::CertificateDecision,
    ) -> Result<(), rust_cef_core::host_capabilities::HostCapabilityError> {
        self.post_host_resolution(
            id.browser,
            || self.registry.host_requests.claim_certificate(id, decision),
            move || TaskPayload::ResolveCertificate { id, decision },
        )
    }

    fn ensure_cookies_ready(
        &self,
    ) -> Result<(), rust_cef_core::host_capabilities::HostCapabilityError> {
        if self.finished || self.registry.is_draining() {
            return Err(rust_cef_core::host_capabilities::HostCapabilityError::BrowserClosed);
        }
        if self.state() != RuntimeState::Ready {
            return Err(rust_cef_core::host_capabilities::HostCapabilityError::NotReady);
        }
        Ok(())
    }

    pub fn visit_profile_cookies(
        &self,
        profile: ProfileHandle,
        url: Option<String>,
        include_http_only: bool,
    ) -> Result<crate::CookieVisitTicket, rust_cef_core::host_capabilities::HostCapabilityError>
    {
        self.ensure_cookies_ready()?;
        self.profile_cookies
            .visit_cookies(profile, url, include_http_only)
    }

    pub fn set_profile_cookie(
        &self,
        profile: ProfileHandle,
        url: impl Into<String>,
        cookie: rust_cef_core::host_capabilities::CookieRecord,
    ) -> Result<crate::CookieMutationTicket, rust_cef_core::host_capabilities::HostCapabilityError>
    {
        self.ensure_cookies_ready()?;
        self.profile_cookies.set_cookie(profile, url.into(), cookie)
    }

    pub fn delete_profile_cookies(
        &self,
        profile: ProfileHandle,
        url: Option<String>,
        cookie_name: Option<String>,
    ) -> Result<crate::CookieMutationTicket, rust_cef_core::host_capabilities::HostCapabilityError>
    {
        self.ensure_cookies_ready()?;
        self.profile_cookies
            .delete_cookies(profile, url, cookie_name)
    }

    pub fn clear_profile_http_auth_credentials(
        &self,
        profile: ProfileHandle,
    ) -> Result<crate::ProfileActionTicket, rust_cef_core::host_capabilities::HostCapabilityError>
    {
        self.ensure_cookies_ready()?;
        self.profile_cookies.clear_http_auth_credentials(profile)
    }

    pub fn set_profile_preference(
        &self,
        profile: ProfileHandle,
        name: impl Into<String>,
        value: rust_cef_core::host_capabilities::PreferenceValue,
    ) -> Result<crate::ProfileActionTicket, rust_cef_core::host_capabilities::HostCapabilityError>
    {
        self.ensure_cookies_ready()?;
        self.profile_cookies
            .set_preference(profile, name.into(), value)
    }

    pub fn ax_tree_slot(
        &self,
        browser: BrowserId,
    ) -> Result<rust_cef_core::accessibility::AxTreeSlot, EngineError> {
        if self.finished {
            return Err(EngineError::Closed);
        }
        self.registry.ax_tree_slot(browser)
    }

    pub fn finish_outbound_drag(
        &self,
        browser: BrowserId,
        finish: rust_cef_core::outbound_drag::OutboundDragFinish,
    ) -> Result<rust_cef_core::ports::CommandId, EngineError> {
        if self.finished || self.executor.is_stopped() {
            return Err(EngineError::Closed);
        }
        self.registry
            .submit_outbound_drag_finish(browser, &self.executor, finish)
    }

    pub fn start_url_request(
        &self,
        spec: rust_cef_core::url_request::UrlRequestSpec,
    ) -> Result<crate::UrlRequestTicket, rust_cef_core::url_request::UrlRequestError> {
        use rust_cef_core::url_request::UrlRequestError;
        if self.finished {
            return Err(UrlRequestError::Closing);
        }
        if self.state() != RuntimeState::Ready {
            return Err(UrlRequestError::NotReady);
        }
        let draining = self.registry.is_draining() || self.executor.is_stopped();
        self.url_requests.submit(spec, &self.executor, draining)
    }

    pub fn set_url_request_auth_resolver(&self, resolver: Option<crate::UrlRequestAuthResolver>) {
        if self.finished && resolver.is_some() {
            return;
        }
        self.url_requests.set_auth_resolver(resolver);
    }

    pub fn url_request_pending_count(&self) -> u64 {
        self.url_requests.pending_count()
    }

    pub fn print_to_pdf(
        &self,
        browser: BrowserId,
        destination: PathBuf,
        options: rust_cef_core::host_capabilities::PdfExportOptions,
    ) -> Result<crate::PdfExportTicket, rust_cef_core::host_capabilities::HostCapabilityError> {
        use rust_cef_core::host_capabilities::HostCapabilityError;
        self.ensure_capability_active(browser)?;
        if !self.registry.snapshot(browser).is_ready() {
            return Err(HostCapabilityError::NotReady);
        }
        let roots = self.policy.forbidden_roots();
        let (clean, ticket) =
            self.registry
                .pdf_exports
                .reserve(browser, &destination, &options, &roots)?;
        let id = ticket.id();
        if self
            .executor
            .post(TaskPayload::PrintToPdf {
                id,
                destination: clean,
                options,
            })
            .is_err()
        {
            self.registry
                .pdf_exports
                .rollback_reserve(id, HostCapabilityError::PostFailed);
            return Err(HostCapabilityError::PostFailed);
        }
        Ok(ticket)
    }

    pub fn attach_cdp_observer(
        &self,
        browser: BrowserId,
        observer: Arc<dyn rust_cef_core::host_capabilities::CdpObserver>,
    ) -> Result<(), rust_cef_core::host_capabilities::HostCapabilityError> {
        use rust_cef_core::host_capabilities::HostCapabilityError;
        self.ensure_capability_active(browser)?;
        if !self.registry.is_headless_cdp_enabled(browser)? {
            return Err(HostCapabilityError::CapabilityDisabled);
        }
        self.registry
            .headless_cdp
            .set_pending_observer(browser, observer);
        if self
            .executor
            .post(TaskPayload::AttachCdpObserver { browser })
            .is_err()
        {
            let _ = self.registry.headless_cdp.take_pending_observer(browser);
            return Err(HostCapabilityError::PostFailed);
        }
        Ok(())
    }

    pub fn detach_cdp_observer(
        &self,
        browser: BrowserId,
    ) -> Result<(), rust_cef_core::host_capabilities::HostCapabilityError> {
        self.ensure_capability_active(browser)?;
        if !self.registry.is_headless_cdp_enabled(browser)? {
            return Err(rust_cef_core::host_capabilities::HostCapabilityError::CapabilityDisabled);
        }
        self.executor
            .post(TaskPayload::DetachCdpObserver { browser })
            .map(|_| ())
            .map_err(|_| rust_cef_core::host_capabilities::HostCapabilityError::PostFailed)
    }

    pub fn send_cdp_message(
        &self,
        browser: BrowserId,
        json_message: impl Into<String>,
    ) -> Result<(), rust_cef_core::host_capabilities::HostCapabilityError> {
        use rust_cef_core::host_capabilities::HostCapabilityError;
        self.ensure_capability_active(browser)?;
        if !self.registry.is_headless_cdp_enabled(browser)? {
            return Err(HostCapabilityError::CapabilityDisabled);
        }
        let json_message = json_message.into();
        rust_cef_core::host_capabilities::validate_cdp_json(&json_message)?;
        self.executor
            .post(TaskPayload::SendCdpMessage {
                browser,
                json_message,
            })
            .map(|_| ())
            .map_err(|_| HostCapabilityError::PostFailed)
    }

    pub fn execute_cdp_method(
        &self,
        browser: BrowserId,
        method: impl Into<String>,
        params_json: Option<String>,
    ) -> Result<
        rust_cef_core::host_capabilities::CdpMethodId,
        rust_cef_core::host_capabilities::HostCapabilityError,
    > {
        use rust_cef_core::host_capabilities::HostCapabilityError;
        self.ensure_capability_active(browser)?;
        if !self.registry.is_headless_cdp_enabled(browser)? {
            return Err(HostCapabilityError::CapabilityDisabled);
        }
        let method = method.into();
        rust_cef_core::host_capabilities::validate_cdp_method_name(&method)?;
        if let Some(raw) = params_json.as_deref() {
            rust_cef_core::host_capabilities::validate_cdp_json(raw)?;
            if !raw.trim().starts_with('{') {
                return Err(HostCapabilityError::InvalidCdpPayload);
            }
        }
        let id = self.registry.headless_cdp.alloc_message_id(browser)?;
        self.executor
            .post(TaskPayload::ExecuteCdpMethod {
                id,
                method,
                params_json,
            })
            .map(|_| id)
            .map_err(|_| HostCapabilityError::PostFailed)
    }

    pub fn cdp_diagnostics(&self) -> crate::CdpDiagnostics {
        self.registry.headless_cdp.diagnostics()
    }

    pub fn configure_source_views(
        &self,
        limits: rust_cef_core::source_view::SourceViewLimits,
    ) -> Result<(), EngineError> {
        self.registry.configure_source_views(limits)
    }
    pub fn source_view_diagnostics(&self) -> crate::SourceViewDiagnostics {
        self.registry.sources.snapshot()
    }

    #[cfg(feature = "test-support")]
    pub fn probe_hold_source_visitor(&self, browser: BrowserId) -> Result<(), EngineError> {
        if browser.runtime() != self.generation() {
            return Err(EngineError::WrongRuntime);
        }
        if self.finished
            || !self.registry.snapshot(browser).is_ready()
            || self.registry.is_draining()
        {
            return Err(EngineError::Closed);
        }
        self.registry.sources.arm_visitor_probe(browser)
    }

    #[cfg(feature = "test-support")]
    pub fn probe_held_source_visitor(
        &self,
    ) -> Option<rust_cef_core::source_view::SourceOperationId> {
        self.registry.sources.held_visitor_probe()
    }

    #[cfg(feature = "test-support")]
    pub fn probe_release_source_visitor(
        &self,
        id: rust_cef_core::source_view::SourceOperationId,
    ) -> Result<(), EngineError> {
        if self.finished {
            return Err(EngineError::Closed);
        }
        self.registry.sources.reserve_visitor_release_probe(id)?;
        if let Err(error) = self
            .executor
            .post_close(TaskPayload::ProbeReleaseSourceVisitor(id))
        {
            self.registry.sources.rollback_visitor_release_probe(id);
            return Err(error);
        }
        Ok(())
    }

    pub fn configure_context_menus(
        &self,
        config: crate::ContextMenuConfig,
    ) -> Result<(), EngineError> {
        self.registry.configure_context_menus(config)
    }
    pub fn bind_context_menu(
        &self,
        target: rust_cef_core::ports::ContextMenuTarget,
    ) -> Result<crate::ContextMenuRegistration, EngineError> {
        self.registry.bind_context_menu(target)
    }

    pub fn create_browser(
        &mut self,
        config: BrowserConfig,
    ) -> Result<BrowserHandle, BrowserCreateError> {
        self.create_browser_with_post(config, |executor, browser, config| {
            executor.post(TaskPayload::Create { browser, config })
        })
    }

    /// Test-only fixed transparent windowless background. All original create,
    /// policy/profile/admission and closing paths remain shared.
    #[cfg(feature = "test-support")]
    pub fn probe_create_capture_browser(
        &mut self,
        config: BrowserConfig,
    ) -> Result<BrowserHandle, BrowserCreateError> {
        self.create_browser_with_post(config, |executor, browser, config| {
            executor.post(TaskPayload::CreateCaptureProbe { browser, config })
        })
    }

    /// Inject device creation failure for one accepted D3d11Required create
    /// task. Uses the real capture preparation and creation rejection paths.
    #[cfg(feature = "test-support")]
    pub fn probe_create_with_device_failure(
        &mut self,
        config: BrowserConfig,
    ) -> Result<BrowserHandle, BrowserCreateError> {
        if config.render_preference != RenderPreference::D3d11Required {
            return Err(BrowserCreateError::InvalidConfig(
                "device failure requires D3d11Required",
            ));
        }
        self.create_browser_with_post(config, |executor, browser, config| {
            executor.post(TaskPayload::CreateDeviceFailureProbe { browser, config })
        })
    }

    /// Refuse the executor post for this creation only. No native create is issued.
    #[cfg(feature = "test-support")]
    pub fn probe_create_with_refused_post(
        &mut self,
        config: BrowserConfig,
    ) -> Result<BrowserHandle, BrowserCreateError> {
        self.create_browser_with_post(config, Executor::probe_reject_create_post)
    }

    /// On CEF UI, register close intent immediately after a genuine accepted
    /// create request and let the real OnAfterCreated/OnBeforeClose run.
    #[cfg(feature = "test-support")]
    pub fn probe_create_then_close(
        &mut self,
        config: BrowserConfig,
    ) -> Result<BrowserHandle, BrowserCreateError> {
        self.create_browser_with_post(config, |executor, browser, config| {
            executor.post(TaskPayload::CreateThenCloseProbe { browser, config })
        })
    }

    #[cfg(feature = "test-support")]
    pub fn probe_late_create_observed(&self, browser: BrowserId) -> bool {
        self.registry.probe_late_create_observed(browser) == Some(true)
    }

    fn create_browser_with_post(
        &mut self,
        config: BrowserConfig,
        post: impl FnOnce(&Executor, BrowserId, BrowserConfig) -> Result<TaskId, EngineError>,
    ) -> Result<BrowserHandle, BrowserCreateError> {
        if config.initial_url.trim().is_empty() {
            return Err(BrowserCreateError::InvalidConfig("initial_url is empty"));
        }
        config
            .options
            .surface_mode
            .validate_for_runtime(self.config.settings.windowless_rendering_enabled)
            .map_err(BrowserCreateError::Core)?;
        match config.options.surface_mode {
            BrowserSurfaceMode::Offscreen => {
                if config.viewport.width <= 0 || config.viewport.height <= 0 {
                    return Err(BrowserCreateError::InvalidConfig(
                        "viewport must have positive dimensions",
                    ));
                }
                if crate::client::pixel_extent(
                    config.viewport.width,
                    config.viewport.height,
                    config.viewport.scale,
                )
                .is_err()
                {
                    return Err(BrowserCreateError::InvalidConfig(
                        "invalid or oversized pixel viewport",
                    ));
                }
                match config.render_preference {
                    RenderPreference::SoftwareRequired | RenderPreference::D3d11Required => {}
                    // Selection is explicit: no automatic software fallback is implemented.
                    RenderPreference::Auto => {
                        return Err(BrowserCreateError::UnsupportedBackend(
                            config.render_preference,
                        ));
                    }
                }
            }
            BrowserSurfaceMode::NativeChildHwnd(native_cfg) => {
                if !crate::native_hwnd::is_valid_window(native_cfg.parent_hwnd) {
                    return Err(BrowserCreateError::Core(CreateError::InvalidOptions));
                }
            }
        }
        if self.state() != RuntimeState::Ready || self.registry.is_draining() {
            return Err(BrowserCreateError::Core(CreateError::NotReady));
        }
        if !self
            .policy
            .rules
            .allows_host_navigation(&config.initial_url)
        {
            self.policy
                .deny(None, rust_cef_core::ports::PolicyAction::Navigation);
            return Err(BrowserCreateError::PolicyDenied);
        }
        // `None` and an explicit default handle are the same reference: both
        // resolve to the one selected default context. A bad explicit handle is
        // rejected here and never replaced by the default (no fallback).
        let profile = match self.profiles.select(config.profile) {
            Ok(profile) => profile,
            Err(ProfileCreateError::Profile(error)) => {
                return Err(BrowserCreateError::Profile(error));
            }
            Err(_) => return Err(BrowserCreateError::Profile(ProfileError::NotReady)),
        };
        let mut config = config;
        config.profile = Some(profile);
        let local = self.next_browser_local();
        let browser = BrowserId::new(self.generation(), local);
        self.registry
            .begin_create(browser, config.clone())
            .map_err(BrowserCreateError::Core)?;
        if let Err(error) = post(&self.executor, browser, config) {
            self.registry.reject_create(browser, error.clone());
            return Err(BrowserCreateError::PostFailed(error));
        }
        let control =
            RuntimeBrowserControl::new(browser, self.executor.clone(), self.registry.clone());
        Ok(BrowserHandle::new(control))
    }

    pub fn frame_recovery_diagnostics(
        &self,
        browser: BrowserId,
    ) -> Result<FrameRecoveryDiagnostics, EngineError> {
        self.registry.recovery_diagnostics(browser)
    }
    #[cfg(feature = "test-support")]
    pub fn probe_frame_control(
        &self,
        browser: BrowserId,
        control: crate::test_support::FrameProbeControl,
    ) -> Result<(), EngineError> {
        if self.finished || !self.registry.snapshot(browser).is_ready() {
            return Err(EngineError::Closed);
        }
        self.executor
            .post(TaskPayload::ProbeFrameControl { browser, control })
            .map(|_| ())
    }
    #[cfg(feature = "test-support")]
    pub fn probe_frame_state(
        &self,
        browser: BrowserId,
    ) -> Option<crate::test_support::FrameProbeState> {
        self.registry.frame_probe_state(browser)
    }

    #[cfg(feature = "test-support")]
    fn probe_renderer_action(
        &self,
        browser: BrowserId,
        action: crate::renderer_probe::RendererProbeAction,
    ) -> Result<(), EngineError> {
        if self.finished || !self.registry.snapshot(browser).is_ready() {
            return Err(EngineError::Closed);
        }
        self.executor
            .post(TaskPayload::ProbeRendererControl { browser, action })
            .map(|_| ())
    }
    /// Owned capture diagnostics remain queryable after this Browser closes.
    /// Requested backend/device creation alone never records an observed frame.
    pub fn frame_capture_diagnostics(
        &self,
        browser: BrowserId,
    ) -> Result<CaptureDiagnostics, EngineError> {
        self.registry.capture_diagnostics(browser)
    }

    /// Test-only fixed-capacity sampling of one live Browser's real VIEW input copies.
    #[cfg(feature = "test-support")]
    pub fn probe_arm_view_copy_timing(&self, browser: BrowserId) -> Result<(), EngineError> {
        if self.finished {
            return Err(EngineError::Closed);
        }
        self.registry.probe_arm_view_copy_timing(browser)
    }

    /// Owned diagnostic copy, also available after retirement. No native call.
    #[cfg(feature = "test-support")]
    pub fn probe_view_copy_samples(
        &self,
        browser: BrowserId,
    ) -> Option<Vec<crate::test_support::ViewCopySample>> {
        self.registry.probe_view_copy_samples(browser)
    }

    /// Test-only preflight; successful post is not a successful method reply.
    #[cfg(feature = "test-support")]
    pub fn probe_renderer_enable(&self, browser: BrowserId) -> Result<(), EngineError> {
        self.probe_renderer_action(browser, crate::renderer_probe::RendererProbeAction::Enable)
    }
    #[cfg(feature = "test-support")]
    pub fn probe_renderer_enabled(&self, browser: BrowserId) -> Option<bool> {
        self.registry.probe_renderer_enabled(browser)
    }
    /// Test-only fixed Page.crash; actual termination callback is mandatory proof.
    #[cfg(feature = "test-support")]
    pub fn probe_renderer_crash(&self, browser: BrowserId) -> Result<(), EngineError> {
        self.probe_renderer_action(browser, crate::renderer_probe::RendererProbeAction::Crash)
    }
    #[cfg(feature = "test-support")]
    pub fn probe_renderer_crash_submitted(&self, browser: BrowserId) -> bool {
        self.registry.probe_renderer_crash_submitted(browser)
    }
    #[cfg(feature = "test-support")]
    pub fn probe_renderer_references(&self) -> usize {
        self.registry.probe_renderer_references()
    }

    /// Test-only raw native navigation WITHOUT granting host data approval.
    /// Exercises the real CEF callback, not renderer navigation restrictions.
    /// This is not a stable component command or a user input/activation API.
    #[cfg(feature = "test-support")]
    pub fn probe_unapproved_data_navigation(
        &self,
        browser: BrowserId,
        target: String,
    ) -> Result<(), EngineError> {
        if self.finished || !self.registry.snapshot(browser).is_ready() {
            return Err(EngineError::Closed);
        }
        if !target.starts_with("data:") || target.len() > 16384 {
            return Err(EngineError::InvalidRequest);
        }
        self.executor
            .post(TaskPayload::ProbeDataNavigation { browser, target })
            .map(|_| ())
    }
    /// Explicit windowless callback fixture only: synthetic CEF right mouse,
    /// not Win32/GPUI input and never a request to activate an OS window.
    #[cfg(feature = "test-support")]
    pub fn probe_menu_spelling_open(
        &self,
        browser: BrowserId,
        dictionary: bool,
    ) -> Result<(), EngineError> {
        self.executor
            .post(TaskPayload::ProbeMenuSpellingOpen {
                browser,
                dictionary,
            })
            .map(|_| ())
    }
    #[cfg(feature = "test-support")]
    pub fn probe_menu_find_open(&self, browser: BrowserId) -> Result<(), EngineError> {
        self.executor
            .post(TaskPayload::ProbeMenuFindOpen { browser })
            .map(|_| ())
    }
    #[cfg(feature = "test-support")]
    pub fn probe_menu_source_open(&self, browser: BrowserId) -> Result<(), EngineError> {
        self.executor
            .post(TaskPayload::ProbeMenuSourceOpen { browser })?;
        Ok(())
    }
    #[cfg(feature = "test-support")]
    pub fn probe_menu_bridge_open(
        &self,
        browser: BrowserId,
        print: bool,
    ) -> Result<(), EngineError> {
        self.executor
            .post(TaskPayload::ProbeMenuBridgeOpen { browser, print })?;
        Ok(())
    }
    /// Native OSR fixture: prepare and drop the original menu's Context/client
    /// without issuing a DevTools operation. Normal policy stays disabled.
    #[cfg(feature = "test-support")]
    pub fn probe_prepare_menu_auxiliary(
        &self,
        request: rust_cef_core::menu::ContextMenuRequestId,
        binding: rust_cef_core::ports::WindowBinding,
        revision: u64,
    ) -> Result<crate::test_support::AuxiliaryPreparationTicket, EngineError> {
        if self.finished {
            return Err(EngineError::Closed);
        }
        self.registry
            .post_auxiliary_preparation_probe(request, binding, revision)
    }

    #[cfg(feature = "test-support")]
    pub fn probe_menu_open(&self, browser: BrowserId) -> Result<(), EngineError> {
        if self.finished || !self.registry.snapshot(browser).is_ready() {
            return Err(EngineError::Closed);
        }
        self.executor
            .post(TaskPayload::ProbeMenuOpen { browser })
            .map(|_| ())
    }
    /// Advance only the fixed pressure fixture's title. This accepts no script
    /// body and is not a production execution or event injection API.
    #[cfg(feature = "test-support")]
    pub fn probe_pressure_title(&self, browser: BrowserId, step: u16) -> Result<(), EngineError> {
        if self.finished {
            return Err(EngineError::Closed);
        }
        if step == 0 || usize::from(step) > crate::test_support::PRESSURE_EVENT_CAPACITY + 1 {
            return Err(EngineError::InvalidRequest);
        }
        self.executor
            .post(TaskPayload::ProbePressureTitle { browser, step })
            .map(|_| ())
    }

    /// Windowless test-only exercise of close admission beside a full normal lane.
    #[cfg(feature = "test-support")]
    pub fn probe_command_pressure_close(&self, browser: BrowserId) -> Result<(), EngineError> {
        if self.finished {
            return Err(EngineError::Closed);
        }
        self.registry.post_command_pressure_close(
            browser,
            &self.executor,
            crate::test_support::PressureExpectation::ProbeMenu(None),
        )
    }
    /// Test only: qualify the original cookie completion at Close admission
    /// while the ordinary command and native menu lanes are still full/pending.
    #[cfg(feature = "test-support")]
    pub fn probe_command_pressure_close_with_flush(
        &self,
        browser: BrowserId,
        profile: ProfileHandle,
        sequence: u64,
    ) -> Result<(), EngineError> {
        if self.finished {
            return Err(EngineError::Closed);
        }
        self.registry.post_command_pressure_close(
            browser,
            &self.executor,
            crate::test_support::PressureExpectation::ProbeMenu(Some((profile, sequence))),
        )
    }
    /// Test only: submit and close a Browser under command pressure without a menu.
    #[cfg(feature = "test-support")]
    pub fn probe_command_pressure_close_without_menu(
        &self,
        browser: BrowserId,
    ) -> Result<(), EngineError> {
        if self.finished {
            return Err(EngineError::Closed);
        }
        self.registry.post_command_pressure_close(
            browser,
            &self.executor,
            crate::test_support::PressureExpectation::NoMenu,
        )
    }

    /// Test only: fill the ordinary lane beside one original managed window menu.
    #[cfg(feature = "test-support")]
    pub fn probe_command_pressure_close_with_window_menu(
        &self,
        request: rust_cef_core::menu::ContextMenuRequestId,
        gpui_window: u64,
        peer_flush: Option<crate::test_support::WindowPeerFlush>,
        source: Option<rust_cef_core::source_view::SourceOperationId>,
        find: Option<rust_cef_core::find::FindOperationId>,
        prints: Option<crate::test_support::WindowPrintCut>,
    ) -> Result<(), EngineError> {
        if self.finished {
            return Err(EngineError::Closed);
        }
        if gpui_window == 0
            || request.sequence == 0
            || find.is_some_and(|id| {
                source.is_none() || peer_flush.is_none_or(|f| f.browser != id.browser)
            })
            || prints.is_some_and(|p| {
                p.ids[0].browser != request.browser
                    || p.windows[0] != gpui_window
                    || p.windows[1] == 0
                    || p.windows[0] == p.windows[1]
                    || p.ids[0].browser == p.ids[1].browser
                    || peer_flush.is_none_or(|f| f.browser != p.ids[1].browser)
            })
        {
            return Err(EngineError::InvalidRequest);
        }
        self.registry.post_command_pressure_close(
            request.browser,
            &self.executor,
            crate::test_support::PressureExpectation::WindowMenu(
                request,
                gpui_window,
                peer_flush,
                source,
                find,
                prints,
            ),
        )
    }

    /// Test only: exact original managed menu, Source, Find and flush at the pressured close.
    #[cfg(feature = "test-support")]
    pub fn probe_command_pressure_close_with_operations(
        &self,
        cut: crate::test_support::ManagedPressureCut,
    ) -> Result<(), EngineError> {
        if self.finished {
            return Err(EngineError::Closed);
        }
        self.registry.post_command_pressure_close(
            cut.menu.browser,
            &self.executor,
            crate::test_support::PressureExpectation::ManagedMenu(cut),
        )
    }
    #[cfg(feature = "test-support")]
    pub fn probe_command_pressure_status(
        &self,
        browser: BrowserId,
    ) -> Option<crate::test_support::CommandPressure> {
        self.registry.command_pressure_status(browser)
    }
    #[cfg(feature = "test-support")]
    pub fn probe_menu_deferred_enable(
        &self,
        browser: BrowserId,
        lease: rust_cef_core::menu_lease::MenuLease,
    ) -> Result<(), EngineError> {
        self.registry.probe_menu_deferred_enable(browser, lease)
    }
    #[cfg(feature = "test-support")]
    pub fn probe_menu_cancel(&self, browser: BrowserId) -> Result<(), EngineError> {
        self.registry.probe_menu_cancel(browser)
    }
    #[cfg(feature = "test-support")]
    pub fn probe_menu_status(
        &self,
        browser: BrowserId,
    ) -> Option<crate::test_support::MenuProbeSnapshot> {
        self.registry.probe_menu_status(browser)
    }
    /// Test-host bridge: forwards only the visible fixture's already observed
    /// Win32 mouse input. Public component input is implemented separately in 11.
    #[cfg(feature = "test-support")]
    pub fn forward_probe_mouse(
        &self,
        browser: BrowserId,
        x: i32,
        y: i32,
        up: bool,
    ) -> Result<(), EngineError> {
        if self.finished || !self.registry.snapshot(browser).is_ready() {
            return Err(EngineError::Closed);
        }
        self.executor
            .post(TaskPayload::ProbeMouse { browser, x, y, up })
            .map(|_| ())
    }

    /// Narrow native reset driver for the fixture only. No generic protocol or
    /// remote-debugging endpoint is exposed by the default Runtime API.
    #[cfg(feature = "test-support")]
    pub fn probe_dialog_control(
        &self,
        browser: BrowserId,
        action: crate::dialog_probe::DialogProbeAction,
    ) -> Result<(), EngineError> {
        if self.finished || !self.registry.snapshot(browser).is_ready() {
            return Err(EngineError::Closed);
        }
        self.executor
            .post(TaskPayload::ProbeDialogControl { browser, action })
            .map(|_| ())
    }
    #[cfg(feature = "test-support")]
    pub fn probe_dialog_status(
        &self,
        browser: BrowserId,
        action: crate::dialog_probe::DialogProbeAction,
    ) -> Option<bool> {
        self.registry.probe_dialog_status(browser, action)
    }
    #[cfg(feature = "test-support")]
    pub fn probe_dialog_references(&self) -> usize {
        self.registry.probe_dialog_references()
    }
    #[cfg(feature = "test-support")]
    pub fn probe_dialog_reset_count(&self, browser: BrowserId) -> u64 {
        self.registry.probe_dialog_reset_count(browser)
    }

    fn next_browser_local(&mut self) -> u64 {
        // Session entries are never removed; a monotonic local id avoids reusing
        // identity after a failed or closed generation.
        self.next_local = self.next_local.wrapping_add(1);
        self.next_local
    }

    /// One non-reentrant manual pump step. Threaded mode never calls this and
    /// returns WrongThread without touching CEF.
    pub fn pump(&mut self) -> Result<u32, EngineError> {
        if self.finished {
            return Err(EngineError::Closed);
        }
        if self.config.thread_mode != ThreadMode::Integrated {
            return Err(EngineError::WrongThread);
        }
        self.audit.record();
        do_message_loop_work();
        self.manual_pump_count = self.manual_pump_count.wrapping_add(1);
        Ok(1)
    }

    pub fn request_shutdown(&mut self, mode: CloseMode, deadline: Instant) -> ShutdownTicket {
        let first = self.shutdown_ticket.is_none();
        let ticket = *self.shutdown_ticket.get_or_insert(ShutdownTicket {
            runtime: self.generation(),
            sequence: 1,
        });
        if self.finished {
            // Refused before any native entry: CEF is already shut down.
            return ticket;
        }
        // The core barrier records the deadline once; a repeated request (for
        // example a Force escalation) keeps the original diagnostic deadline.
        let _ = self.barrier.request(deadline);
        self.shutdown_deadline.get_or_insert(deadline);
        self.registry.set_draining();
        let boundary_pending = self.executor.begin_drain();
        if first {
            self.pending_tasks_at_shutdown = boundary_pending;
        }
        for browser in self.registry.open_browsers() {
            self.registry.close_browser(browser, mode);
        }
        // Close scheduling goes through the UI payload; a rejected post leaves
        // the continuations pending and is reported, never faked as cancelled.
        // A repeated request (Force escalation) must not add a second drain task.
        if first {
            let _ = self.registry.post_drain_requests();
        }
        ticket
    }

    pub fn poll_shutdown(&mut self, ticket: &ShutdownTicket) -> ShutdownStatus {
        if self.shutdown_ticket.is_none()
            || ticket.runtime != self.generation()
            || ticket.sequence != 1
        {
            return ShutdownStatus::NotRequested;
        }
        if self.finished {
            return ShutdownStatus::Stopped;
        }
        let now = Instant::now();
        let decision = self.observe_shutdown(now);
        self.status_from(decision, now)
    }

    fn observe_shutdown(&mut self, now: Instant) -> ShutdownDecision {
        self.barrier.observe(
            &self.registry.snapshots(),
            self.executor.pending(),
            self.registry.pending_requests(),
            now,
        )
    }

    /// Profile initialization and in-flight handler callbacks are a separate
    /// native gate. They are never folded into `pending_tasks`; the core barrier
    /// keeps its original browser/task/request semantics.
    fn profile_gate(&self) -> u64 {
        self.profiles
            .pending_profiles()
            .saturating_add(self.profiles.callbacks_in_flight())
            .saturating_add(self.profile_cookies.pending())
    }

    fn status_from(&self, decision: ShutdownDecision, now: Instant) -> ShutdownStatus {
        let pending_profiles = self.profile_gate();
        let timed_out = self
            .shutdown_deadline
            .is_some_and(|deadline| now >= deadline);
        shutdown_status_with_flushes(
            decision,
            self.executor.stats().post_failed,
            pending_profiles,
            self.flushes.pending(),
            timed_out,
        )
    }

    /// The only place that calls `cef::shutdown`, exactly once, and only after
    /// every Browser closed, every accepted task finished, native references
    /// were really released, and every pending continuation was terminated. A
    /// blocked barrier returns an error instead of shutting down early.
    pub fn finish_shutdown(&mut self) -> Result<ShutdownCompletion, ShutdownError> {
        if self.shutdown_ticket.is_none() {
            return Err(ShutdownError::NotRequested);
        }
        if self.finished {
            return Err(ShutdownError::AlreadyFinished);
        }
        let actual = current_thread_id();
        if actual != self.main_thread_id {
            return Err(ShutdownError::WrongThread {
                expected: self.main_thread_id,
                actual,
            });
        }
        let now = Instant::now();
        let decision = self.observe_shutdown(now);
        let post_failures = self.executor.stats().post_failed;
        let pending_profiles = self.profile_gate();
        let blocked =
            |open_browsers: Vec<BrowserId>, pending_tasks, pending_requests, pending_profiles| {
                ShutdownError::Blocked {
                    open_browsers,
                    pending_tasks,
                    pending_requests,
                    pending_profiles,
                    pending_cookie_flushes: self.flushes.pending(),
                    post_failures,
                }
            };
        match decision {
            ShutdownDecision::Ready => {}
            ShutdownDecision::NotRequested => return Err(ShutdownError::NotRequested),
            ShutdownDecision::ProcessExitRequired { exit_code } => {
                return Err(ShutdownError::ProcessExitRequired { exit_code });
            }
            ShutdownDecision::Draining {
                open_browsers,
                pending_tasks,
                pending_requests,
            }
            | ShutdownDecision::Blocked {
                open_browsers,
                pending_tasks,
                pending_requests,
            } => {
                return Err(blocked(
                    open_browsers,
                    pending_tasks,
                    pending_requests,
                    pending_profiles,
                ));
            }
            ShutdownDecision::Stopped => return Err(ShutdownError::AlreadyFinished),
        }
        // Core readiness is a snapshot; the adapter still proves the real
        // native table is empty, no profile initialization/callback is in
        // flight, and seals admission without blocking. A profile gate hit is
        // reported as Blocked, never folded into the task count.
        if pending_profiles != 0
            || self.flushes.pending() != 0
            || !self.registry.native_refs_clear()
            || !self.executor.try_seal_if_idle()
        {
            return Err(blocked(
                self.registry.open_browsers(),
                self.executor.pending(),
                self.registry.pending_requests(),
                pending_profiles,
            ));
        }
        // All CEF references are released on this owner thread, before the
        // single shutdown call. Contexts are released before the App wrapper;
        // no UI retirement task exists, so nothing can try to post after seal.
        let released_browsers = self.registry.snapshot_count();
        self.policy.clear_interceptor();
        self.policy.clear_host_capabilities();
        self.audit.record();
        let on_ui = cef::currently_on(cef::ThreadId::UI) != 0;
        self.registry.release_all_refs(on_ui);
        self.registry.pdf_exports.close_all();
        self.flushes.close();
        self.profile_cookies.close();
        self.url_requests.clear_auth_resolver();
        self.profiles.seal_and_release();
        debug_assert_eq!(
            self.profiles.references_retained(),
            0,
            "every profile reference must be released before cef::shutdown"
        );
        // Our App reference must be gone before the one shutdown call; CEF
        // keeps its own reference for the browser process lifetime.
        self.app.release_before_cef_shutdown();
        // Counted immediately before the real call only; the barrier itself
        // cannot prove this and diagnostics must not imply otherwise.
        self.cef_shutdown_calls = self.cef_shutdown_calls.wrapping_add(1);
        self.audit.record();
        shutdown();
        // Every public entry point now refuses before touching CEF; a call
        // recorded after this point is a real violation, not a rejected attempt.
        self.audit.shutdown_returned();
        self.root_lease.release();
        let _ = self.barrier.finish();
        self.shutdown_thread_id = Some(actual);
        self.finished = true;
        Ok(ShutdownCompletion {
            shutdown_thread_id: actual,
            released_browsers,
        })
    }

    /// Metadata-only print confirmations for the real shutdown barrier probe.
    /// Does not register a menu command, native printer, or confirmation UI.
    #[cfg(feature = "test-support")]
    pub fn probe_arm_prints(
        &self,
        owners: [BrowserId; 2],
    ) -> Result<[rust_cef_core::print::PrintId; 2], EngineError> {
        self.registry
            .probe_arm_prints(owners.map(|browser| rust_cef_core::ports::WindowBinding {
                browser,
                window: browser.local(),
            }))
    }

    /// Metadata confirmation rows for the two originally owned timing windows.
    #[cfg(feature = "test-support")]
    pub fn probe_arm_window_print_pair(
        &self,
        owners: [rust_cef_core::ports::WindowBinding; 2],
    ) -> Result<crate::test_support::WindowPrintCut, EngineError> {
        let ids = self.registry.probe_arm_prints(owners)?;
        Ok(crate::test_support::WindowPrintCut {
            ids,
            windows: owners.map(|p| p.window),
        })
    }

    /// One metadata-only confirmation presented by the validation host in its
    /// original GPUI window. The binding is not a managed provider registration.
    #[cfg(feature = "test-support")]
    pub fn probe_arm_print_panel(
        &self,
        binding: rust_cef_core::ports::WindowBinding,
    ) -> Result<rust_cef_core::print::PrintId, EngineError> {
        self.registry.probe_arm_prints([binding]).map(|[id]| id)
    }

    #[cfg(feature = "test-support")]
    pub fn probe_print_status(
        &self,
        id: rust_cef_core::print::PrintId,
    ) -> Option<rust_cef_core::print::PrintStatus> {
        self.registry.prints.probe_status(id)
    }

    #[cfg(feature = "test-support")]
    pub fn probe_print_cancelled(
        &self,
        id: rust_cef_core::print::PrintId,
    ) -> Result<(), EngineError> {
        self.registry.prints.probe_cancelled(id)
    }

    #[cfg(feature = "test-support")]
    pub fn probe_print_retired(
        &self,
        id: rust_cef_core::print::PrintId,
    ) -> Result<rust_cef_core::print::PrintStatus, EngineError> {
        self.registry.prints.probe_retired(id)
    }

    /// Test-support seam for 5.3: register a real pending native continuation.
    /// Rejected once drainage begins; it is never reported as cancelled.
    #[cfg(feature = "test-support")]
    pub fn inject_pending_request<F>(
        &self,
        browser: BrowserId,
        cancel: F,
    ) -> Result<RequestId, EngineError>
    where
        F: FnOnce() + Send + 'static,
    {
        self.registry.inject_request(browser, cancel)
    }

    /// Test-support fault gate: keep the close intent/tombstone without issuing
    /// the native close, for a reproducible timeout scenario. Clearing it posts
    /// a UI close task; it never calls CEF from the caller thread.
    #[cfg(feature = "test-support")]
    pub fn set_close_deferred(
        &self,
        browser: BrowserId,
        deferred: bool,
    ) -> Result<(), EngineError> {
        self.registry.set_close_deferred(browser, deferred)
    }

    pub fn diagnostics(&self) -> RuntimeDiagnostics {
        let (_, context_thread_id) = self.startup.snapshot();
        let tasks = self.executor.stats();
        let (native_calls, native_calls_after_shutdown) = self.audit.snapshot();
        RuntimeDiagnostics {
            mode: self.config.thread_mode,
            state: self.state(),
            main_thread_id: self.main_thread_id,
            init_thread_id: self.init_thread_id,
            context_thread_id,
            cef_ui_thread_id: tasks.ui_execute_thread_id,
            shutdown_thread_id: self.shutdown_thread_id,
            manual_pump_count: self.manual_pump_count,
            observed_backend: self.registry.observed_backend(),
            app_reference_retained: self.app.is_retained(),
            cef_shutdown_calls: self.cef_shutdown_calls,
            pending_tasks_at_shutdown: self.pending_tasks_at_shutdown,
            native_calls,
            native_calls_after_shutdown,
            pending_requests: self.registry.pending_requests(),
            cancelled_requests: self.registry.cancelled_requests(),
            before_unload_requested: self.registry.dialog_stats().0,
            before_unload_answered: self.registry.dialog_stats().1,
            before_unload_invalidated: self.registry.dialog_stats().2,
            before_unload_timeouts: self.registry.dialog_stats().3,
            pending_profiles: self.profiles.pending_profiles(),
            profile_references_retained: self.profiles.references_retained(),
            pending_cookie_flushes: self.flushes.pending(),
            cookie_flushes_accepted: self.flushes.accepted(),
            cookie_flushes_completed: self.flushes.completed_count(),
            root_lease_held: self.root_lease.is_held(),
            policy_denied: self.policy.snapshot().0,
            policy_unattributed_denials: self.policy.snapshot().1,
            policy_factory_without_browser: self.policy.factory_without_browser(),
            single_process_enabled: match self.single_process.load(Ordering::Acquire) {
                SINGLE_PROCESS_DISABLED => Some(false),
                SINGLE_PROCESS_ENABLED => Some(true),
                SINGLE_PROCESS_UNOBSERVED => None,
                // 0 is the only "never observed" value, but an unknown byte must
                // still never be reported as an observed "false".
                _ => None,
            },
            single_process_disabled: match self.single_process.load(Ordering::Acquire) {
                SINGLE_PROCESS_DISABLED => Some(true),
                SINGLE_PROCESS_ENABLED => Some(false),
                SINGLE_PROCESS_UNOBSERVED => None,
                _ => None,
            },
            tasks,
        }
    }

    pub fn observed_backend(&self) -> Option<RenderPreference> {
        self.registry.observed_backend()
    }

    pub fn frame_count(&self) -> u64 {
        self.registry.frame_count()
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Never run CEF teardown from Drop. A failed barrier leaves native
        // owners alone (the probe intentionally leaks the Runtime) and the OS
        // reclaims them when the process exits. `AppOwner::drop` leaks our App
        // reference in that case; `lease` keeps its committed OS handle.
        let _ = &self.lease;
        let _ = &self.services;
        if self.finished {
            self.registry.release_all_refs(false);
            self.profiles.seal_and_release();
        } else {
            // Dropping the registry would release CEF objects while CEF is still
            // initialized. Leak those native references instead; the process is
            // required to exit without another shutdown call. The same applies
            // to every profile context/handler reference.
            self.registry.abandon_native_refs();
            self.profiles.abandon_all();
            self.root_lease.abandon();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::{
        contracts::{RuntimeFiles, TrustedContentPolicy},
        ports::{BrowserEventEnvelope, OwnedFrame, PublishResult},
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn canonicalize_verbatim_prefix_is_stripped_for_cef() {
        assert_eq!(dos_path(Path::new(r"\\?\C:\cache")), r"C:\cache");
        assert_eq!(
            dos_path(Path::new(r"\\?\UNC\server\share")),
            r"\\server\share"
        );
        assert_eq!(dos_path(Path::new(r"C:\already\dos")), r"C:\already\dos");
    }

    #[test]
    fn blocked_status_exposes_pending_requests_without_authorizing_shutdown() {
        let browser = BrowserId::new(RuntimeGeneration::new(1), 3);
        let status = shutdown_status(
            ShutdownDecision::Blocked {
                open_browsers: vec![browser],
                pending_tasks: 2,
                pending_requests: 1,
            },
            0,
            0,
            true,
        );
        assert_eq!(
            status,
            ShutdownStatus::Blocked {
                open_browsers: vec![browser],
                pending_tasks: 2,
                pending_requests: 1,
                pending_profiles: 0,
                pending_cookie_flushes: 0,
                post_failures: 0,
            }
        );
        assert_eq!(
            shutdown_status(
                ShutdownDecision::Draining {
                    open_browsers: vec![],
                    pending_tasks: 0,
                    pending_requests: 1,
                },
                0,
                0,
                false,
            ),
            ShutdownStatus::Draining {
                open_browsers: vec![],
                pending_tasks: 0,
                pending_requests: 1,
                pending_profiles: 0,
                pending_cookie_flushes: 0,
                post_failures: 0,
            }
        );
        assert_eq!(
            shutdown_status(
                ShutdownDecision::ProcessExitRequired { exit_code: Some(7) },
                0,
                0,
                false,
            ),
            ShutdownStatus::ProcessExitRequired { exit_code: Some(7) }
        );
    }

    #[test]
    fn profile_gate_never_reports_ready_without_authorizing_shutdown() {
        // Core says Ready (no browsers/tasks/requests) but a profile
        // initialization is still pending: the native projection must stay
        // Draining, with the profile count kept separate from task counts.
        let status = shutdown_status(ShutdownDecision::Ready, 0, 1, false);
        assert_eq!(
            status,
            ShutdownStatus::Draining {
                open_browsers: vec![],
                pending_tasks: 0,
                pending_requests: 0,
                pending_profiles: 1,
                pending_cookie_flushes: 0,
                post_failures: 0,
            }
        );
        // The deadline does not convert a live profile into permission to
        // shut down; it only becomes a diagnostic Blocked status.
        assert!(matches!(
            shutdown_status(ShutdownDecision::Ready, 0, 2, true),
            ShutdownStatus::Blocked {
                pending_profiles: 2,
                ..
            }
        ));
        assert_eq!(
            shutdown_status(ShutdownDecision::Ready, 0, 0, true),
            ShutdownStatus::Ready
        );
    }

    #[test]
    fn cookie_flush_gate_is_separate_and_never_authorizes_early_shutdown() {
        assert!(matches!(
            shutdown_status_with_flushes(ShutdownDecision::Ready, 0, 0, 1, false),
            ShutdownStatus::Draining {
                pending_tasks: 0,
                pending_profiles: 0,
                pending_cookie_flushes: 1,
                ..
            }
        ));
        assert!(matches!(
            shutdown_status_with_flushes(ShutdownDecision::Ready, 0, 0, 1, true),
            ShutdownStatus::Blocked {
                pending_cookie_flushes: 1,
                ..
            }
        ));
        assert_eq!(
            shutdown_status_with_flushes(ShutdownDecision::Ready, 0, 0, 0, true),
            ShutdownStatus::Ready
        );
    }

    #[test]
    fn single_process_switch_is_detected_from_real_arguments() {
        use std::ffi::OsString;
        let plain = ["--type=renderer".to_string(), "--lang=en-US".to_string()]
            .into_iter()
            .map(OsString::from);
        assert!(!contains_single_process_switch(plain));
        for rejected in [
            "--single-process",
            "--single-process=1",
            "-single-process",
            "/SINGLE-PROCESS=1",
            "--Single-Process",
        ] {
            assert!(
                contains_single_process_switch([OsString::from(rejected)]),
                "{rejected} must be rejected before any CEF call"
            );
        }
        assert!(!contains_single_process_switch([
            OsString::from("--"),
            OsString::from("--single-process")
        ]));
        // Lookalikes are not the CEF switch.
        assert!(!contains_single_process_switch([OsString::from(
            "--single-processed"
        )]));
        assert!(!contains_single_process_switch([OsString::from(
            "single-process"
        )]));
    }

    #[test]
    fn command_line_cannot_override_explicit_storage_layout() {
        use std::ffi::OsString;
        for name in [
            "--user-data-dir=X",
            "/PROFILE-DIRECTORY=Other",
            "-incognito",
            "--guest",
            "--cache-path=X",
            "--root-cache-path=X",
        ] {
            assert!(contains_storage_override_switch([OsString::from(name)]));
        }
        assert!(!contains_storage_override_switch([
            OsString::from("--"),
            OsString::from("--incognito")
        ]));
        assert!(!contains_storage_override_switch([
            OsString::from("--storage"),
            OsString::from("Memory")
        ]));
    }

    #[test]
    fn shutdown_ticket_identity_rejects_foreign_runtimes() {
        let ticket = ShutdownTicket {
            runtime: RuntimeGeneration::new(1),
            sequence: 1,
        };
        assert_ne!(ticket.runtime, RuntimeGeneration::new(2));
    }

    #[derive(Default)]
    struct CountingWake {
        wakes: AtomicU64,
    }
    impl UiDispatcher for CountingWake {
        fn wake(&self) -> Result<(), EngineError> {
            self.wakes.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    struct NoopUi;
    impl UiDispatcher for NoopUi {
        fn wake(&self) -> Result<(), EngineError> {
            Ok(())
        }
    }

    #[test]
    fn thread_identity_is_os_backed_before_any_cef_bootstrap() {
        // No `initialize`/`api_hash` has run in this test process. Thread
        // identity must therefore come from the OS, not from a CEF helper call.
        let first = current_thread_id();
        assert_ne!(first, 0);
        assert_eq!(first, current_thread_id());
        let observed = std::thread::spawn(current_thread_id).join().unwrap();
        assert_ne!(observed, 0);
        assert_ne!(observed, first, "distinct OS threads keep distinct ids");
    }

    #[test]
    fn audit_counts_only_calls_and_flags_entries_after_shutdown() {
        let audit = NativeAudit::new();
        audit.record();
        audit.record();
        assert_eq!(audit.snapshot(), (2, 0));
        audit.shutdown_returned();
        assert_eq!(audit.snapshot(), (2, 0));
        audit.record();
        assert_eq!(audit.snapshot(), (3, 1));
    }

    #[test]
    fn refused_public_entry_does_not_advance_native_audit() {
        // The admission gate refuses before any native entry; this mirrors what
        // `pump`/`submit`/`create_browser` rely on after `finish_shutdown`.
        // Building a real Runtime needs CEF initialize, so this checks the same
        // seal predicate the adapter consults.
        let mut barrier = ShutdownBarrier::new(InitializeOutcome::Accepted);
        barrier.request(Instant::now()).unwrap();
        assert_eq!(
            barrier.observe(&[], 0, 0, Instant::now()),
            ShutdownDecision::Ready
        );
        barrier.finish().unwrap();
        assert!(!barrier.accepts_work());
        let audit = NativeAudit::new();
        audit.shutdown_returned();
        // A rejected attempt is never recorded as a native call.
        assert_eq!(audit.snapshot(), (0, 0));
    }

    #[test]
    fn startup_wakes_once_when_both_readiness_inputs_arrive() {
        let wake = Arc::new(CountingWake::default());
        let startup = StartupEndpoint::new(wake.clone());
        // ContextReady before the initialize return must not claim readiness.
        startup.context_initialized(42);
        assert_eq!(wake.wakes.load(Ordering::Acquire), 0);
        startup.initialize_returned(InitializeOutcome::Accepted);
        assert_eq!(wake.wakes.load(Ordering::Acquire), 1);
        // A duplicate context callback is not a new readiness transition.
        startup.context_initialized(42);
        assert_eq!(wake.wakes.load(Ordering::Acquire), 1);
    }

    #[test]
    fn startup_wakes_when_initialize_return_follows_context_ready() {
        let wake = Arc::new(CountingWake::default());
        let startup = StartupEndpoint::new(wake.clone());
        startup.initialize_returned(InitializeOutcome::Accepted);
        assert_eq!(wake.wakes.load(Ordering::Acquire), 0);
        startup.context_initialized(7);
        assert_eq!(wake.wakes.load(Ordering::Acquire), 1);
    }

    // These tests build the real cef-rs App wrapper (no CEF initialize call) and
    // observe our own reference count, so they stay metadata-only.
    fn test_app() -> cef::App {
        // The builder holds only weak/plain state, so this metadata-only App
        // can be built without a ProfileManager or CEF initialize.
        RuntimeAppBuilder::new(
            StartupEndpoint::new(Arc::new(NoopUi)),
            std::sync::Weak::new(),
            Arc::new(std::sync::atomic::AtomicU8::new(0)),
            Vec::new(),
            Vec::new(),
            None,
        )
    }

    #[test]
    fn app_owner_releases_our_reference_before_native_shutdown() {
        let app = test_app();
        let observer = app.clone();
        assert!(!observer.has_one_ref(), "owner and observer hold refs");

        let mut owner = AppOwner::new(app);
        assert!(owner.is_retained());
        owner.release_before_cef_shutdown();
        assert!(!owner.is_retained());
        // Exactly our runtime reference was released; the observer remains.
        assert!(observer.has_one_ref());
    }

    #[test]
    fn app_owner_retains_reference_when_runtime_did_not_finish() {
        let app = test_app();
        let observer = app.clone();
        let owner = AppOwner::new(app);
        drop(owner);
        // A normal release would leave only the observer; `has_one_ref` being
        // false proves `Drop` leaked our reference instead of releasing a
        // wrapper whose native peers may still be live.
        assert!(!observer.has_one_ref());
        drop(observer);
    }

    static NEXT_LEASE_TEST: AtomicU64 = AtomicU64::new(1);

    fn test_lease_name() -> String {
        // Never the production per-PID name: a committed test token must not
        // block later native initialization in this process.
        format!(
            "rust-cef-runtime-test-{}-{}",
            std::process::id(),
            NEXT_LEASE_TEST.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[test]
    fn uncommitted_process_lease_releases_name_on_drop() {
        let name = test_lease_name();
        {
            let guard = ProcessLease::acquire_named(&name).expect("first acquire");
            assert!(matches!(
                ProcessLease::acquire_named(&name),
                Err(StartError::AlreadyActive)
            ));
            drop(guard);
        }
        // The pre-init failure path must not lock the process out of CEF.
        let guard = ProcessLease::acquire_named(&name).expect("re-acquire after drop");
        drop(guard);
    }

    #[test]
    fn start_acquires_the_lease_before_any_cef_bootstrap() {
        // Source-level ordering guard for the calls that touch libcef before
        // `initialize`: a second `start` after shutdown/false must be rejected
        // by the lease, never by re-entering CEF.
        let source = include_str!("runtime.rs");
        let start = source.split_once("pub fn start(").expect("start exists").1;
        let body = start
            .split_once("\n    pub fn state(")
            .expect("start body")
            .0;
        let lease = body
            .find("ProcessLease::acquire()?")
            .expect("lease acquire");
        let version = body
            .find("crate::subprocess::check_loaded_cef()")
            .expect("version check");
        let hash = body.find("api_hash(").expect("api hash");
        let init = body.find("initialize(").expect("initialize");
        assert!(
            lease < version,
            "lease must precede the loaded-version probe"
        );
        assert!(lease < hash, "lease must precede api_hash");
        assert!(lease < init, "lease must precede initialize");
        assert!(version < init && hash < init);
    }

    #[test]
    fn committed_process_lease_blocks_reinitialization_until_process_exit() {
        let name = test_lease_name();
        let token = ProcessLease::acquire_named(&name)
            .expect("first acquire")
            .commit();
        assert!(matches!(
            ProcessLease::acquire_named(&name),
            Err(StartError::AlreadyActive)
        ));
        drop(token);
        // The committed OS handle survives until process exit; dropping the
        // Rust owner must not allow a second initialize.
        assert!(matches!(
            ProcessLease::acquire_named(&name),
            Err(StartError::AlreadyActive)
        ));
    }

    #[test]
    fn runtime_start_rejects_invalid_settings_before_process_lease() {
        let dir = std::env::temp_dir();
        let config = RuntimeConfig {
            thread_mode: ThreadMode::Threaded,
            files: RuntimeFiles {
                directory: dir.clone(),
                helper_executable: dir.join("rust-cef-helper.exe"),
            },
            root_cache_path: dir.clone(),
            default_profile: ProfileStorage::Memory,
            content_policy: TrustedContentPolicy {
                allowed_origins: Vec::new(),
                allow_embedded_data: true,
            },
            settings: rust_cef_core::contracts::RuntimeSettings {
                allowlisted_switches: vec![rust_cef_core::contracts::CliSwitch::flag(
                    "single-process",
                )],
                ..Default::default()
            },
        };
        struct NoopSink;
        impl BrowserEventSink for NoopSink {
            fn publish(&self, _event: BrowserEventEnvelope) -> PublishResult {
                PublishResult::Published
            }
        }
        impl FrameSink for NoopSink {
            fn publish(&self, _frame: OwnedFrame) -> PublishResult {
                PublishResult::Published
            }
        }
        impl UiDispatcher for NoopSink {
            fn wake(&self) -> Result<(), EngineError> {
                Ok(())
            }
        }
        let noop_events = Arc::new(NoopSink);
        let err = Runtime::start(
            config,
            RuntimeServices {
                events: noop_events.clone(),
                frames: noop_events.clone(),
                ui: noop_events,
            },
        );
        assert!(matches!(
            err,
            Err(StartError::InvalidSettings(
                rust_cef_core::contracts::ConfigError::ForbiddenSwitch
            ))
        ));
    }

    #[test]
    fn build_settings_preserves_explicit_runtime_settings() {
        let dir = std::env::temp_dir();
        let config = RuntimeConfig {
            thread_mode: ThreadMode::Threaded,
            files: RuntimeFiles {
                directory: dir.clone(),
                helper_executable: dir.join("rust-cef-helper.exe"),
            },
            root_cache_path: dir.clone(),
            default_profile: ProfileStorage::Memory,
            content_policy: TrustedContentPolicy {
                allowed_origins: Vec::new(),
                allow_embedded_data: true,
            },
            settings: rust_cef_core::contracts::RuntimeSettings {
                user_agent: Some("MyHost/2.0".into()),
                user_agent_product: Some("MyProd/2.0".into()),
                locale: Some("zh-CN".into()),
                accept_language_list: Some("zh-CN,en".into()),
                background_color: Some(0xFF112233),
                allowlisted_switches: vec![],
                ..Default::default()
            },
        };
        let cef_settings = build_settings(&config, &dir, &dir, &dir.join("rust-cef-helper.exe"));
        assert_eq!(cef_settings.user_agent.to_string(), "MyHost/2.0");
        assert_eq!(cef_settings.user_agent_product.to_string(), "MyProd/2.0");
        assert_eq!(cef_settings.locale.to_string(), "zh-CN");
        assert_eq!(cef_settings.accept_language_list.to_string(), "zh-CN,en");
        assert_eq!(cef_settings.background_color, 0xFF112233);
        assert_eq!(cef_settings.windowless_rendering_enabled, 1);

        let mut disabled_config = config;
        disabled_config.settings.windowless_rendering_enabled = false;
        let disabled_settings = build_settings(
            &disabled_config,
            &dir,
            &dir,
            &dir.join("rust-cef-helper.exe"),
        );
        assert_eq!(disabled_settings.windowless_rendering_enabled, 0);
    }
}

/// Projects the core decision plus the native-only profile/callback gate.
///
/// The core barrier keeps its original browser/task/request semantics; this
/// projection never reports `Ready` while a profile initialization or handler
/// callback is still live. A deadline that elapsed while only the profile gate
/// is live reports `Blocked` (diagnostic), never an authorization to finish.
#[cfg(test)]
fn shutdown_status(
    decision: ShutdownDecision,
    post_failures: u64,
    pending_profiles: u64,
    deadline_elapsed: bool,
) -> ShutdownStatus {
    shutdown_status_with_flushes(
        decision,
        post_failures,
        pending_profiles,
        0,
        deadline_elapsed,
    )
}

fn shutdown_status_with_flushes(
    decision: ShutdownDecision,
    post_failures: u64,
    pending_profiles: u64,
    pending_cookie_flushes: u64,
    deadline_elapsed: bool,
) -> ShutdownStatus {
    match decision {
        ShutdownDecision::Ready if pending_profiles != 0 || pending_cookie_flushes != 0 => {
            if deadline_elapsed {
                ShutdownStatus::Blocked {
                    open_browsers: Vec::new(),
                    pending_tasks: 0,
                    pending_requests: 0,
                    pending_profiles,
                    pending_cookie_flushes,
                    post_failures,
                }
            } else {
                ShutdownStatus::Draining {
                    open_browsers: Vec::new(),
                    pending_tasks: 0,
                    pending_requests: 0,
                    pending_profiles,
                    pending_cookie_flushes,
                    post_failures,
                }
            }
        }
        ShutdownDecision::NotRequested => ShutdownStatus::NotRequested,
        ShutdownDecision::Draining {
            open_browsers,
            pending_tasks,
            pending_requests,
        } => ShutdownStatus::Draining {
            open_browsers,
            pending_tasks,
            pending_requests,
            pending_profiles,
            pending_cookie_flushes,
            post_failures,
        },
        ShutdownDecision::Blocked {
            open_browsers,
            pending_tasks,
            pending_requests,
        } => ShutdownStatus::Blocked {
            open_browsers,
            pending_tasks,
            pending_requests,
            pending_profiles,
            pending_cookie_flushes,
            post_failures,
        },
        ShutdownDecision::Ready => ShutdownStatus::Ready,
        ShutdownDecision::Stopped => ShutdownStatus::Stopped,
        ShutdownDecision::ProcessExitRequired { exit_code } => {
            ShutdownStatus::ProcessExitRequired { exit_code }
        }
    }
}

/// Mirrors cef_command_line.h: Windows accepts --, - and / prefixes,
/// lowercases ASCII switch names, and stops parsing switches at --.
/// Pure argument inspection keeps rejected starts out of CEF entirely.
fn contains_single_process_switch(arguments: impl IntoIterator<Item = std::ffi::OsString>) -> bool {
    for argument in arguments {
        let argument = argument.to_string_lossy();
        if argument == "--" {
            break;
        }
        let switch = argument
            .strip_prefix("--")
            .or_else(|| argument.strip_prefix('-'))
            .or_else(|| argument.strip_prefix('/'));
        if switch.is_some_and(|value| {
            value
                .split('=')
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case("single-process"))
        }) {
            return true;
        }
    }
    false
}

fn contains_storage_override_switch(
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> bool {
    for argument in arguments {
        let argument = argument.to_string_lossy();
        if argument == "--" {
            break;
        }
        let name = argument
            .strip_prefix("--")
            .or_else(|| argument.strip_prefix('-'))
            .or_else(|| argument.strip_prefix('/'))
            .and_then(|value| value.split('=').next());
        if name.is_some_and(|name| {
            [
                "user-data-dir",
                "profile-directory",
                "incognito",
                "guest",
                "cache-path",
                "root-cache-path",
            ]
            .iter()
            .any(|key| name.eq_ignore_ascii_case(key))
        }) {
            return true;
        }
    }
    false
}

fn contains_policy_override_switch(
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> bool {
    for argument in arguments {
        let argument = argument.to_string_lossy();
        if argument == "--" {
            break;
        }
        let name = argument
            .strip_prefix("--")
            .or_else(|| argument.strip_prefix('-'))
            .or_else(|| argument.strip_prefix('/'))
            .and_then(|value| value.split('=').next());
        if name.is_some_and(|name| {
            [
                "ignore-certificate-errors",
                "ignore-certificate-errors-spki-list",
                "allow-insecure-localhost",
                "enable-media-stream",
                "use-fake-ui-for-media-stream",
                "disable-web-security",
                "allow-running-insecure-content",
                "allow-file-access-from-files",
                "disable-features",
            ]
            .iter()
            .any(|key| name.eq_ignore_ascii_case(key))
        }) {
            return true;
        }
    }
    false
}

pub(crate) fn dos_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    // std canonicalize yields \\?\C:\... (verbatim). CEF's file APIs expect the
    // normal DOS/UNC form; strip only the prefix, not the rest of the path.
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        text.into_owned()
    }
}

fn build_settings(
    config: &RuntimeConfig,
    root_cache: &Path,
    directory: &Path,
    helper: &Path,
) -> Settings {
    let locales = directory.join("locales");
    let cache_path = match &config.default_profile {
        rust_cef_core::contracts::ProfileStorage::Memory => String::new(),
        rust_cef_core::contracts::ProfileStorage::Persistent { .. } => dos_path(root_cache),
    };
    Settings {
        windowless_rendering_enabled: i32::from(config.settings.windowless_rendering_enabled),
        multi_threaded_message_loop: (config.thread_mode == ThreadMode::Threaded) as i32,
        external_message_pump: 0,
        // Trusted-content proof of concept only; this is not a security sandbox.
        no_sandbox: 1,
        browser_subprocess_path: CefString::from(dos_path(helper).as_str()),
        resources_dir_path: CefString::from(dos_path(directory).as_str()),
        locales_dir_path: CefString::from(dos_path(&locales).as_str()),
        root_cache_path: CefString::from(dos_path(root_cache).as_str()),
        cache_path: CefString::from(cache_path.as_str()),
        user_agent: config
            .settings
            .user_agent
            .as_deref()
            .map(CefString::from)
            .unwrap_or_default(),
        user_agent_product: config
            .settings
            .user_agent_product
            .as_deref()
            .map(CefString::from)
            .unwrap_or_default(),
        locale: config
            .settings
            .locale
            .as_deref()
            .map(CefString::from)
            .unwrap_or_default(),
        accept_language_list: config
            .settings
            .accept_language_list
            .as_deref()
            .map(CefString::from)
            .unwrap_or_default(),
        background_color: config.settings.background_color.unwrap_or(0),
        ..Default::default()
    }
}
