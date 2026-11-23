//! CEF UI-sequence executor. Task payloads own only plain data plus a weak
//! registry reference, so a rejected task can be destroyed on any thread
//! without touching native objects.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use cef::*;
use rust_cef_core::{
    contracts::{BrowserConfig, BrowserId, ProfileHandle, RuntimeGeneration},
    ports::{
        CloseMode, CloseTicket, CommandEnvelope, EngineError, OwnedEngineTask, TaskId, UiDispatcher,
    },
};

use crate::registry::UiRegistry;

/// Native thread identity used for diagnostics and the owner-thread check.
/// Deliberately the OS thread id: CEF's platform-thread helper is a libcef
/// entry point, and identity may be needed before `api_hash`/`initialize` or
/// after `shutdown`, where any CEF call is forbidden.
pub(crate) fn current_thread_id() -> u64 {
    // SAFETY: no arguments, always succeeds on Windows, touches no CEF state.
    unsafe { GetCurrentThreadId() as u64 }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
}

/// One shared audit for every place this adapter deliberately enters a native
/// CEF call. Entries are counted immediately before the call, and the phase
/// flips after the single real `cef::shutdown` returns, so a late call is
/// observable instead of being hidden behind a default-zero statistic.
///
/// Covered entries: `initialize`, `shutdown`, `do_message_loop_work`,
/// `post_task`, `browser_host_create_browser`, `BrowserHost::close_browser`,
/// `request_context_get_global_context`, `request_context_create_context`,
/// `RequestContext::is_same`/`is_sharing_with`, and the browser command calls
/// dispatched on the CEF UI sequence, plus explicit test-support menu model
/// mutations/cancel and synthetic CEF input calls. Rejected API attempts are refused
/// before these entries and are not counted. Not covered: wrapper refcount
/// Release/AddRef, pure queries such as
/// `api_hash`/`currently_on`/`get_current_platform_thread_id`, and callback
/// bodies CEF invokes on our wrappers (the profile handler itself reports into
/// the separate real `callbacks_in_flight` counter).
pub(crate) struct NativeAudit {
    after_shutdown: AtomicBool,
    native_calls: AtomicU64,
    calls_after_shutdown: AtomicU64,
}

impl NativeAudit {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            after_shutdown: AtomicBool::new(false),
            native_calls: AtomicU64::new(0),
            calls_after_shutdown: AtomicU64::new(0),
        })
    }

    /// Call exactly once, immediately before entering the native API.
    pub(crate) fn record(&self) {
        if self.after_shutdown.load(Ordering::Acquire) {
            // Increment the violation counter before the total so a test can
            // never observe "total advanced, after-shutdown still zero".
            self.calls_after_shutdown.fetch_add(1, Ordering::AcqRel);
        }
        self.native_calls.fetch_add(1, Ordering::Release);
    }

    /// Marks the phase after `cef::shutdown` returned. No native call may be
    /// recorded afterwards by a correctly rejected public API.
    pub(crate) fn shutdown_returned(&self) {
        self.after_shutdown.store(true, Ordering::Release);
    }

    pub(crate) fn snapshot(&self) -> (u64, u64) {
        (
            self.native_calls.load(Ordering::Acquire),
            self.calls_after_shutdown.load(Ordering::Acquire),
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TaskPayload {
    /// Ask CEF to enable spellchecking on the browser's own request context.
    SpellingPreferences {
        browser: BrowserId,
    },
    Create {
        browser: BrowserId,
        config: BrowserConfig,
    },
    #[cfg(feature = "test-support")]
    CreateDeviceFailureProbe {
        browser: BrowserId,
        config: BrowserConfig,
    },
    #[cfg(feature = "test-support")]
    CreateThenCloseProbe {
        browser: BrowserId,
        config: BrowserConfig,
    },
    #[cfg(feature = "test-support")]
    CreateCaptureProbe {
        browser: BrowserId,
        config: BrowserConfig,
    },
    #[cfg(feature = "test-support")]
    ProbeCommandPressureClose {
        browser: BrowserId,
        expectation: crate::test_support::PressureExpectation,
    },
    #[cfg(feature = "test-support")]
    ProbePressureTitle {
        browser: BrowserId,
        step: u16,
    },
    #[cfg(feature = "test-support")]
    ProbeFrameControl {
        browser: BrowserId,
        control: crate::test_support::FrameProbeControl,
    },
    ResizeRefresh {
        browser: BrowserId,
        ticket: crate::resize::ResizeTicket,
    },
    Command(CommandEnvelope),
    DrainMouseMoves {
        browser: BrowserId,
    },
    ApplyZoom {
        browser: BrowserId,
    },
    InputReset(rust_cef_core::ports::CommandId),
    Close {
        browser: BrowserId,
        mode: CloseMode,
        /// Only an owned result cell, never a Browser/Runtime reference.
        /// A browser-wide Runtime drain has no caller ticket.
        ticket: Option<CloseTicket>,
    },
    #[cfg(feature = "test-support")]
    /// Test-only: ask the real BrowserHost to start a print job, which must
    /// consult our PrintHandler before any dialog is shown.
    ProbePrint {
        browser: BrowserId,
    },
    /// Host decision for a pending print dialog.
    ResolvePrintDialog {
        browser: BrowserId,
        accept: bool,
    },
    /// Host-initiated managed DevTools open. The UI handler owns every native
    /// step; this payload carries only the original Browser identity.
    OpenDevTools {
        browser: BrowserId,
        inspect_at: Option<(i32, i32)>,
    },
    /// Host-initiated close of the associated DevTools browser. Mirrors
    /// CefBrowserHostWrapper::CloseDevTools.
    CloseDevTools {
        browser: BrowserId,
    },
    /// Resume a recorded close intent on CEF UI after OnAfterCreated returns.
    CloseAfterCreated {
        browser: BrowserId,
    },
    /// Completes OnBeforeClose cleanup deferred by a reentrant GPU checkout.
    /// No native reference is captured; the original endpoint owns retirement.
    RetireCapture {
        browser: BrowserId,
    },
    /// Takes the private request table on the CEF UI sequence and invokes the
    /// captured cancel callbacks after every registry lock is released.
    DrainRequests,
    /// Opaque reservation only, never a native callback or borrowed menu.
    #[cfg(feature = "test-support")]
    MenuCancel(crate::menu_native::MenuCancel),
    MenuReply(crate::menu_native::MenuCancel),
    #[cfg(feature = "test-support")]
    ProbeAuxiliaryPreparation {
        request: rust_cef_core::menu::ContextMenuRequestId,
        reservation: crate::menu_native::MenuCancel,
        witness: crate::test_support::AuxiliaryPreparationTicket,
    },
    SourceResult {
        id: rust_cef_core::source_view::SourceOperationId,
        result: Result<
            rust_cef_core::source_view::SourceText,
            rust_cef_core::source_view::SourceFailure,
        >,
    },
    SourceReleased(rust_cef_core::source_view::SourceOperationId),
    #[cfg(feature = "test-support")]
    ProbeReleaseSourceVisitor(rust_cef_core::source_view::SourceOperationId),
    /// Final dedicated-client fields are gone; no native reference is captured.
    /// The resource-owner retirement gate is independent and remains pending.
    AuxiliaryClientReleased {
        id: crate::devtools::AuxiliaryId,
        native_id: i32,
    },
    AuxiliaryReferencesReleased {
        id: crate::devtools::AuxiliaryId,
        native_id: i32,
    },
    AuxiliaryClose(crate::devtools::AuxiliaryCloseTicket),
    AuxiliaryNativeReleased {
        id: crate::devtools::AuxiliaryId,
        native_id: i32,
    },
    FindCancel(rust_cef_core::find::FindOperationId),
    RetireMenuBinding {
        browser: BrowserId,
        serial: u64,
    },
    /// Creates the one native request context for a freshly reserved profile.
    /// Plain data only: the manager reads its own catalog entry on the UI
    /// sequence, so no second copy of the config can diverge from it.
    CreateProfile {
        handle: ProfileHandle,
    },
    /// Compares two already-Ready profiles with the real CEF identity calls.
    /// The result is stored as owned booleans in the ticket table.
    CompareContexts {
        sequence: u64,
        left: ProfileHandle,
        right: ProfileHandle,
    },
    /// Marks a profile initialized after its CEF callback frame has returned.
    /// Never posted from inside the callback body itself, so no borrowed CEF
    /// wrapper can still be on the stack when readiness is published.
    ProfileInitialized {
        handle: ProfileHandle,
    },
    BeginCookieFlush {
        sequence: u64,
        handle: ProfileHandle,
    },
    CookieManagerReady {
        sequence: u64,
        handle: ProfileHandle,
    },
    CookieFlushCompleted {
        sequence: u64,
    },
    #[cfg(feature = "test-support")]
    ProbeRendererControl {
        browser: BrowserId,
        action: crate::renderer_probe::RendererProbeAction,
    },
    #[cfg(feature = "test-support")]
    ProbeDialogControl {
        browser: BrowserId,
        action: crate::dialog_probe::DialogProbeAction,
    },
    #[cfg(feature = "test-support")]
    ProbeDataNavigation {
        browser: BrowserId,
        target: String,
    },
    #[cfg(feature = "test-support")]
    ProbeMenuSourceOpen {
        browser: BrowserId,
    },
    #[cfg(feature = "test-support")]
    ProbeMenuFindOpen {
        browser: BrowserId,
    },
    #[cfg(feature = "test-support")]
    ProbeMenuSpellingOpen {
        browser: BrowserId,
        dictionary: bool,
    },
    #[cfg(feature = "test-support")]
    ProbeMenuBridgeOpen {
        browser: BrowserId,
        print: bool,
    },
    #[cfg(feature = "test-support")]
    ProbeMenuOpen {
        browser: BrowserId,
    },
    #[cfg(feature = "test-support")]
    ProbeMouse {
        browser: BrowserId,
        x: i32,
        y: i32,
        up: bool,
    },
    BeforeUnloadSweep {
        request: rust_cef_core::ports::RequestId,
    },
    ProfilePolicyIoBarrier {
        handle: ProfileHandle,
    },
    ProfilePolicyReady {
        handle: ProfileHandle,
    },
    RegisterProfileScheme {
        sequence: u64,
        handle: ProfileHandle,
        binding: rust_cef_core::custom_schemes::ProfileSchemeBinding,
    },
    ProfileSchemeIoBarrier {
        sequence: u64,
    },
    ProfileSchemeReady {
        sequence: u64,
    },
    PolicyEvents {
        browser: BrowserId,
    },
    EvalTimeout {
        id: rust_cef_core::js_bridge::EvalRequestId,
    },
    StartDownload {
        browser: BrowserId,
        url: String,
    },
    ConfirmDownload {
        id: rust_cef_core::host_capabilities::DownloadId,
        destination: std::path::PathBuf,
    },
    CancelDownload {
        id: rust_cef_core::host_capabilities::DownloadId,
    },
    ResolveFileDialog {
        id: rust_cef_core::host_capabilities::HostRequestId,
        decision: rust_cef_core::host_capabilities::FileDialogDecision,
    },
    ResolveJsDialog {
        id: rust_cef_core::host_capabilities::HostRequestId,
        decision: rust_cef_core::host_capabilities::JsDialogDecision,
    },
    ResolvePermission {
        id: rust_cef_core::host_capabilities::HostRequestId,
        decision: rust_cef_core::host_capabilities::PermissionDecision,
    },
    ResolveAuth {
        id: rust_cef_core::host_capabilities::HostRequestId,
        decision: rust_cef_core::host_capabilities::AuthDecision,
    },
    ResolveCertificate {
        id: rust_cef_core::host_capabilities::HostRequestId,
        decision: rust_cef_core::host_capabilities::CertificateDecision,
    },
    VisitProfileCookies {
        request_id: rust_cef_core::host_capabilities::CookieRequestId,
        handle: ProfileHandle,
        url: Option<String>,
        include_http_only: bool,
    },
    SetProfileCookie {
        request_id: rust_cef_core::host_capabilities::CookieRequestId,
        handle: ProfileHandle,
        url: String,
        cookie: rust_cef_core::host_capabilities::CookieRecord,
    },
    DeleteProfileCookies {
        request_id: rust_cef_core::host_capabilities::CookieRequestId,
        handle: ProfileHandle,
        url: Option<String>,
        cookie_name: Option<String>,
    },
    ClearProfileHttpAuth {
        request_id: rust_cef_core::host_capabilities::CookieRequestId,
        handle: ProfileHandle,
    },
    SetProfilePreference {
        request_id: rust_cef_core::host_capabilities::CookieRequestId,
        handle: ProfileHandle,
        name: String,
        value: rust_cef_core::host_capabilities::PreferenceValue,
    },
    PrintToPdf {
        id: rust_cef_core::host_capabilities::PdfExportId,
        destination: std::path::PathBuf,
        options: rust_cef_core::host_capabilities::PdfExportOptions,
    },
    AttachCdpObserver {
        browser: BrowserId,
    },
    DetachCdpObserver {
        browser: BrowserId,
    },
    SendCdpMessage {
        browser: BrowserId,
        json_message: String,
    },
    ExecuteCdpMethod {
        id: rust_cef_core::host_capabilities::CdpMethodId,
        method: String,
        params_json: Option<String>,
    },
    StartUrlRequest {
        id: rust_cef_core::url_request::UrlRequestId,
        spec: rust_cef_core::url_request::UrlRequestSpec,
    },
    CancelUrlRequest {
        id: rust_cef_core::url_request::UrlRequestId,
    },
    TimeoutUrlRequest {
        id: rust_cef_core::url_request::UrlRequestId,
    },
    DestroyNativeChildHwnd {
        browser: BrowserId,
        hwnd: std::num::NonZeroUsize,
    },
}

impl From<OwnedEngineTask> for TaskPayload {
    fn from(task: OwnedEngineTask) -> Self {
        match task {
            OwnedEngineTask::Create { browser, config } => Self::Create { browser, config },
            OwnedEngineTask::Command(command) => Self::Command(command),
            OwnedEngineTask::Close { browser, mode } => Self::Close {
                browser,
                mode,
                ticket: None,
            },
        }
    }
}

struct Counters {
    posted: AtomicU64,
    completed: AtomicU64,
    post_failed: AtomicU64,
    pending: AtomicU64,
    /// Completion notification is owned data only; the call happens after the
    /// pending decrement and outside every registry lock.
    ui: Arc<dyn UiDispatcher>,
}

impl Counters {
    fn new(ui: Arc<dyn UiDispatcher>) -> Self {
        Self {
            posted: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            post_failed: AtomicU64::new(0),
            pending: AtomicU64::new(0),
            ui,
        }
    }

    /// Returns true when this retirement emptied the queue. The host is woken
    /// only for that transition: intermediate completions cannot unblock a
    /// shutdown barrier or readiness check.
    fn retire(&self) -> bool {
        self.completed.fetch_add(1, Ordering::Relaxed);
        let idle = self.pending.fetch_sub(1, Ordering::AcqRel) == 1;
        if idle {
            let _ = self.ui.wake();
        }
        idle
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutorStats {
    pub posted: u64,
    pub completed: u64,
    pub post_failed: u64,
    pub pending: u64,
    pub ui_execute_thread_id: Option<u64>,
}

/// Entries are accepted before they execute; completion is observed only when
/// CEF actually runs the task on its UI thread.
pub(crate) struct Executor {
    runtime: RuntimeGeneration,
    draining: AtomicBool,
    /// Held from admission checks through native post's return. Sealing never
    /// waits for this lock on the host thread; a busy gate means not yet idle.
    admission: Mutex<bool>,
    counters: Arc<Counters>,
    ui_thread: Arc<Mutex<Option<u64>>>,
    registry: Weak<UiRegistry>,
    next_task: AtomicU64,
    audit: Arc<NativeAudit>,
    /// Test seam: replaces the real native post. Never set in a production
    /// build, and an injected rejection still counts as a failure.
    #[cfg(test)]
    test_post: Mutex<Option<Arc<dyn Fn(&TaskPayload) -> bool + Send + Sync>>>,
}

impl Executor {
    pub(crate) fn new(
        runtime: RuntimeGeneration,
        registry: &Arc<UiRegistry>,
        audit: Arc<NativeAudit>,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            draining: AtomicBool::new(false),
            admission: Mutex::new(false),
            counters: Arc::new(Counters::new(registry.dispatcher())),
            ui_thread: Arc::new(Mutex::new(None)),
            registry: Arc::downgrade(registry),
            next_task: AtomicU64::new(1),
            audit,
            #[cfg(test)]
            test_post: Mutex::new(None),
        })
    }

    #[cfg(test)]
    pub(crate) fn set_test_post(&self, post: Arc<dyn Fn(&TaskPayload) -> bool + Send + Sync>) {
        *self
            .test_post
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(post);
    }

    pub(crate) fn post(&self, payload: TaskPayload) -> Result<TaskId, EngineError> {
        self.post_inner(payload, false)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn probe_reject_create_post(
        &self,
        browser: BrowserId,
        config: BrowserConfig,
    ) -> Result<TaskId, EngineError> {
        self.post_with(TaskPayload::Create { browser, config }, false, |_, _| false)
    }

    /// Close tasks stay admissible while draining so every native instance can
    /// still be terminated exactly once.
    pub(crate) fn post_close(&self, payload: TaskPayload) -> Result<TaskId, EngineError> {
        self.post_inner(payload, true)
    }

    /// Completion markers for already-issued native work stay admissible while
    /// draining: an issued profile still needs its own initialized callback to
    /// reach a terminal state, exactly like a close. A sealed executor rejects
    /// it, which can only happen after the profile gate was already zero.
    pub(crate) fn post_while_draining(&self, payload: TaskPayload) -> Result<TaskId, EngineError> {
        self.post_inner(payload, true)
    }

    pub(crate) fn post_delayed(
        &self,
        payload: TaskPayload,
        delay_ms: i64,
    ) -> Result<TaskId, EngineError> {
        self.post_on(payload, false, ThreadId::UI, delay_ms)
    }

    pub(crate) fn post_delayed_while_draining(
        &self,
        payload: TaskPayload,
        delay_ms: i64,
    ) -> Result<TaskId, EngineError> {
        self.post_on(payload, true, ThreadId::UI, delay_ms)
    }

    pub(crate) fn post_policy_io_barrier(
        &self,
        handle: ProfileHandle,
    ) -> Result<TaskId, EngineError> {
        self.post_on(
            TaskPayload::ProfilePolicyIoBarrier { handle },
            true,
            ThreadId::IO,
            0,
        )
    }

    pub(crate) fn post_scheme_io_barrier(&self, sequence: u64) -> Result<TaskId, EngineError> {
        self.post_on(
            TaskPayload::ProfileSchemeIoBarrier { sequence },
            true,
            ThreadId::IO,
            0,
        )
    }

    fn post_inner(
        &self,
        payload: TaskPayload,
        allow_while_draining: bool,
    ) -> Result<TaskId, EngineError> {
        self.post_on(payload, allow_while_draining, ThreadId::UI, 0)
    }

    fn post_on(
        &self,
        payload: TaskPayload,
        allow_while_draining: bool,
        thread: ThreadId,
        delay_ms: i64,
    ) -> Result<TaskId, EngineError> {
        #[cfg(test)]
        let injected: Option<Arc<dyn Fn(&TaskPayload) -> bool + Send + Sync>> = self
            .test_post
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        #[cfg(not(test))]
        let injected: Option<Arc<dyn Fn(&TaskPayload) -> bool + Send + Sync>> = None;
        self.post_with(
            payload,
            allow_while_draining,
            |task, payload| match &injected {
                Some(post) => post(payload),
                None => {
                    self.audit.record();
                    if delay_ms > 0 {
                        post_delayed_task(thread, Some(task), delay_ms) != 0
                    } else {
                        post_task(thread, Some(task)) != 0
                    }
                }
            },
        )
    }

    /// `post` returns whether CEF accepted the task. Test-only callers inject a
    /// rejecting closure to exercise the failure path without CEF.
    fn post_with(
        &self,
        payload: TaskPayload,
        allow_while_draining: bool,
        post: impl FnOnce(&mut Task, &TaskPayload) -> bool,
    ) -> Result<TaskId, EngineError> {
        let stopped = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *stopped {
            return Err(EngineError::Closed);
        }
        if !allow_while_draining && self.draining.load(Ordering::Acquire) {
            return Err(EngineError::Closing);
        }
        let sequence = self
            .next_task
            .try_update(Ordering::AcqRel, Ordering::Acquire, |next| {
                next.checked_add(1)
            })
            .map_err(|_| EngineError::Backend { code: None })?;
        self.counters.posted.fetch_add(1, Ordering::Relaxed);
        self.counters.pending.fetch_add(1, Ordering::Release);
        let id = TaskId {
            runtime: self.runtime,
            sequence,
        };
        #[cfg(feature = "test-support")]
        let queued = (id, payload.clone());
        #[cfg(not(feature = "test-support"))]
        let queued = payload.clone();
        let mut task = RuntimeTask::new(
            Arc::new(Mutex::new(Some(queued))),
            self.registry.clone(),
            self.counters.clone(),
            self.ui_thread.clone(),
        );
        // A failed post releases the task immediately; the payload holds no
        // native references, so dropping it here is safe on the caller thread.
        if !post(&mut task, &payload) {
            self.counters.post_failed.fetch_add(1, Ordering::Relaxed);
            self.counters.pending.fetch_sub(1, Ordering::Release);
            return Err(EngineError::PostRejected);
        }
        Ok(id)
    }

    pub(crate) fn begin_drain(&self) -> u64 {
        self.draining.store(true, Ordering::Release);
        self.pending()
    }

    pub(crate) fn try_seal_if_idle(&self) -> bool {
        let mut stopped = match self.admission.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => return false,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        };
        if self.pending() != 0
            || self
                .registry
                .upgrade()
                .is_some_and(|registry| registry.input_cleanup_pending())
        {
            return false;
        }
        *stopped = true;
        true
    }

    pub(crate) fn is_stopped(&self) -> bool {
        *self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn pending(&self) -> u64 {
        self.counters.pending.load(Ordering::Acquire)
    }

    pub(crate) fn stats(&self) -> ExecutorStats {
        let ui_execute_thread_id = self.ui_thread.lock().ok().and_then(|id| *id);
        ExecutorStats {
            posted: self.counters.posted.load(Ordering::Acquire),
            completed: self.counters.completed.load(Ordering::Acquire),
            post_failed: self.counters.post_failed.load(Ordering::Acquire),
            pending: self.pending(),
            ui_execute_thread_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::contracts::Viewport;

    fn registry() -> Arc<UiRegistry> {
        use rust_cef_core::ports::{BrowserEventEnvelope, FrameSink, OwnedFrame, PublishResult};
        struct Sinks;
        impl rust_cef_core::ports::BrowserEventSink for Sinks {
            fn publish(&self, _: BrowserEventEnvelope) -> PublishResult {
                PublishResult::Published
            }
        }
        impl FrameSink for Sinks {
            fn publish(&self, _: OwnedFrame) -> PublishResult {
                PublishResult::Published
            }
        }
        impl rust_cef_core::ports::UiDispatcher for Sinks {
            fn wake(&self) -> Result<(), EngineError> {
                Ok(())
            }
        }
        let sinks = Arc::new(Sinks);
        UiRegistry::new(
            RuntimeGeneration::new(1),
            sinks.clone(),
            sinks.clone(),
            sinks,
            NativeAudit::new(),
        )
    }

    fn close_payload() -> TaskPayload {
        OwnedEngineTask::Close {
            browser: BrowserId::new(RuntimeGeneration::new(1), 1),
            mode: CloseMode::Force,
        }
        .into()
    }

    #[test]
    fn owned_engine_close_stays_unticketed() {
        assert!(matches!(close_payload(), TaskPayload::Close {
            browser, mode: CloseMode::Force, ticket: None,
        } if browser == BrowserId::new(RuntimeGeneration::new(1), 1)));
    }

    fn create_payload() -> TaskPayload {
        TaskPayload::Create {
            browser: BrowserId::new(RuntimeGeneration::new(1), 1),
            config: BrowserConfig {
                initial_url: "data:text/html,fixture".into(),
                profile: None,
                render_preference: rust_cef_core::contracts::RenderPreference::SoftwareRequired,
                viewport: Viewport {
                    width: 320,
                    height: 240,
                    scale: 1.0,
                },
                options: Default::default(),
            },
        }
    }

    #[derive(Default)]
    struct CountingWake {
        wakes: AtomicU64,
    }
    impl rust_cef_core::ports::UiDispatcher for CountingWake {
        fn wake(&self) -> Result<(), EngineError> {
            self.wakes.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    fn registry_with_wake(wake: Arc<CountingWake>) -> Arc<UiRegistry> {
        use rust_cef_core::ports::{BrowserEventEnvelope, FrameSink, OwnedFrame, PublishResult};
        struct Sinks;
        impl rust_cef_core::ports::BrowserEventSink for Sinks {
            fn publish(&self, _: BrowserEventEnvelope) -> PublishResult {
                PublishResult::Published
            }
        }
        impl FrameSink for Sinks {
            fn publish(&self, _: OwnedFrame) -> PublishResult {
                PublishResult::Published
            }
        }
        let sinks = Arc::new(Sinks);
        UiRegistry::new(
            RuntimeGeneration::new(1),
            sinks.clone(),
            sinks,
            wake,
            NativeAudit::new(),
        )
    }

    #[test]
    fn completion_wakes_the_host_only_after_the_queue_is_empty() {
        let wake = Arc::new(CountingWake::default());
        let registry = registry_with_wake(wake.clone());
        let executor = Executor::new(RuntimeGeneration::new(1), &registry, NativeAudit::new());
        let mut accepted = None;
        executor
            .post_with(close_payload(), true, |task, _| {
                accepted = Some(task.clone());
                true
            })
            .unwrap();
        assert_eq!(wake.wakes.load(Ordering::Acquire), 0);
        // One accepted task retires: the queue is empty and the host is woken.
        executor.counters.retire();
        assert_eq!(wake.wakes.load(Ordering::Acquire), 1);
        assert_eq!(executor.pending(), 0);
        drop(accepted);
    }

    #[test]
    fn intermediate_completion_does_not_wake_the_host() {
        let wake = Arc::new(CountingWake::default());
        let registry = registry_with_wake(wake.clone());
        let executor = Executor::new(RuntimeGeneration::new(1), &registry, NativeAudit::new());
        for _ in 0..2 {
            executor
                .post_with(close_payload(), true, |_, _| true)
                .unwrap();
        }
        executor.counters.retire();
        assert_eq!(wake.wakes.load(Ordering::Acquire), 0);
        executor.counters.retire();
        assert_eq!(wake.wakes.load(Ordering::Acquire), 1);
    }

    #[test]
    fn rejected_post_counts_failure_and_leaves_nothing_pending() {
        let registry = registry();
        let executor = Executor::new(RuntimeGeneration::new(1), &registry, NativeAudit::new());
        // Injected rejection: no CEF call, and the plain payload is dropped on
        // this thread without touching native references.
        let strong = Arc::strong_count(&registry);
        let weak = Arc::weak_count(&registry);
        let error = executor
            .post_with(close_payload(), true, |_, _| false)
            .unwrap_err();
        assert_eq!(Arc::strong_count(&registry), strong);
        assert_eq!(Arc::weak_count(&registry), weak);
        assert_eq!(error, EngineError::PostRejected);
        let stats = executor.stats();
        assert_eq!(stats.posted, 1);
        assert_eq!(stats.post_failed, 1);
        assert_eq!(stats.completed, 0);
        assert_eq!(stats.pending, 0);
    }

    #[test]
    fn seal_never_overtakes_an_inflight_native_post() {
        let registry = registry();
        let executor = Executor::new(RuntimeGeneration::new(1), &registry, NativeAudit::new());
        let (entered, entered_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let poster = executor.clone();
        let worker = std::thread::spawn(move || {
            poster.post_with(close_payload(), true, |_, _| {
                entered.send(()).unwrap();
                release_rx.recv().unwrap();
                false // Injected native rejection; no CEF initialization.
            })
        });
        entered_rx.recv().unwrap();
        // Sealing is nonblocking and cannot authorize shutdown while posting.
        let sealed_while_posting = executor.try_seal_if_idle();
        release.send(()).unwrap();
        assert_eq!(worker.join().unwrap(), Err(EngineError::PostRejected));
        assert!(!sealed_while_posting);
        assert_eq!(executor.pending(), 0);
        assert!(executor.try_seal_if_idle());
        assert!(executor.is_stopped());
        let result = executor.post_with(close_payload(), true, |_, _| panic!("post after seal"));
        assert_eq!(result, Err(EngineError::Closed));
    }

    #[test]
    fn seal_cannot_pass_a_claim_before_its_counter_increment() {
        let registry = registry();
        let executor = Executor::new(RuntimeGeneration::new(1), &registry, NativeAudit::new());
        let claim = executor.admission.lock().unwrap();
        assert_eq!(executor.pending(), 0);
        assert!(!executor.try_seal_if_idle());
        drop(claim);
        assert!(executor.try_seal_if_idle());
    }

    #[test]
    fn accepted_task_blocks_seal_until_completion() {
        let registry = registry();
        let executor = Executor::new(RuntimeGeneration::new(1), &registry, NativeAudit::new());
        let mut accepted = None;
        executor
            .post_with(close_payload(), true, |task, _| {
                accepted = Some(task.clone());
                true
            })
            .unwrap();
        assert_eq!(executor.pending(), 1);
        assert!(!executor.try_seal_if_idle());
        // Only this injected task's retirement is simulated; never a Native pass.
        executor.counters.retire();
        assert!(executor.try_seal_if_idle());
        drop(accepted);
    }

    #[test]
    fn drain_transition_observes_tasks_still_inflight() {
        let registry = registry();
        let executor = Executor::new(RuntimeGeneration::new(1), &registry, NativeAudit::new());
        executor
            .post_with(close_payload(), true, |_, _| true)
            .unwrap();
        assert_eq!(executor.begin_drain(), 1);
        executor.counters.retire();
        assert_eq!(executor.pending(), 0);
    }

    #[test]
    fn draining_rejects_commands_but_keeps_close_admissible() {
        let registry = registry();
        let executor = Executor::new(RuntimeGeneration::new(1), &registry, NativeAudit::new());
        executor.begin_drain();
        assert_eq!(
            executor.post(create_payload()).unwrap_err(),
            EngineError::Closing
        );
        assert_eq!(
            executor.post_delayed(create_payload(), 100).unwrap_err(),
            EngineError::Closing,
            "resize delayed work cannot enter after drain"
        );
        assert_eq!(executor.pending(), 0);
        assert_eq!(
            executor
                .post_with(close_payload(), true, |_, _| false)
                .unwrap_err(),
            EngineError::PostRejected,
            "drain must allow close to reach the injected native post"
        );
        assert!(executor.try_seal_if_idle());
        assert_eq!(
            executor.post(create_payload()).unwrap_err(),
            EngineError::Closed
        );
    }
}

#[cfg(feature = "test-support")]
type QueuedTask = (TaskId, TaskPayload);
#[cfg(not(feature = "test-support"))]
type QueuedTask = TaskPayload;

wrap_task! {
    struct RuntimeTask {
        payload: Arc<Mutex<Option<QueuedTask>>>,
        registry: Weak<UiRegistry>,
        counters: Arc<Counters>,
        ui_thread: Arc<Mutex<Option<u64>>>,
    }

    impl Task {
        fn execute(&self) {
            let payload = self
                .payload
                .lock()
                .ok()
                .and_then(|mut slot| slot.take());
            let Some(payload) = payload else { return };
            #[cfg(feature = "test-support")]
            let (id, payload) = payload;
            if !matches!(payload, TaskPayload::ProfilePolicyIoBarrier { .. }) {
                if let Ok(mut observed) = self.ui_thread.lock() && observed.is_none() {
                    *observed = Some(current_thread_id());
                }
            }
            if let Some(registry) = self.registry.upgrade() {
                registry.execute(payload, #[cfg(feature = "test-support")] id);
            }
            // Count completion only after dispatch and its native locals are
            // gone, so a main thread polling `pending` never treats an in-flight
            // task as retired.
            self.counters.retire();
        }
    }
}
