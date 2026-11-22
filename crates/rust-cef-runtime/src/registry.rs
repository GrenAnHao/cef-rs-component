//! Native Browser/request ownership. One registry belongs to the Runtime owner;
//! native pointers leave it only inside a CEF UI-sequence call.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    num::NonZeroUsize,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
};

use cef::*;
use rust_cef_core::{
    contracts::{
        BrowserConfig, BrowserId, BrowserOptions, BrowserSnapshot, BrowserSurfaceMode,
        CefFeatureState, CreateError, PixelRect, RenderPreference, RuntimeGeneration, SessionState,
        Viewport, zoom_level_to_milli,
    },
    ime::{ImeReportedBounds, ImeSnapshot, ImeState},
    input::{InputEvent, InputKind, InputResetReason, InputSnapshot, Utf16Range},
    js_bridge::{
        EvalError, EvalLedger, EvalOutcome, EvalRequestId, JsBridgeConfig, JsFrameScope,
        MAX_JS_MESSAGE_QUEUE_PER_BROWSER, normalize_eval_timeout_ms, validate_script,
    },
    mailbox::{FrameMailbox, WakeTicket},
    ports::{
        BrowserCommand, BrowserControl, BrowserEngine, BrowserEvent, BrowserEventEnvelope,
        BrowserEventSink, CloseCompletion, CloseMode, CloseResult, CloseTicket, CommandEnvelope,
        CommandId, EngineError, FrameSink, FrameSource, MouseClick, PolicyAction, PublishResult,
        RequestId, SurfaceKind, UiDispatcher,
    },
    session::{AcceptedCommand, Session},
};

use crate::{
    attachment::AttachmentError,
    before_unload::{BeforeUnloadRequests, ClosePromptCut, PendingBeforeUnload},
    client::{
        BrowserClientBuilder, BrowserClientState, ViewportState, new_frame_mailbox, pixel_extent,
    },
    executor::{Executor, NativeAudit, TaskPayload},
    policy_native::NativePolicy,
    profile_flush::CookieFlushRegistry,
    profiles::ProfileManager,
};

const MAX_UX_EVENTS: usize = 64;

fn is_ux_event(event: &BrowserEvent) -> bool {
    matches!(
        event,
        BrowserEvent::CursorChanged(_)
            | BrowserEvent::TooltipChanged(_)
            | BrowserEvent::StatusMessageChanged(_)
            | BrowserEvent::FaviconUrlsChanged(_)
            | BrowserEvent::FullscreenModeChanged(_)
            | BrowserEvent::LoadingProgressChanged(_)
            | BrowserEvent::ConsoleMessage { .. }
            | BrowserEvent::ZoomLevelChanged(_)
            | BrowserEvent::AudioMutedChanged(_)
            | BrowserEvent::FocusTaken { .. }
            | BrowserEvent::GotFocus
            | BrowserEvent::HostShortcutTriggered { .. }
            | BrowserEvent::StartOutboundDrag(_)
            | BrowserEvent::OutboundDragCursorChanged(_)
            | BrowserEvent::AccessibilityTreeUpdated { .. }
    )
}

/// Mutable part is held only by the owner thread and CEF UI callbacks; no
/// native reference is dereferenced while publishing host events.
struct RegistryState {
    browsers: BTreeMap<BrowserId, Entry>,
    draining: bool,
}

// Registry lookups clone this Rust allocation only. The single CEF wrapper is
// adopted before locking and released by the last owner outside registry locks.
// Native API borrows/clones still require their original thread/scope checks.
type NativeBrowser = Arc<Browser>;

struct Entry {
    session: Session,
    /// Monotonic main-frame document epoch; incremented on real LoadStarted.
    document_epoch: u64,
    viewport: Arc<ViewportState>,
    store: Arc<FrameMailbox>,
    client_state: Option<Arc<BrowserClientState>>,
    #[cfg(feature = "test-support")]
    frame_probe_final: Option<crate::test_support::FrameProbeState>,
    #[cfg(feature = "test-support")]
    menu_probe_final: Option<Arc<crate::menu_probe::MenuProbe>>,
    /// Retained pure diagnostic endpoint; GPU refs must retire on CEF UI before
    /// the Browser terminal transition, even if a callback still owns its client.
    capture: Option<Arc<crate::capture::CaptureEndpoint>>,
    recovery: Option<Arc<crate::resize::ResizeState>>,
    /// Durable wrapper adopted in OnAfterCreated; Arc::clone is CEF-free.
    /// OnBeforeClose takes the slot out; task-local owners retire outside locks.
    native: Option<NativeBrowser>,
    close_mode: Option<CloseMode>,
    close_completion: Option<CloseCompletion>,
    close_prompt_cut: Option<ClosePromptCut>,
    policy_pending: BTreeSet<PolicyAction>,
    policy_posted: bool,
    close_sent: bool,
    /// Strongest close task already queued. Repeated identical host requests do
    /// not post duplicate UI work; only a Graceful -> Force escalation adds one.
    close_posted: Option<CloseMode>,
    /// A Force escalation must issue one new native close even when a Graceful
    /// close was already dispatched.
    force_sent: bool,
    /// Test-support only: keep the close intent/tombstone but do not dispatch
    /// the real native close, so a host/deadline can observe a genuinely live
    /// Browser during drainage.
    #[cfg(any(test, feature = "test-support"))]
    close_deferred: bool,
    /// Set by OnBeforeClose before any local release. While it is true the
    /// instance stays `Closing`: the terminal state is published only after the
    /// native reference, handler state and captured continuations are really
    /// gone, so the main thread can never observe a premature Ready.
    cleanup_started: bool,
    capture_cleanup_deferred: bool,
    /// True once a Created event was published, so exactly one matching Closed
    /// event is emitted and the host never sees an unbalanced close.
    created_reported: bool,
    /// True once `browser_host_create_browser` accepted the request. From then
    /// on only OnAfterCreated/OnBeforeClose can make the instance terminal.
    create_issued: bool,
    /// Armed only after CEF returned acceptance and before OnAfterCreated.
    #[cfg(feature = "test-support")]
    late_create_probe: Option<u8>,
    #[cfg(feature = "test-support")]
    command_pressure: Option<crate::test_support::CommandPressure>,
    /// One-shot view attachment claim. Sticky for the whole instance lifetime:
    /// a claim is never released, so a dropped unused token still forbids a
    /// second view from binding the same Browser.
    view_attachment_claimed: bool,
    input: Option<crate::input::NativeInput>,
    input_shadow: crate::input::NativeInput,
    input_outstanding: BTreeSet<u64>,
    input_deferred: Vec<CommandEnvelope>,
    input_reset_ready: bool,
    input_reset_posted: bool,
    input_cleanup_running: bool,
    deferred_force_dialog: Option<PendingBeforeUnload>,
    force_close_returned: bool,
    accepts_text: bool,
    close_waiting_input: Option<DeferredClose>,
    /// Latest owned IME composition geometry plus its revision. Never text and
    /// never a native reference; cleared by reset/close/draining and by a lost
    /// text target, so a late callback cannot revive a closed instance.
    ime: ImeState,
    ux_events: VecDeque<BrowserEvent>,
    js_bridge: JsBridgeConfig,
    headless_cdp_enabled: bool,
    js_messages: VecDeque<BrowserEvent>,
    eval_events: VecDeque<BrowserEvent>,
    ax_tree: Arc<crate::accessibility::RuntimeAccessibilityState>,
    native_hwnd: Option<crate::native_hwnd::ChildHwndState>,
}

struct DeferredClose {
    mode: CloseMode,
    ticket: Option<CloseTicket>,
}

impl Entry {
    fn matches_close_prompt(&self, cut: &ClosePromptCut) -> bool {
        !self.cleanup_started
            && self.session.snapshot().state == SessionState::Closing
            && self
                .close_prompt_cut
                .as_ref()
                .is_some_and(|current| current.same_attempt(cut))
    }

    fn restore_input(&mut self, mut ledger: crate::input::NativeInput) {
        ledger.admitted = self.input_shadow.admitted;
        ledger.rejected = self.input_shadow.rejected;
        if self.input_shadow.cleanup_failed {
            ledger.cleanup_failed = true;
            if ledger.last_native_error.is_none() {
                ledger.last_native_error = self.input_shadow.last_native_error.clone();
            }
        }
        self.input_shadow = ledger.clone();
        self.input = Some(ledger);
    }

    fn close_deferred(&self) -> bool {
        #[cfg(any(test, feature = "test-support"))]
        {
            self.close_deferred
        }
        #[cfg(not(any(test, feature = "test-support")))]
        {
            false
        }
    }
}

/// A continuation captured from a native UI callback. It is taken out of the
/// map and invoked exactly once outside every registry lock.
struct PendingRequest {
    cancel: Box<dyn FnOnce() + Send + 'static>,
}

pub(crate) struct UiRegistry {
    runtime: RuntimeGeneration,
    events: Arc<dyn BrowserEventSink>,
    frames: Arc<dyn FrameSink>,
    ui: Arc<dyn UiDispatcher>,
    state: Mutex<RegistryState>,
    /// Pending native continuations (beforeunload, dialogs, menus). This is a
    /// real request table, not a counter: each entry owns its cancel callback.
    requests: Mutex<BTreeMap<(u64, u64), PendingRequest>>,
    executor: OnceLock<Weak<Executor>>,
    /// Weak by design: the registry must not keep the profile table (and its
    /// native references) alive after the runtime released it.
    profiles: OnceLock<Weak<ProfileManager>>,
    flushes: OnceLock<Weak<CookieFlushRegistry>>,
    profile_cookies: OnceLock<Weak<crate::profile_cookies::ProfileCookieRegistry>>,
    policy: OnceLock<Arc<NativePolicy>>,
    dialogs: BeforeUnloadRequests,
    pub(crate) menus: crate::menu_native::NativeMenus,
    pub(crate) menu_bridge: crate::menu_bridge::MenuBridge,
    pub(crate) sources: Arc<crate::source_view::SourceViews>,
    pub(crate) finds: Arc<crate::find::Finds>,
    pub(crate) prints: crate::print_operations::PrintOperations,
    /// Accounting only; no DevTools capability is enabled until a real managed
    /// auxiliary client/window service supplies the native retirement evidence.
    pub(crate) devtools: Arc<crate::devtools::DevToolsLedger>,
    pub(crate) auxiliary_native: crate::devtools::resources::NativeAuxiliaries,
    pub(crate) auxiliary_windows: crate::devtools::windows::NativeWindows,
    pub(crate) host_requests: Arc<crate::host_requests::HostRequestManager>,
    pub(crate) pdf_exports: Arc<crate::pdf_export::PdfExportRegistry>,
    pub(crate) headless_cdp: Arc<crate::headless_cdp::HeadlessCdpManager>,
    pub(crate) outbound_drags: Arc<crate::outbound_drag::OutboundDragManager>,
    url_requests: OnceLock<Weak<crate::url_request::UrlRequestManager>>,
    #[cfg(feature = "test-support")]
    probe_dialogs: Mutex<BTreeMap<BrowserId, crate::dialog_probe::DialogProbe>>,
    #[cfg(feature = "test-support")]
    probe_renderers: Mutex<BTreeMap<BrowserId, crate::renderer_probe::RendererProbe>>,
    #[cfg(feature = "test-support")]
    probe_reset_notifications: Mutex<BTreeMap<BrowserId, u64>>,
    dialog_requested: AtomicU64,
    dialog_answered: AtomicU64,
    dialog_invalidated: AtomicU64,
    dialog_timeouts: AtomicU64,
    seq: AtomicU64,
    closed: AtomicU64,
    frame_count: AtomicU64,
    observed: AtomicU8,
    #[cfg(any(test, feature = "test-support"))]
    request_seq: AtomicU64,
    cancelled_requests: AtomicU64,
    /// Host opt-in: append a checked item and a one-level submenu so those
    /// forms are reachable from a real menu. CEF's windowless edit model has
    /// neither, so they can only exist as host-managed items.
    menu_structure: AtomicBool,
    host_menu_items: AtomicU64,
    /// How often CEF asked this host for a context-menu handler, and how often a
    /// bound menu bridge answered. Distinguishes "CEF never asked" from "asked
    /// but nothing was bound" without guessing from the outside.
    menu_handler_requests: AtomicU64,
    menu_handler_bound: AtomicU64,
    evals: Mutex<EvalLedger>,
    js_messages_dropped: AtomicU64,
    audit: Arc<NativeAudit>,
    windowless_rendering_enabled: AtomicBool,
}

/// RAII writable CEF UTF-16 output string (`CefString::default()` passes
/// `Borrowed(None)` -> `NULL`, which libcef's C-to-C++ wrapper rejects).
pub(crate) struct WritableCefString(cef::sys::_cef_string_utf16_t);

impl WritableCefString {
    pub(crate) fn new() -> Self {
        Self(unsafe { std::mem::zeroed() })
    }

    pub(crate) fn as_mut_cef(&mut self) -> CefString {
        CefString::from(&mut self.0 as *mut cef::sys::_cef_string_utf16_t)
    }

    pub(crate) fn as_mut_raw(&mut self) -> *mut cef::sys::_cef_string_utf16_t {
        &mut self.0
    }

    pub(crate) fn to_string_lossy(&self) -> String {
        CefString::from(&self.0 as *const cef::sys::_cef_string_utf16_t).to_string()
    }
}

impl Drop for WritableCefString {
    fn drop(&mut self) {
        unsafe {
            cef::sys::cef_string_utf16_clear(&mut self.0);
        }
    }
}

fn cef_feature_state(value: CefFeatureState) -> cef::State {
    match value {
        CefFeatureState::Default => cef::State::DEFAULT,
        CefFeatureState::Enabled => cef::State::ENABLED,
        CefFeatureState::Disabled => cef::State::DISABLED,
    }
}

fn build_browser_settings(
    options: &BrowserOptions,
    default_background_color: u32,
) -> BrowserSettings {
    BrowserSettings {
        windowless_frame_rate: i32::from(options.windowless_frame_rate.unwrap_or(60)),
        background_color: options.background_color.unwrap_or(default_background_color),
        default_encoding: options
            .default_encoding
            .as_deref()
            .map(CefString::from)
            .unwrap_or_default(),
        javascript: cef_feature_state(options.feature_states.javascript),
        local_storage: cef_feature_state(options.feature_states.local_storage),
        image_loading: cef_feature_state(options.feature_states.image_loading),
        tab_to_links: cef_feature_state(options.feature_states.tab_to_links),
        javascript_access_clipboard: cef_feature_state(
            options.feature_states.javascript_access_clipboard,
        ),
        ..Default::default()
    }
}

impl UiRegistry {
    pub(crate) fn new(
        runtime: RuntimeGeneration,
        events: Arc<dyn BrowserEventSink>,
        frames: Arc<dyn FrameSink>,
        ui: Arc<dyn UiDispatcher>,
        audit: Arc<NativeAudit>,
    ) -> Arc<Self> {
        let devtools = Arc::new(crate::devtools::DevToolsLedger::new(runtime));
        Arc::new(Self {
            runtime,
            events,
            frames,
            ui,
            state: Mutex::new(RegistryState {
                browsers: BTreeMap::new(),
                draining: false,
            }),
            requests: Mutex::new(BTreeMap::new()),
            executor: OnceLock::new(),
            profiles: OnceLock::new(),
            flushes: OnceLock::new(),
            profile_cookies: OnceLock::new(),
            policy: OnceLock::new(),
            dialogs: BeforeUnloadRequests::new(),
            menus: crate::menu_native::NativeMenus::new(),
            menu_bridge: crate::menu_bridge::MenuBridge::new(),
            sources: crate::source_view::SourceViews::new(),
            finds: crate::find::Finds::new(),
            prints: crate::print_operations::PrintOperations::default(),
            auxiliary_native: crate::devtools::resources::NativeAuxiliaries::new(devtools.clone()),
            auxiliary_windows: crate::devtools::windows::NativeWindows::new(devtools.clone()),
            host_requests: crate::host_requests::HostRequestManager::new(),
            pdf_exports: crate::pdf_export::PdfExportRegistry::new(),
            headless_cdp: crate::headless_cdp::HeadlessCdpManager::new(),
            outbound_drags: crate::outbound_drag::OutboundDragManager::new(),
            url_requests: OnceLock::new(),
            devtools,
            #[cfg(feature = "test-support")]
            probe_dialogs: Mutex::new(BTreeMap::new()),
            #[cfg(feature = "test-support")]
            probe_renderers: Mutex::new(BTreeMap::new()),
            #[cfg(feature = "test-support")]
            probe_reset_notifications: Mutex::new(BTreeMap::new()),
            dialog_requested: AtomicU64::new(0),
            dialog_answered: AtomicU64::new(0),
            dialog_invalidated: AtomicU64::new(0),
            dialog_timeouts: AtomicU64::new(0),
            seq: AtomicU64::new(0),
            closed: AtomicU64::new(0),
            frame_count: AtomicU64::new(0),
            observed: AtomicU8::new(0),
            #[cfg(any(test, feature = "test-support"))]
            request_seq: AtomicU64::new(0),
            cancelled_requests: AtomicU64::new(0),
            menu_structure: AtomicBool::new(false),
            host_menu_items: AtomicU64::new(0),
            menu_handler_requests: AtomicU64::new(0),
            menu_handler_bound: AtomicU64::new(0),
            evals: Mutex::new(EvalLedger::new()),
            js_messages_dropped: AtomicU64::new(0),
            audit,
            windowless_rendering_enabled: AtomicBool::new(true),
        })
    }

    pub(crate) fn set_windowless_rendering_enabled(&self, enabled: bool) {
        self.windowless_rendering_enabled
            .store(enabled, Ordering::Release);
    }

    pub(crate) fn windowless_rendering_enabled(&self) -> bool {
        self.windowless_rendering_enabled.load(Ordering::Acquire)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn note_probe_dialog_reset(&self, browser: BrowserId) {
        *self
            .probe_reset_notifications
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(browser)
            .or_default() += 1;
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn probe_dialog_reset_count(&self, browser: BrowserId) -> u64 {
        self.probe_reset_notifications
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&browser)
            .copied()
            .unwrap_or(0)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn probe_dialog_status(
        &self,
        browser: BrowserId,
        action: crate::dialog_probe::DialogProbeAction,
    ) -> Option<bool> {
        self.probe_dialogs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&browser)
            .and_then(|v| v.status(action))
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn probe_dialog_references(&self) -> usize {
        self.probe_dialogs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|v| v.has_reference())
            .count()
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn probe_renderer_enabled(&self, browser: BrowserId) -> Option<bool> {
        self.probe_renderers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&browser)
            .and_then(|v| v.enabled())
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn probe_renderer_crash_submitted(&self, browser: BrowserId) -> bool {
        self.probe_renderers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&browser)
            .is_some_and(|v| v.crash_submitted())
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn probe_renderer_references(&self) -> usize {
        self.probe_renderers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|v| v.has_reference())
            .count()
    }
    #[cfg(feature = "test-support")]
    fn retire_probe_renderer(&self, browser: BrowserId) {
        let probe = self
            .probe_renderers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&browser);
        if let Some(probe) = probe {
            probe.retire();
        }
    }
    #[cfg(feature = "test-support")]
    fn retire_probe_dialog(&self, browser: BrowserId) {
        let probe = self
            .probe_dialogs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&browser);
        if let Some(probe) = probe {
            probe.retire();
        }
    }
    pub(crate) fn snapshot_count(&self) -> usize {
        self.lock().browsers.len()
    }

    pub(crate) fn attach_executor(&self, executor: &Arc<Executor>) {
        let _ = self.executor.set(Arc::downgrade(executor));
    }

    pub(crate) fn attach_profiles(&self, profiles: &Arc<ProfileManager>) {
        let _ = self.profiles.set(Arc::downgrade(profiles));
    }

    pub(crate) fn attach_flushes(&self, flushes: &Arc<CookieFlushRegistry>) {
        let _ = self.flushes.set(Arc::downgrade(flushes));
    }

    pub(crate) fn attach_profile_cookies(
        &self,
        profile_cookies: &Arc<crate::profile_cookies::ProfileCookieRegistry>,
    ) {
        let _ = self.profile_cookies.set(Arc::downgrade(profile_cookies));
    }

    pub(crate) fn attach_policy(&self, policy: Arc<NativePolicy>) {
        let _ = self.policy.set(policy);
    }

    pub(crate) fn attach_url_requests(
        &self,
        url_requests: &Arc<crate::url_request::UrlRequestManager>,
    ) {
        let _ = self.url_requests.set(Arc::downgrade(url_requests));
    }

    pub(crate) fn url_requests(&self) -> Option<Arc<crate::url_request::UrlRequestManager>> {
        self.url_requests.get().and_then(Weak::upgrade)
    }

    pub(crate) fn begin_outbound_drag(
        &self,
        browser: BrowserId,
        payload: rust_cef_core::outbound_drag::DraggedPayload,
        allowed_ops: rust_cef_core::outbound_drag::DragOperationMask,
        x: i32,
        y: i32,
    ) -> Result<(), EngineError> {
        {
            let state = self.lock();
            if state.draining {
                return Err(EngineError::Closing);
            }
            let Some(entry) = state.browsers.get(&browser) else {
                return Err(EngineError::Closed);
            };
            if entry.cleanup_started || entry.session.snapshot().state != SessionState::Alive {
                return Err(EngineError::Closing);
            }
        }
        let started = self
            .outbound_drags
            .begin(browser, payload, allowed_ops, x, y)?;
        self.navigation_event(browser, BrowserEvent::StartOutboundDrag(started));
        Ok(())
    }

    pub(crate) fn update_outbound_drag_cursor(
        &self,
        browser: BrowserId,
        op: rust_cef_core::outbound_drag::DragOperationMask,
    ) {
        if self.outbound_drags.update_cursor(browser, op) {
            self.navigation_event(browser, BrowserEvent::OutboundDragCursorChanged(op));
        }
    }

    pub(crate) fn submit_outbound_drag_finish(
        &self,
        browser: BrowserId,
        executor: &Executor,
        finish: rust_cef_core::outbound_drag::OutboundDragFinish,
    ) -> Result<CommandId, EngineError> {
        if finish.drag_id.browser != browser || browser.runtime() != self.runtime {
            return Err(EngineError::WrongRuntime);
        }
        let mut state = self.lock();
        let entry = state
            .browsers
            .get_mut(&browser)
            .ok_or(EngineError::Closed)?;
        let envelope = entry.session.admit_outbound_drag_finish(finish)?;
        let id = envelope.id;
        if let Err(error) = executor.post_while_draining(TaskPayload::Command(envelope)) {
            entry.session.cancel_command(id);
            return Err(error);
        }
        Ok(id)
    }

    pub(crate) fn ax_tree_slot(
        &self,
        browser: BrowserId,
    ) -> Result<rust_cef_core::accessibility::AxTreeSlot, EngineError> {
        if browser.runtime() != self.runtime {
            return Err(EngineError::WrongRuntime);
        }
        let state = self.lock();
        let entry = state.browsers.get(&browser).ok_or(EngineError::Closed)?;
        if matches!(
            entry.session.snapshot().state,
            SessionState::Closed | SessionState::CreateFailed
        ) {
            return Err(EngineError::Closed);
        }
        Ok(entry.ax_tree.slot())
    }

    pub(crate) fn policy(&self) -> Option<Arc<NativePolicy>> {
        self.policy.get().cloned()
    }

    pub(crate) fn audit_native_entry(&self) {
        self.audit.record();
    }

    fn profiles(&self) -> Option<Arc<ProfileManager>> {
        self.profiles.get().and_then(Weak::upgrade)
    }

    fn executor(&self) -> Option<Arc<Executor>> {
        self.executor.get().and_then(Weak::upgrade)
    }

    /// The executor needs the same host dispatcher to publish completion
    /// notifications; it is plain owned data, not a GPUI borrow.
    pub(crate) fn dispatcher(&self) -> Arc<dyn UiDispatcher> {
        self.ui.clone()
    }

    pub(crate) fn runtime_generation(&self) -> RuntimeGeneration {
        self.runtime
    }

    pub(crate) fn begin_create(
        &self,
        browser: BrowserId,
        config: BrowserConfig,
    ) -> Result<(), CreateError> {
        if browser.runtime() != self.runtime {
            return Err(CreateError::WrongRuntime);
        }
        let session = Session::begin_create_for_runtime(
            browser,
            config.clone(),
            self.windowless_rendering_enabled(),
        )?;
        let native_hwnd = config
            .options
            .surface_mode
            .native_child_hwnd()
            .map(crate::native_hwnd::ChildHwndState::new);
        let mailbox_viewport = if native_hwnd.is_some() {
            Viewport {
                width: 1,
                height: 1,
                scale: 1.0,
            }
        } else {
            config.viewport
        };
        let store = new_frame_mailbox(browser, mailbox_viewport)
            .map_err(|_| CreateError::InvalidViewport)?;
        let ax_tree = crate::accessibility::RuntimeAccessibilityState::new(
            config.options.accessibility_state == CefFeatureState::Enabled,
        );
        let mut state = self.lock();
        state.browsers.insert(
            browser,
            Entry {
                session,
                document_epoch: 1,
                viewport: Arc::new(ViewportState::new(config.viewport)),
                store,
                client_state: None,
                #[cfg(feature = "test-support")]
                frame_probe_final: None,
                #[cfg(feature = "test-support")]
                menu_probe_final: None,
                capture: None,
                recovery: None,
                native: None,
                close_mode: None,
                close_completion: None,
                close_prompt_cut: None,
                policy_pending: BTreeSet::new(),
                policy_posted: false,
                close_sent: false,
                close_posted: None,
                force_sent: false,
                #[cfg(any(test, feature = "test-support"))]
                close_deferred: false,
                cleanup_started: false,
                capture_cleanup_deferred: false,
                created_reported: false,
                create_issued: false,
                #[cfg(feature = "test-support")]
                late_create_probe: None,
                #[cfg(feature = "test-support")]
                command_pressure: None,
                view_attachment_claimed: false,
                input: Some(crate::input::NativeInput::default()),
                input_shadow: crate::input::NativeInput::default(),
                input_outstanding: BTreeSet::new(),
                input_deferred: Vec::new(),
                input_reset_ready: false,
                input_reset_posted: false,
                input_cleanup_running: false,
                deferred_force_dialog: None,
                force_close_returned: false,
                accepts_text: false,
                close_waiting_input: None,
                ime: ImeState::new(browser),
                ux_events: VecDeque::new(),
                js_bridge: config.options.js_bridge,
                headless_cdp_enabled: config.options.headless_cdp_enabled,
                js_messages: VecDeque::new(),
                eval_events: VecDeque::new(),
                ax_tree,
                native_hwnd,
            },
        );
        Ok(())
    }

    pub(crate) fn set_draining(&self) {
        let auxiliary_closes = self.devtools.stop_admission();
        self.menu_bridge.retire_all();
        let mut state = self.lock();
        state.draining = true;
        // Admission is serialized by the same lifecycle lock. This sets
        // cancellation intent only; no host/provider code runs under it.
        self.prints.drain();
        let executor = self.executor();
        let mut ime_invalidated = false;
        for (browser, entry) in &mut state.browsers {
            // Drainage invalidates composition geometry for every instance,
            // including one that never admitted text input.
            ime_invalidated |= entry.ime.invalidate();
            let Some(executor) = executor.as_ref() else {
                continue;
            };
            if !matches!(
                entry.session.snapshot().state,
                SessionState::Alive | SessionState::Closing
            ) {
                continue;
            }
            if entry.input.as_ref().is_some_and(|input| input.admitted > 0)
                || !entry.input_outstanding.is_empty()
            {
                let _ = Self::queue_reset(*browser, entry, executor, InputResetReason::Draining);
            }
        }
        drop(state);
        self.post_auxiliary_closes(auxiliary_closes);
        if ime_invalidated {
            let _ = self.ui.wake();
        }
    }

    pub(crate) fn input_cleanup_pending(&self) -> bool {
        match self.state.try_lock() {
            Ok(state) => state.browsers.values().any(|entry| {
                !matches!(
                    entry.session.snapshot().state,
                    SessionState::Closed | SessionState::CreateFailed
                ) && (entry.session.input_reset_pending()
                    || !entry.input_outstanding.is_empty()
                    || entry.input_cleanup_running
                    || entry.session.input_retired())
            }),
            Err(_) => true,
        }
    }

    fn queue_reset(
        _browser: BrowserId,
        entry: &mut Entry,
        executor: &Executor,
        reason: InputResetReason,
    ) -> Result<CommandId, EngineError> {
        // This is the single admission point for every reset reason, and an
        // admitted reset advances the session epoch. Retained geometry from the
        // previous epoch therefore stops being usable here, including on the
        // failure paths. Callers compare the IME revision while holding this
        // same lock and wake after releasing it, even when posting fails.
        entry.ime.invalidate();
        let envelope = match entry.session.admit_input_reset(reason) {
            Ok(Some(envelope)) => envelope,
            Ok(None) => return entry.session.input_reset_id().ok_or(EngineError::Closed),
            // Before Ready no input could have executed, and after Closed no
            // native input target exists. Refusing reset is not failed cleanup.
            Err(error @ (EngineError::NotReady | EngineError::Closed)) => return Err(error),
            Err(error) => {
                entry.input_shadow.cleanup_failed = true;
                entry.input_shadow.last_native_error = Some(error.clone());
                if let Some(input) = entry.input.as_mut() {
                    input.cleanup_failed = true;
                    input.last_native_error = Some(error.clone());
                }
                return Err(error);
            }
        };
        let id = envelope.id;
        Self::post_reset(entry, executor, id)
    }

    fn post_reset(
        entry: &mut Entry,
        executor: &Executor,
        id: CommandId,
    ) -> Result<CommandId, EngineError> {
        if entry.input_reset_posted || entry.input_reset_ready {
            return Ok(id);
        }
        if let Err(error) = executor.post_while_draining(TaskPayload::InputReset(id)) {
            entry.input_shadow.cleanup_failed = true;
            entry.input_shadow.last_native_error = Some(error.clone());
            if let Some(input) = entry.input.as_mut() {
                input.cleanup_failed = true;
                input.last_native_error = Some(error.clone());
            }
            return Err(error);
        }
        entry.input_reset_posted = true;
        Ok(id)
    }

    fn retry_reset(
        entry: &mut Entry,
        executor: &Executor,
        id: CommandId,
    ) -> Result<CommandId, EngineError> {
        if entry.session.input_reset_id() != Some(id) {
            return Err(EngineError::InvalidRequest);
        }
        Self::post_reset(entry, executor, id)
    }

    pub(crate) fn set_accepts_text(&self, browser: BrowserId, accepts: bool) {
        let changed = {
            let mut state = self.lock();
            state.browsers.get_mut(&browser).is_some_and(|entry| {
                // A closed/failed instance must not be revived by a late keyboard
                // request; cleanup owns the terminal transition.
                if entry.cleanup_started
                    || matches!(
                        entry.session.snapshot().state,
                        SessionState::Closed | SessionState::CreateFailed
                    )
                {
                    return false;
                }
                let changed = entry.accepts_text != accepts;
                entry.accepts_text = accepts;
                let cleared = if accepts {
                    false
                } else {
                    // Losing the text target invalidates the previous anchor; a
                    // later composition callback must not reuse it.
                    entry.ime.invalidate()
                };
                cleared || changed
            })
        };
        if changed {
            let _ = self.ui.wake();
        }
    }

    /// Latest owned IME geometry for a complete Browser identity. A closed or
    /// failed instance reports `Closed`; a live instance that never produced a
    /// real callback reports `NotReady` rather than a synthesized zero
    /// observation (same rule as the default trait method).
    pub(crate) fn ime_snapshot(&self, browser: BrowserId) -> Result<ImeSnapshot, EngineError> {
        if browser.runtime() != self.runtime {
            return Err(EngineError::WrongRuntime);
        }
        let state = self.lock();
        let entry = state.browsers.get(&browser).ok_or(EngineError::Closed)?;
        if matches!(
            entry.session.snapshot().state,
            SessionState::Closed | SessionState::CreateFailed
        ) {
            return Err(EngineError::Closed);
        }
        if entry.ime.revision() == 0 {
            // No callback or invalidation was ever retained for this instance.
            return Err(EngineError::NotReady);
        }
        Ok(entry.ime.snapshot())
    }

    /// Records one adapter callback. The observed input epoch is read from the
    /// session at callback time; it is not carried by the native callback.
    /// Returns `Ok(None)` when the lifecycle gate refuses late geometry without
    /// mutating the retained state, and the accepted revision otherwise.
    ///
    /// `accepts_text == false` already cleared the slot when it was observed.
    /// It is not a permanent refusal: fixed CEF reports the text target only
    /// through `OnVirtualKeyboardRequested` (which the same reference gates on
    /// `show_ime_if_needed`), so treating the initial false as authoritative
    /// would silently disable geometry on hosts that never emit it. The
    /// component's own alive/epoch/composition gate owns usability.
    pub(crate) fn observe_ime(
        &self,
        browser: BrowserId,
        range: Option<Utf16Range>,
        bounds: ImeReportedBounds,
    ) -> Result<Option<u64>, EngineError> {
        if browser.runtime() != self.runtime {
            return Err(EngineError::WrongRuntime);
        }
        let (accepted, changed, revision) = {
            let mut state = self.lock();
            let draining = state.draining;
            let Some(entry) = state.browsers.get_mut(&browser) else {
                return Err(EngineError::Closed);
            };
            if draining
                || entry.cleanup_started
                || entry.ime.is_retired()
                || entry.session.input_reset_pending()
                || entry.session.snapshot().state != SessionState::Alive
            {
                // Draining, cleanup, revision exhaustion, a pending reset and
                // any non-Alive state (including Closing) already invalidated
                // the slot; a callback describing the previous composition may
                // not repopulate it or borrow the new epoch for old geometry.
                (false, false, entry.ime.revision())
            } else {
                let epoch = entry.session.input_epoch();
                let changed = entry.ime.observe(epoch, range, bounds);
                (true, changed, entry.ime.revision())
            }
        };
        if changed {
            // Owned data only; a geometry update wakes the host without waiting
            // for a frame, and never while the registry lock is held.
            let _ = self.ui.wake();
        }
        Ok(accepted.then_some(revision))
    }

    /// Latest retained revision without mutating or synthesizing state.
    fn ime_revision(&self, browser: BrowserId) -> Option<u64> {
        self.lock()
            .browsers
            .get(&browser)
            .map(|entry| entry.ime.revision())
    }

    /// Clears the retained geometry of one instance and wakes the host outside
    /// the registry lock. Returns whether anything was actually cleared.
    fn invalidate_ime(&self, browser: BrowserId) -> bool {
        let changed = self
            .lock()
            .browsers
            .get_mut(&browser)
            .is_some_and(|entry| entry.ime.invalidate());
        if changed {
            let _ = self.ui.wake();
        }
        changed
    }

    /// Clears geometry only if no callback was applied after `observed`.
    /// A synchronous CEF callback that lands during the native call is newer
    /// than the stale anchor this is retiring and must not be erased.
    fn invalidate_stale_ime(&self, browser: BrowserId, observed: u64) -> bool {
        let changed = self
            .lock()
            .browsers
            .get_mut(&browser)
            .is_some_and(|entry| entry.ime.revision() == observed && entry.ime.invalidate());
        if changed {
            let _ = self.ui.wake();
        }
        changed
    }

    /// Test-only fixture: the real handler body runs, but no CEF UI sequence or
    /// native Browser exists in a unit test. Applies the same session transition
    /// CEF would, so the production gate sees a real Alive instance.
    #[cfg(test)]
    pub(crate) fn prepare_ime_test_entry(&self, browser: BrowserId) {
        if let Some(entry) = self.lock().browsers.get_mut(&browser) {
            entry.session.after_created();
        }
    }

    pub(crate) fn input_snapshot(&self, browser: BrowserId) -> Result<InputSnapshot, EngineError> {
        if browser.runtime() != self.runtime {
            return Err(EngineError::WrongRuntime);
        }
        let state = self.lock();
        let entry = state.browsers.get(&browser).ok_or(EngineError::Closed)?;
        let mut snapshot = entry
            .input
            .as_ref()
            .unwrap_or(&entry.input_shadow)
            .snapshot(
                entry.session.input_epoch(),
                entry.session.input_reset_pending(),
            );
        snapshot.inflight = entry.input_outstanding.len();
        snapshot.native_execution_unconfirmed =
            entry.input.is_none() || entry.input_cleanup_running;
        snapshot.admitted = entry.input_shadow.admitted;
        snapshot.rejected = entry.input_shadow.rejected;
        snapshot.accepts_text = entry.accepts_text;
        Ok(snapshot)
    }

    pub(crate) fn is_draining(&self) -> bool {
        self.lock().draining
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_requests(&self) -> std::sync::MutexGuard<'_, BTreeMap<(u64, u64), PendingRequest>> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Registers a real pending continuation. Callers must still hold the
    /// registry state lock so draining cannot begin between the gate check and
    /// the insert. Returns `Closing` once drainage is visible; a rejected
    /// registration is never reported as a cancellation.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn inject_request(
        &self,
        browser: BrowserId,
        cancel: impl FnOnce() + Send + 'static,
    ) -> Result<RequestId, EngineError> {
        let state = self.lock();
        if state.draining {
            return Err(EngineError::Closing);
        }
        let Some(entry) = state.browsers.get(&browser) else {
            return Err(EngineError::Closed);
        };
        match entry.session.snapshot().state {
            SessionState::Alive => {}
            SessionState::Creating => return Err(EngineError::NotReady),
            _ => return Err(EngineError::Closing),
        }
        let sequence = self.request_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let id = RequestId { browser, sequence };
        self.lock_requests().insert(
            (browser.local(), sequence),
            PendingRequest {
                cancel: Box::new(cancel),
            },
        );
        Ok(id)
    }

    fn lock_evals(&self) -> std::sync::MutexGuard<'_, EvalLedger> {
        self.evals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn js_bridge_stats(&self) -> (u64, u64, u64, u64, u64) {
        let evals = self.lock_evals();
        (
            evals.pending_count(),
            evals.completed_count(),
            evals.cancelled_count(),
            evals.stale_dropped(),
            self.js_messages_dropped.load(Ordering::Acquire),
        )
    }

    fn eval_is_pending(&self, id: EvalRequestId) -> bool {
        self.lock_evals().contains(id)
    }

    fn record_eval_terminal(&self, browser: BrowserId, event: BrowserEvent) {
        {
            let mut state = self.lock();
            if let Some(entry) = state.browsers.get_mut(&browser)
                && !matches!(
                    entry.session.snapshot().state,
                    SessionState::Closed | SessionState::CreateFailed
                )
            {
                if entry.eval_events.len() >= 128 {
                    entry.eval_events.pop_front();
                }
                entry.eval_events.push_back(event.clone());
            }
        }
        self.emit(browser, event);
    }

    fn complete_eval(&self, id: EvalRequestId, expected_epoch: Option<u64>, outcome: EvalOutcome) {
        let resolved = self.lock_evals().complete(id, expected_epoch, outcome);
        if let Some((eval_id, outcome)) = resolved {
            self.record_eval_terminal(
                eval_id.browser,
                BrowserEvent::EvalCompleted {
                    id: eval_id,
                    outcome,
                },
            );
        }
    }

    fn cancel_browser_evals(&self, browser: BrowserId, reason: EvalError) {
        let cancelled = self.lock_evals().cancel_for_browser(browser, reason);
        for (eval_id, outcome) in cancelled {
            self.record_eval_terminal(
                eval_id.browser,
                BrowserEvent::EvalCompleted {
                    id: eval_id,
                    outcome,
                },
            );
        }
    }

    fn cancel_all_evals(&self, reason: EvalError) {
        let cancelled = self.lock_evals().cancel_all(reason);
        for (eval_id, outcome) in cancelled {
            self.record_eval_terminal(
                eval_id.browser,
                BrowserEvent::EvalCompleted {
                    id: eval_id,
                    outcome,
                },
            );
        }
    }

    pub(crate) fn audit_ref(&self) -> &NativeAudit {
        &self.audit
    }

    pub(crate) fn host_capabilities(&self) -> rust_cef_core::host_capabilities::HostCapabilities {
        self.policy
            .get()
            .map(|p| p.host_capabilities())
            .unwrap_or_default()
    }

    pub(crate) fn is_headless_cdp_enabled(
        &self,
        browser: BrowserId,
    ) -> Result<bool, rust_cef_core::host_capabilities::HostCapabilityError> {
        use rust_cef_core::host_capabilities::HostCapabilityError;
        if browser.runtime() != self.runtime {
            return Err(HostCapabilityError::WrongRuntime);
        }
        let state = self.lock();
        if state.draining {
            return Err(HostCapabilityError::BrowserClosed);
        }
        let Some(entry) = state.browsers.get(&browser) else {
            return Err(HostCapabilityError::BrowserClosed);
        };
        if entry.cleanup_started {
            return Err(HostCapabilityError::BrowserClosed);
        }
        match entry.session.snapshot().state {
            SessionState::Alive => Ok(entry.headless_cdp_enabled),
            SessionState::Creating => Err(HostCapabilityError::NotReady),
            _ => Err(HostCapabilityError::BrowserClosed),
        }
    }

    pub(crate) fn pending_requests(&self) -> u64 {
        let deferred_dialogs = self
            .lock()
            .browsers
            .values()
            .filter(|entry| entry.deferred_force_dialog.is_some())
            .count() as u64;
        self.lock_requests().len() as u64
            + deferred_dialogs
            + self.dialogs.pending()
            + self.menus.pending()
            + self.sources.pending()
            + self.finds.pending()
            + self.prints.pending()
            + self.devtools.pending()
            + self.lock_evals().pending_count()
            + self.host_requests.pending_count()
            + self.pdf_exports.pending_count()
            + self.headless_cdp.active_registrations()
            + self.outbound_drags.pending_count()
            + self.url_requests().map_or(0, |u| u.pending_count())
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn probe_arm_prints<const N: usize>(
        &self,
        owners: [rust_cef_core::ports::WindowBinding; N],
    ) -> Result<[rust_cef_core::print::PrintId; N], EngineError> {
        let state = self.lock();
        if state.draining {
            return Err(EngineError::Closing);
        }
        if !owners.iter().all(|binding| {
            state
                .browsers
                .get(&binding.browser)
                .is_some_and(|e| !e.cleanup_started && e.session.snapshot().is_ready())
        }) {
            return Err(EngineError::NotReady);
        }
        self.prints.probe_arm(self.runtime, owners)
    }

    pub(crate) fn cancelled_requests(&self) -> u64 {
        self.cancelled_requests.load(Ordering::Acquire)
    }

    fn take_browser_requests(&self, local: u64) -> Vec<PendingRequest> {
        let mut requests = self.lock_requests();
        let keys: Vec<_> = requests
            .range((local, 0)..=(local, u64::MAX))
            .map(|(key, _)| *key)
            .collect();
        keys.into_iter()
            .filter_map(|key| requests.remove(&key))
            .collect()
    }

    fn take_all_requests(&self) -> Vec<PendingRequest> {
        std::mem::take(&mut *self.lock_requests())
            .into_values()
            .collect()
    }

    /// Invoked from the CEF UI sequence. Callbacks run after the state and
    /// request locks are released, exactly once per taken continuation.
    fn cancel_requests(&self, requests: Vec<PendingRequest>) {
        let count = requests.len() as u64;
        for request in requests {
            let PendingRequest { cancel } = request;
            cancel();
        }
        self.cancelled_requests.fetch_add(count, Ordering::AcqRel);
    }

    pub(crate) fn cancel_all_requests(&self, on_ui: bool) {
        // Taking the state lock first serializes with `inject_request`, so a
        // concurrent registration either precedes this drain or observes
        // `draining` and is rejected.
        let _state = self.lock();
        let requests = self.take_all_requests();
        drop(_state);
        self.cancel_requests(requests);
        for work in self.menus.drain() {
            work.cancel(&self.audit);
        }
        let _ = self.sources.cancel(None, None, None);
        self.cancel_finds(None, None);
        self.request_auxiliary_close(None, None);
        self.cancel_all_evals(EvalError::Closed);
        let caps = self.host_capabilities();
        self.host_requests.cancel_all(&self.audit, &caps);
        self.headless_cdp.detach_all(on_ui);
        self.pdf_exports.close_all();
        if let Some(urls) = self.url_requests() {
            urls.drain_on_ui(on_ui);
        }
    }

    pub(crate) fn post_menu_cancel(
        &self,
        id: rust_cef_core::menu::ContextMenuRequestId,
        binding: rust_cef_core::ports::WindowBinding,
        revision: u64,
    ) -> Result<(), EngineError> {
        let executor = self.executor().ok_or(EngineError::Closed)?;
        let ticket = self.menus.reserve_cancel(id, binding, revision)?;
        if let Err(error) = executor.post(TaskPayload::MenuCancel(ticket)) {
            // Exact request + attempt only. Failure is not a native answer;
            // pending ownership remains for an explicit retry/close drainage.
            self.menus.rollback(ticket);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn lock_source_admission(
        &self,
        browser: BrowserId,
    ) -> Result<impl Drop + '_, EngineError> {
        let s = self.lock();
        if s.draining {
            return Err(EngineError::Closing);
        }
        if !s.browsers.get(&browser).is_some_and(|e| {
            !e.cleanup_started && e.session.snapshot().state == SessionState::Alive
        }) {
            return Err(EngineError::Closed);
        }
        Ok(s)
    }
    /// Host-facing managed DevTools open for one original Browser. Only the
    /// admission half exists: the preflight is real, but no Show/WindowInfo
    /// path is installed yet, so this never claims an auxiliary was opened.
    pub(crate) fn close_devtools(self: &Arc<Self>, browser: BrowserId) -> Result<(), EngineError> {
        {
            let s = self.lock();
            if s.draining {
                return Err(EngineError::Closing);
            }
        }
        let executor = self.executor().ok_or(EngineError::Closed)?;
        executor
            .post_close(TaskPayload::CloseDevTools { browser })
            .map(|_| ())
    }
    pub(crate) fn open_devtools(self: &Arc<Self>, browser: BrowserId) -> Result<(), EngineError> {
        {
            let s = self.lock();
            if s.draining {
                return Err(EngineError::Closing);
            }
        }
        self.devtools.preflight_open(browser)?;
        let executor = self.executor().ok_or(EngineError::Closed)?;
        executor
            .post_close(TaskPayload::OpenDevTools { browser })
            .map(|_| ())
    }
    pub(crate) fn devtools_tails(&self, browser: BrowserId) -> Option<String> {
        self.devtools.tails(browser)
    }
    /// Pure diagnostic: current auxiliary phase for this owner, if any.
    pub(crate) fn devtools_associated_after_close(&self, browser: BrowserId) -> Option<bool> {
        self.devtools.after_close(browser)
    }
    pub(crate) fn devtools_associated(&self, browser: BrowserId) -> Option<bool> {
        self.devtools.associated(browser)
    }
    pub(crate) fn devtools_phase(&self, browser: BrowserId) -> Option<&'static str> {
        self.devtools.phase_name(browser)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn probe_print(&self, browser: BrowserId) -> Result<(), EngineError> {
        self.executor()
            .ok_or(EngineError::Closed)?
            .post_close(TaskPayload::ProbePrint { browser })?;
        Ok(())
    }
    /// Pure diagnostic: a stored print dialog still awaiting the host.
    pub(crate) fn print_dialog_pending(&self, browser: BrowserId) -> bool {
        crate::print_provider::has_dialog(browser)
    }
    /// Main-frame document epoch, or None when the Browser is unknown.
    pub(crate) fn document_epoch(&self, browser: BrowserId) -> Option<u64> {
        self.lock()
            .browsers
            .get(&browser)
            .map(|entry| entry.document_epoch)
    }
    /// Pure diagnostic: print operations still awaiting a terminal result.
    /// Post one host decision for a pending print dialog onto the CEF UI lane.
    pub(crate) fn resolve_print_dialog(
        &self,
        browser: BrowserId,
        accept: bool,
    ) -> Result<(), EngineError> {
        self.executor()
            .ok_or(EngineError::Closed)?
            .post_close(TaskPayload::ResolvePrintDialog { browser, accept })?;
        Ok(())
    }
    pub(crate) fn print_pending(&self) -> u64 {
        self.prints.pending()
    }
    /// Pure diagnostic: how many auxiliary slots this owner still holds.
    pub(crate) fn devtools_pending(&self, browser: BrowserId) -> u64 {
        self.devtools.pending_for(browser)
    }
    /// Explicit opt-in for the managed DevTools auxiliary service. An
    /// unconfigured Runtime keeps the capability closed; the ledger admits a
    /// single configuration and refuses one after shutdown admission stops.
    pub(crate) fn configure_devtools(
        &self,
        capacity: std::num::NonZeroUsize,
    ) -> Result<(), EngineError> {
        {
            let s = self.lock();
            if s.draining {
                return Err(EngineError::Closing);
            }
        }
        self.devtools.configure(capacity)
    }
    pub(crate) fn note_menu_handler_request(&self, bound: bool) {
        self.menu_handler_requests.fetch_add(1, Ordering::AcqRel);
        if bound {
            self.menu_handler_bound.fetch_add(1, Ordering::AcqRel);
        }
    }

    pub(crate) fn menu_handler_counts(&self) -> (u64, u64) {
        (
            self.menu_handler_requests.load(Ordering::Acquire),
            self.menu_handler_bound.load(Ordering::Acquire),
        )
    }

    pub(crate) fn configure_menu_structure(&self, enabled: bool) -> Result<(), EngineError> {
        let s = self.lock();
        if s.draining {
            return Err(EngineError::Closing);
        }
        drop(s);
        self.menu_structure.store(enabled, Ordering::Release);
        Ok(())
    }

    pub(crate) fn menu_structure_enabled(&self) -> bool {
        self.menu_structure.load(Ordering::Acquire)
    }

    /// Real selection of a host-managed structural item, recorded so the
    /// receipt can distinguish it from a CEF edit command.
    pub(crate) fn note_host_menu_item(&self, command_id: i32) {
        self.host_menu_items
            .store(command_id as u64 | (1 << 32), Ordering::Release);
    }

    pub(crate) fn host_menu_item(&self) -> Option<i32> {
        let v = self.host_menu_items.load(Ordering::Acquire);
        (v >> 32 == 1).then_some((v & 0xffff_ffff) as i32)
    }

    pub(crate) fn configure_finds(
        &self,
        limits: rust_cef_core::find::FindLimits,
    ) -> Result<(), EngineError> {
        let s = self.lock();
        if s.draining {
            return Err(EngineError::Closing);
        }
        self.finds.configure(limits)
    }
    pub(crate) fn bind_spelling(
        &self,
        binding: &Arc<crate::menu_bridge::Binding>,
        config: crate::SpellingConfig,
    ) -> Result<(), EngineError> {
        let s = self.lock();
        if s.draining || binding.retired.load(Ordering::Acquire) {
            return Err(EngineError::Closing);
        }
        if !s.browsers.get(&binding.browser).is_some_and(|e| {
            !e.cleanup_started
                && matches!(
                    e.session.snapshot().state,
                    SessionState::Creating | SessionState::Alive
                )
        }) || !self
            .menu_bridge
            .get(binding.browser)
            .is_some_and(|b| Arc::ptr_eq(&b, binding))
        {
            return Err(EngineError::Closed);
        }
        binding.spelling.configure(config)?;
        drop(s);
        let _ = self.executor().map(|ex| {
            ex.post(TaskPayload::SpellingPreferences {
                browser: binding.browser,
            })
        });
        Ok(())
    }
    pub(crate) fn bind_find_results(
        &self,
        binding: &Arc<crate::menu_bridge::Binding>,
        sink: Arc<dyn rust_cef_core::find::FindResultSink>,
    ) -> Result<(), EngineError> {
        let s = self.lock();
        if s.draining || binding.retired.load(Ordering::Acquire) || !self.finds.configured() {
            return Err(EngineError::NotReady);
        }
        if !s.browsers.get(&binding.browser).is_some_and(|e| {
            !e.cleanup_started
                && matches!(
                    e.session.snapshot().state,
                    SessionState::Creating | SessionState::Alive
                )
        }) || !self
            .menu_bridge
            .get(binding.browser)
            .is_some_and(|b| Arc::ptr_eq(&b, binding))
        {
            return Err(EngineError::Closed);
        }
        let result = binding.find.set(sink);
        drop(s);
        result.map_err(|_| EngineError::InvalidRequest)
    }
    pub(crate) fn find_pristine(&self, browser: BrowserId) -> bool {
        self.lock()
            .browsers
            .get(&browser)
            .and_then(|e| e.client_state.as_ref())
            .is_some_and(|c| !c.find_unmanaged.load(Ordering::Acquire))
    }
    pub(crate) fn find_browser(&self, browser: BrowserId) -> Option<Browser> {
        let owner = {
            let state = self.lock();
            state
                .browsers
                .get(&browser)
                .filter(|e| !e.cleanup_started)
                .and_then(|e| e.native.clone())
        };
        // This API returns a CEF wrapper; its AddRef occurs AFTER the guard.
        owner.map(|native| native.as_ref().clone())
    }
    pub(crate) fn find_native_matches(&self, browser: BrowserId, native: &Browser) -> bool {
        self.find_browser(browser).is_some_and(|mut current| {
            self.audit.record();
            native.is_valid() != 0 && native.is_same(Some(&mut current)) != 0
        })
    }
    pub(crate) fn post_find_cleanup(
        &self,
        id: rust_cef_core::find::FindOperationId,
    ) -> Result<(), EngineError> {
        self.executor()
            .ok_or(EngineError::Closed)?
            .post_close(TaskPayload::FindCancel(id))
            .map(|_| ())
    }
    pub(crate) fn configure_source_views(
        &self,
        limits: rust_cef_core::source_view::SourceViewLimits,
    ) -> Result<(), EngineError> {
        let s = self.lock();
        if s.draining {
            return Err(EngineError::Closing);
        }
        self.sources.configure(limits)
    }
    pub(crate) fn bind_source_viewer(
        &self,
        binding: &Arc<crate::menu_bridge::Binding>,
        viewer: Arc<dyn rust_cef_core::source_view::SourceViewSink>,
    ) -> Result<(), EngineError> {
        let s = self.lock();
        if s.draining || binding.retired.load(Ordering::Acquire) {
            return Err(EngineError::Closed);
        }
        if !self.sources.configured() {
            return Err(EngineError::NotReady);
        }
        if !s.browsers.get(&binding.browser).is_some_and(|e| {
            !e.cleanup_started
                && matches!(
                    e.session.snapshot().state,
                    SessionState::Creating | SessionState::Alive
                )
        }) || !self
            .menu_bridge
            .get(binding.browser)
            .is_some_and(|b| Arc::ptr_eq(&b, binding))
        {
            return Err(EngineError::Closed);
        }
        let result = binding.source.set(viewer);
        drop(s);
        result.map_err(|_| EngineError::InvalidRequest)
    }
    pub(crate) fn post_source_result(
        &self,
        id: rust_cef_core::source_view::SourceOperationId,
        result: Result<
            rust_cef_core::source_view::SourceText,
            rust_cef_core::source_view::SourceFailure,
        >,
    ) -> Result<(), EngineError> {
        self.executor()
            .ok_or(EngineError::Closed)?
            .post(TaskPayload::SourceResult { id, result })?;
        Ok(())
    }
    pub(crate) fn post_source_release(
        &self,
        id: rust_cef_core::source_view::SourceOperationId,
    ) -> Result<(), EngineError> {
        self.executor()
            .ok_or(EngineError::Closed)?
            .post_close(TaskPayload::SourceReleased(id))?;
        Ok(())
    }
    /// Original normalized metadata only; no fallback or native context clone.
    pub(crate) fn recorded_auxiliary_profile(
        &self,
        browser: BrowserId,
    ) -> Result<rust_cef_core::contracts::ProfileHandle, EngineError> {
        if browser.runtime() != self.runtime {
            return Err(EngineError::WrongRuntime);
        }
        self.lock()
            .browsers
            .get(&browser)
            .and_then(|e| e.session.configuration().profile)
            .filter(|p| p.runtime() == self.runtime)
            .ok_or(EngineError::NotReady)
    }
    pub(crate) fn note_auxiliary_native_call(&self) {
        self.audit.record();
    }
    pub(crate) fn post_auxiliary_client_released(
        &self,
        id: crate::devtools::AuxiliaryId,
        native_id: i32,
    ) -> Result<(), EngineError> {
        self.executor()
            .ok_or(EngineError::Closed)?
            .post_close(TaskPayload::AuxiliaryClientReleased { id, native_id })?;
        Ok(())
    }
    pub(crate) fn post_auxiliary_references_released(
        &self,
        id: crate::devtools::AuxiliaryId,
        native_id: i32,
    ) -> Result<(), EngineError> {
        self.executor()
            .ok_or(EngineError::Closed)?
            .post_close(TaskPayload::AuxiliaryReferencesReleased { id, native_id })?;
        Ok(())
    }
    pub(crate) fn post_auxiliary_native_released(
        &self,
        id: crate::devtools::AuxiliaryId,
        native_id: i32,
    ) -> Result<(), EngineError> {
        self.executor()
            .ok_or(EngineError::Closed)?
            .post_close(TaskPayload::AuxiliaryNativeReleased { id, native_id })?;
        Ok(())
    }
    /// Data-only pre-native reservation under the ORIGINAL lifecycle gate.
    /// Caller owns the menu work and its original lease permit; actual native
    /// Browser/Frame/Host/context/window-policy checks still precede any Show.
    pub(crate) fn reserve_auxiliary_menu(
        &self,
        scope: &crate::menu_bridge::Scope,
        menu: rust_cef_core::menu::ContextMenuRequestId,
    ) -> Result<crate::devtools::AuxiliaryId, EngineError> {
        if menu.browser != scope.browser || menu.sequence == 0 {
            return Err(EngineError::InvalidRequest);
        }
        self.with_auxiliary_menu_owner(scope, |_| {
            self.devtools
                .reserve(scope.browser, scope.serial, scope.native_id)
        })
    }

    // This private helper is only used with the pure ledger operations below.
    // No CEF/user callbacks or native wrapper drops under the Registry guard.
    fn with_auxiliary_menu_owner<T>(
        &self,
        scope: &crate::menu_bridge::Scope,
        record: impl FnOnce(rust_cef_core::contracts::ProfileHandle) -> Result<T, EngineError>,
    ) -> Result<T, EngineError> {
        if scope.context.binding != scope.stamp.binding()
            || scope.context.binding_revision != scope.stamp.revision()
            || scope.stamp.binding().browser != scope.browser
            || scope.context.frame_id != scope.frame_id
            || scope.frame_id.is_empty()
            || scope.native_id <= 0
        {
            return Err(EngineError::InvalidRequest);
        }
        let registry = scope.registry.upgrade().ok_or(EngineError::Closed)?;
        if !std::ptr::eq(Arc::as_ptr(&registry), self) {
            return Err(EngineError::WrongRuntime);
        }
        // Hold these Rust owners BEFORE the lifecycle guard so even rejection
        // releases opaque target allocations only after the guard has gone.
        let original = scope.registration.upgrade().ok_or(EngineError::Closed)?;
        let current = self
            .menu_bridge
            .get(scope.browser)
            .ok_or(EngineError::Closed)?;
        if !Arc::ptr_eq(&original, &current) {
            return Err(EngineError::InvalidRequest);
        }
        if !scope.current() {
            return Err(EngineError::InvalidRequest);
        }
        let state = self.lock();
        if state.draining || original.retired.load(Ordering::Acquire) {
            return Err(EngineError::Closing);
        }
        if original.browser != scope.browser
            || original.serial != scope.serial
            || original.epoch.load(Ordering::Acquire) != scope.epoch
        {
            return Err(EngineError::InvalidRequest);
        }
        let entry = state
            .browsers
            .get(&scope.browser)
            .ok_or(EngineError::Closed)?;
        if entry.cleanup_started || entry.session.snapshot().state != SessionState::Alive {
            return Err(EngineError::Closing);
        }
        let profile = entry
            .session
            .configuration()
            .profile
            .ok_or(EngineError::NotReady)?;
        if profile.runtime() != self.runtime {
            return Err(EngineError::WrongRuntime);
        }
        record(profile)
    }

    pub(crate) fn preflight_auxiliary_menu_preparation(
        &self,
        scope: &crate::menu_bridge::Scope,
        id: crate::devtools::AuxiliaryId,
    ) -> Result<(), EngineError> {
        self.with_auxiliary_preparation(scope, id, |_| Ok(()))
    }

    pub(crate) fn attach_auxiliary_menu_context(
        &self,
        scope: &crate::menu_bridge::Scope,
        id: crate::devtools::AuxiliaryId,
    ) -> Result<(), EngineError> {
        self.with_auxiliary_preparation(scope, id, |_| self.devtools.attach_context(id))
    }

    pub(crate) fn attach_auxiliary_menu_client(
        &self,
        scope: &crate::menu_bridge::Scope,
        id: crate::devtools::AuxiliaryId,
    ) -> Result<rust_cef_core::contracts::ProfileHandle, EngineError> {
        self.with_auxiliary_preparation(scope, id, |profile| {
            self.devtools.attach_prepared_client(id, profile)?;
            Ok(profile)
        })
    }

    fn with_auxiliary_preparation<T>(
        &self,
        scope: &crate::menu_bridge::Scope,
        id: crate::devtools::AuxiliaryId,
        record: impl FnOnce(rust_cef_core::contracts::ProfileHandle) -> Result<T, EngineError>,
    ) -> Result<T, EngineError> {
        if id.owner != scope.browser || id.serial != scope.serial {
            return Err(EngineError::InvalidRequest);
        }
        self.with_auxiliary_menu_owner(scope, |profile| {
            self.devtools
                .preflight_reserved_origin(id, scope.native_id)?;
            record(profile)
        })
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn post_auxiliary_preparation_probe(
        &self,
        request: rust_cef_core::menu::ContextMenuRequestId,
        binding: rust_cef_core::ports::WindowBinding,
        revision: u64,
    ) -> Result<crate::test_support::AuxiliaryPreparationTicket, EngineError> {
        if request.browser != binding.browser
            || request.browser.runtime() != self.runtime
            || self.is_draining()
        {
            return Err(EngineError::InvalidRequest);
        }
        let parent = self.snapshot(request.browser);
        if !parent.is_ready()
            || parent.navigation.address != crate::menu_bridge_fixture::fixture_url()
        {
            return Err(EngineError::InvalidRequest);
        }
        let executor = self.executor().ok_or(EngineError::Closed)?;
        let reservation = self.menus.reserve_cancel(request, binding, revision)?;
        let witness = crate::test_support::AuxiliaryPreparationTicket::default();
        if let Err(e) = executor.post(TaskPayload::ProbeAuxiliaryPreparation {
            request,
            reservation,
            witness: witness.clone(),
        }) {
            self.menus.rollback(reservation);
            return Err(e);
        }
        Ok(witness)
    }

    #[cfg(feature = "test-support")]
    fn probe_prepare_unissued_auxiliary(
        self: &Arc<Self>,
        request: rust_cef_core::menu::ContextMenuRequestId,
        work: &crate::menu_native::MenuWork,
    ) -> Result<crate::test_support::AuxiliaryPreparationReport, EngineError> {
        use crate::test_support::AuxiliaryPreparationReport;
        if work.request() != request
            || !work.is_managed_native()
            || self.snapshot(request.browser).navigation.address
                != crate::menu_bridge_fixture::fixture_url()
        {
            return Err(EngineError::InvalidRequest);
        }
        let pending_before = self.devtools.pending();
        if pending_before != 0 {
            return Err(EngineError::InvalidRequest);
        }
        // Only the private accounting fixture is reserved. No menu capability
        // is enabled and there is no native auxiliary creation/Show path here.
        self.devtools.configure(std::num::NonZeroUsize::MIN)?;
        let mut prepared = crate::devtools::prepare::PreparedAuxiliary::from_menu(self, work)?;
        prepared.prepare_client()?;
        let pending_prepared = self.devtools.pending();
        let owner = prepared.id().owner;
        // Actual RequestContext and CEF-owned Rust client are retired on the
        // original CEF UI sequence, before the original menu is answered.
        drop(prepared);
        let pending_released = self.devtools.pending();
        if pending_prepared != 1 || owner != request.browser || pending_released != 0 {
            return Err(EngineError::InvalidRequest);
        }
        Ok(AuxiliaryPreparationReport {
            browser: (owner.runtime().value(), owner.local()),
            menu_sequence: request.sequence,
            cef_ui_thread: crate::executor::current_thread_id(),
            pending_before,
            pending_prepared,
            pending_released,
        })
    }

    /// Explicit future Show completion on CEF UI; no creation/focus is issued.
    /// Failure keeps issued ownership pending, never guesses a replacement HWND.
    pub(crate) fn capture_auxiliary_window(
        &self,
        id: crate::devtools::AuxiliaryId,
        native_id: i32,
    ) -> Result<(), EngineError> {
        self.auxiliary_windows.capture(self, id, native_id)
    }
    /// Explicit later observation if the two tail samples still found a window.
    /// No timer, requeue or implicit retry is installed by this operation.
    pub(crate) fn observe_auxiliary_window(
        &self,
        id: crate::devtools::AuxiliaryId,
        native_id: i32,
    ) -> Result<bool, EngineError> {
        self.auxiliary_windows.observe(self, id, native_id)
    }
    pub(crate) fn schedule_auxiliary_close(&self, id: crate::devtools::AuxiliaryId) {
        self.post_auxiliary_closes(vec![id]);
    }
    fn request_auxiliary_close(&self, browser: Option<BrowserId>, serial: Option<u64>) {
        self.post_auxiliary_closes(self.devtools.request_close(browser, serial));
    }
    fn post_auxiliary_closes(&self, ids: Vec<crate::devtools::AuxiliaryId>) {
        for id in ids {
            let ticket = match self.devtools.reserve_close(id) {
                Ok(Some(ticket)) => ticket,
                Ok(None) => continue,
                Err(_) => {
                    self.devtools.fault();
                    continue;
                }
            };
            let result = self
                .executor()
                .ok_or(EngineError::Closed)
                .and_then(|e| e.post_close(TaskPayload::AuxiliaryClose(ticket)));
            if result.is_err() {
                self.devtools.close_post_failed(ticket);
            }
        }
    }
    pub(crate) fn configure_context_menus(
        &self,
        config: crate::ContextMenuConfig,
    ) -> Result<(), EngineError> {
        let state = self.lock();
        if state.draining {
            return Err(EngineError::Closed);
        }
        self.menu_bridge.configure(config, || {
            self.menus.configure(config.capacity, config.model_limits)
        })
    }
    pub(crate) fn bind_context_menu(
        self: &Arc<Self>,
        target: rust_cef_core::ports::ContextMenuTarget,
    ) -> Result<crate::ContextMenuRegistration, EngineError> {
        let snapshot = target
            .lease
            .snapshot()
            .map_err(|_| EngineError::InvalidRequest)?;
        let browser = snapshot.binding.browser;
        if snapshot.retired
            || snapshot.faulted
            || snapshot.binding.window == 0
            || browser.runtime() != self.runtime
        {
            return Err(EngineError::InvalidRequest);
        }
        let state = self.lock();
        let entry = state.browsers.get(&browser).ok_or(EngineError::Closed)?;
        if state.draining
            || entry.cleanup_started
            || !matches!(
                entry.session.snapshot().state,
                SessionState::Creating | SessionState::Alive
            )
        {
            return Err(EngineError::Closed);
        }
        let (binding, old) = self.menu_bridge.bind(
            browser,
            &target,
            self.menus.pending_for(browser) == 0
                && self.sources.pending_for(browser) == 0
                && self.finds.pending_for(browser) == 0
                && !self.prints.pending_for(browser)
                && self.devtools.pending_for(browser) == 0,
        )?;
        drop(state);
        drop(old); // Arbitrary host sink destruction must be outside lifecycle gate too.
        Ok(crate::ContextMenuRegistration {
            binding,
            registry: Arc::downgrade(self),
        })
    }
    pub(crate) fn post_menu_retirement(
        &self,
        browser: BrowserId,
        serial: u64,
    ) -> Result<(), EngineError> {
        // Record intent even if posting fails; never discard a live operation.
        if let Some(registration) = std::num::NonZeroU64::new(serial) {
            self.prints.invalidate(browser, Some(registration));
        }
        self.request_auxiliary_close(Some(browser), Some(serial));
        let result = self
            .executor()
            .ok_or(EngineError::Closed)?
            .post(TaskPayload::RetireMenuBinding { browser, serial });
        if let Err(e) = &result {
            if let Some(b) = self.menu_bridge.get(browser).filter(|b| b.serial == serial) {
                b.note(|s| s.post_failures += 1);
                b.fail(format!("menu retirement post refused: {e:?}"));
            }
        }
        result.map(|_| ())
    }
    pub(crate) fn post_menu_response(
        &self,
        id: rust_cef_core::menu::ContextMenuRequestId,
        scope: &Arc<crate::menu_bridge::Scope>,
        response: rust_cef_core::menu::ContextMenuResponse,
    ) -> Result<(), EngineError> {
        if matches!(
            response,
            rust_cef_core::menu::ContextMenuResponse::Select { .. }
        ) {
            if !scope.admission()
                || self.snapshot(id.browser).state != SessionState::Alive
                || self.is_draining()
            {
                return Err(EngineError::InvalidRequest);
            }
        }
        let executor = self.executor().ok_or(EngineError::Closed)?;
        let ticket = self.menus.reserve_response(
            id,
            scope.stamp.binding(),
            scope.stamp.revision(),
            response,
        )?;
        if let Err(e) = executor.post(TaskPayload::MenuReply(ticket)) {
            self.menus.rollback(ticket);
            if let Some(b) = scope.registration.upgrade() {
                b.note(|s| s.post_failures += 1);
                b.fail(format!("menu response post refused: {e:?}"));
            }
            return Err(e);
        }
        Ok(())
    }
    pub(crate) fn menu_audit(&self) -> &NativeAudit {
        &self.audit
    }
    pub(crate) fn menu_wake(&self) -> Result<(), EngineError> {
        self.ui.wake()
    }
    pub(crate) fn menu_native_matches(&self, browser: BrowserId, native: &Browser) -> bool {
        let current = {
            let s = self.lock();
            s.browsers
                .get(&browser)
                .filter(|e| {
                    !s.draining
                        && !e.cleanup_started
                        && e.session.snapshot().state == SessionState::Alive
                })
                .and_then(|e| e.native.clone())
        };
        current.is_some_and(|current| {
            let mut current = current.as_ref().clone();
            self.audit.record();
            native.is_valid() != 0 && native.is_same(Some(&mut current)) != 0
        })
    }
    pub(crate) fn menu_original_frame(
        &self,
        scope: &crate::menu_bridge::Scope,
    ) -> Option<(Browser, cef::Frame)> {
        let native = {
            let s = self.lock();
            s.browsers
                .get(&scope.browser)
                .filter(|e| {
                    !s.draining
                        && !e.cleanup_started
                        && e.session.snapshot().state == SessionState::Alive
                })
                .and_then(|e| e.native.clone())
        }?;
        let native = native.as_ref().clone();
        self.audit.record();
        if native.is_valid() == 0 || native.identifier() != scope.native_id {
            return None;
        }
        let id = cef::CefString::from(scope.frame_id.as_str());
        self.audit.record();
        let frame = native.frame_by_identifier(Some(&id))?;
        if !self.menu_original_frame_valid(scope, &native, &frame) {
            return None;
        }
        Some((native, frame))
    }
    pub(crate) fn menu_original_frame_valid(
        &self,
        scope: &crate::menu_bridge::Scope,
        native: &Browser,
        frame: &cef::Frame,
    ) -> bool {
        use cef::ImplFrame;
        if self.is_draining() || self.snapshot(scope.browser).state != SessionState::Alive {
            return false;
        }
        self.audit.record();
        if native.is_valid() == 0
            || native.identifier() != scope.native_id
            || frame.is_valid() == 0
            || frame.is_focused() == 0
        {
            return false;
        }
        self.audit.record();
        let Some(mut owner) = frame.browser() else {
            return false;
        };
        if native.is_same(Some(&mut owner)) == 0 {
            return false;
        }
        crate::menu_model::read_userfree(&frame.identifier(), scope.frame_id.len())
            .is_ok_and(|id| id == scope.frame_id)
    }
    pub(crate) fn invalidate_native_menu(&self, browser: BrowserId) {
        if let Some(b) = self.menu_bridge.get(browser) {
            b.advance();
            let _ = self.sources.cancel(Some(browser), Some(b.serial), None);
            self.cancel_finds(Some(browser), Some(b.serial));
            if let Some(work) = self.menus.take_browser(browser) {
                work.cancel(&self.audit);
            }
        }
    }

    pub(crate) fn dialog_stats(&self) -> (u64, u64, u64, u64) {
        (
            self.dialog_requested.load(Ordering::Acquire),
            self.dialog_answered.load(Ordering::Acquire),
            self.dialog_invalidated.load(Ordering::Acquire),
            self.dialog_timeouts.load(Ordering::Acquire),
        )
    }

    fn before_unload_status(&self, browser: BrowserId) -> Option<(Option<ClosePromptCut>, bool)> {
        let mut state = self.lock();
        let entry = state.browsers.get_mut(&browser)?;
        let phase = entry.session.snapshot().state;
        if entry.cleanup_started
            || matches!(phase, SessionState::Closed | SessionState::CreateFailed)
        {
            return None;
        }
        let force = entry.close_mode == Some(CloseMode::Force);
        let cut = (phase == SessionState::Closing && !force).then(|| {
            entry
                .close_prompt_cut
                .get_or_insert_with(ClosePromptCut::default)
                .clone()
        });
        Some((cut, force))
    }

    pub(crate) fn before_unload(
        &self,
        browser: BrowserId,
        is_reload: bool,
        callback: JsdialogCallback,
    ) {
        self.dialog_requested.fetch_add(1, Ordering::Relaxed);
        let Some((close_cut, force)) = self.before_unload_status(browser) else {
            self.audit.record();
            callback.cont(0, None);
            return;
        };
        if force {
            // Initial Force can still invoke this handler in the fixed Alloy
            // implementation. Do not publish a blocking host question.
            self.audit.record();
            callback.cont(1, None);
            self.dialog_answered.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let request =
            match self
                .dialogs
                .insert(browser, callback, close_cut, std::time::Instant::now())
            {
                Ok(request) => request,
                Err(callback) => {
                    self.audit.record();
                    callback.cont(0, None);
                    return;
                }
            };
        if !self.schedule_dialog_sweep(request) {
            if let Some(pending) = self.dialogs.take(request) {
                self.finish_dialog(pending, false);
            }
            return;
        }
        if self.emit(
            browser,
            BrowserEvent::BeforeUnloadRequested { request, is_reload },
        ) != PublishResult::Published
        {
            if let Some(pending) = self.dialogs.take(request) {
                self.finish_dialog(pending, false);
            }
        }
    }

    fn schedule_dialog_sweep(&self, request: RequestId) -> bool {
        self.executor().is_some_and(|executor| {
            executor
                .post_delayed_while_draining(TaskPayload::BeforeUnloadSweep { request }, 100)
                .is_ok()
        })
    }

    fn sweep_before_unload(&self, request: RequestId) {
        let Some(deadline) = self.dialogs.deadline(request) else {
            return;
        };
        if std::time::Instant::now() >= deadline {
            self.dialog_timeouts.fetch_add(1, Ordering::Relaxed);
            if let Some(pending) = self.dialogs.take(request) {
                self.finish_dialog(pending, false);
            }
        } else if !self.schedule_dialog_sweep(request) {
            // This UI task still owns the gate: a failed continuation post must
            // not strand an unanswered native dialog without a timer.
            if let Some(pending) = self.dialogs.take(request) {
                self.finish_dialog(pending, false);
            }
        }
    }

    fn submit_before_unload_response(
        &self,
        browser: BrowserId,
        executor: &Executor,
        request: RequestId,
        allow: bool,
    ) -> Result<CommandId, EngineError> {
        if request.browser != browser || browser.runtime() != self.runtime {
            return Err(EngineError::WrongRuntime);
        }
        let mut state = self.lock();
        let entry = state
            .browsers
            .get_mut(&browser)
            .ok_or(EngineError::Closed)?;
        if !self.dialogs.reserve_reply(request) {
            return Err(EngineError::InvalidRequest);
        }
        let envelope = match entry.session.admit_dialog_response(request, allow) {
            Ok(envelope) => envelope,
            Err(error) => {
                self.dialogs.unreserve_reply(request);
                return Err(error);
            }
        };
        let id = envelope.id;
        if let Err(error) = executor.post_while_draining(TaskPayload::Command(envelope)) {
            entry.session.cancel_command(id);
            self.dialogs.unreserve_reply(request);
            return Err(error);
        }
        Ok(id)
    }

    fn answer_before_unload(&self, request: RequestId, allow: bool) -> Result<(), EngineError> {
        let pending = self
            .dialogs
            .take(request)
            .ok_or(EngineError::InvalidRequest)?;
        self.finish_dialog(pending, allow);
        Ok(())
    }

    fn restore_cancelled_close(entry: &mut Entry, cut: Option<&ClosePromptCut>) -> bool {
        if entry.close_mode == Some(CloseMode::Force)
            || !cut.is_some_and(|cut| entry.matches_close_prompt(cut))
        {
            return false;
        }
        entry.session.close_cancelled();
        entry.close_mode = None;
        entry.close_prompt_cut = None;
        entry.close_sent = false;
        entry.close_posted = None;
        entry.close_waiting_input = None;
        entry.force_sent = false;
        if let Some(completion) = entry.close_completion.take() {
            completion.complete(CloseResult::Cancelled);
        }
        true
    }

    fn finish_dialog(&self, pending: PendingBeforeUnload, allow: bool) {
        let browser = pending.id.browser;
        enum Outcome {
            Answer { allow: bool, cancelled: bool },
            HoldForForce { dispatch: bool },
        }
        let mut pending = Some(pending);
        let denied = Outcome::Answer {
            allow: false,
            cancelled: false,
        };
        let outcome = {
            let mut state = self.lock();
            let cut = pending
                .as_ref()
                .and_then(|dialog| dialog.close_cut.as_ref());
            match state.browsers.get_mut(&browser) {
                // An old close dialog cannot revoke or join a successor.
                Some(entry) if cut.is_some_and(|cut| !entry.matches_close_prompt(cut)) => denied,
                Some(entry) if entry.close_mode == Some(CloseMode::Force) => {
                    if entry.force_close_returned || entry.deferred_force_dialog.is_some() {
                        denied
                    } else {
                        // Retain the continuation through the actual native
                        // Force call. Never replace a live callback under lock.
                        entry.deferred_force_dialog = pending.take();
                        Outcome::HoldForForce {
                            dispatch: !entry.force_sent && !entry.session.input_reset_pending(),
                        }
                    }
                }
                Some(entry) => Outcome::Answer {
                    allow,
                    cancelled: !allow && Self::restore_cancelled_close(entry, cut),
                },
                None => denied,
            }
        };
        match outcome {
            Outcome::Answer { allow, cancelled } => {
                self.audit.record();
                pending.expect("reply was not deferred").answer(allow);
                self.dialog_answered.fetch_add(1, Ordering::Relaxed);
                if cancelled {
                    self.emit(browser, BrowserEvent::CloseCancelled);
                }
            }
            Outcome::HoldForForce { dispatch: true } => self.close_now(browser, CloseMode::Force),
            Outcome::HoldForForce { dispatch: false } => {}
        }
    }

    pub(crate) fn reset_dialogs(&self, browser: BrowserId) {
        let removed = self.dialogs.take_browser(browser);
        let cancelled = self.lock().browsers.get_mut(&browser).is_some_and(|entry| {
            removed
                .iter()
                .any(|pending| Self::restore_cancelled_close(entry, pending.close_cut.as_ref()))
        });
        self.dialog_invalidated
            .fetch_add(removed.len() as u64, Ordering::Relaxed);
        // Fixed CEF callback destruction can answer(false) and reenter reset.
        // Everything was taken, and every lock was released before this drop.
        drop(removed);
        let caps = self.host_capabilities();
        self.host_requests
            .cancel_js_dialogs_for_browser(browser, &self.audit, &caps);
        if cancelled {
            self.emit(browser, BrowserEvent::CloseCancelled);
        }
    }

    /// The close scheduling path. Failure leaves requests pending and is
    /// reported through `pending_requests`, never as a fake cancellation.
    pub(crate) fn post_drain_requests(self: &Arc<Self>) -> Result<(), EngineError> {
        let Some(executor) = self.executor() else {
            return Err(EngineError::Closed);
        };
        executor.post_close(TaskPayload::DrainRequests).map(|_| ())
    }

    /// Test-support fault gate for a genuine close timeout: keep the close
    /// intent/tombstone but do not dispatch the native close. Clearing it only
    /// schedules another UI close task; it never calls CEF from the caller's
    /// thread and cannot bypass a sealed executor.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn set_close_deferred(
        self: &Arc<Self>,
        browser: BrowserId,
        deferred: bool,
    ) -> Result<(), EngineError> {
        enum Action {
            None,
            Repost(CloseMode),
        }
        let action = {
            let mut state = self.lock();
            let Some(entry) = state.browsers.get_mut(&browser) else {
                return Err(EngineError::Closed);
            };
            if deferred {
                if entry.close_sent {
                    return Err(EngineError::Closing);
                }
                entry.close_deferred = true;
                Action::None
            } else if !entry.close_deferred {
                Action::None
            } else {
                entry.close_deferred = false;
                let terminal = matches!(
                    entry.session.snapshot().state,
                    SessionState::Closed | SessionState::CreateFailed
                );
                if terminal {
                    Action::None
                } else {
                    Action::Repost(entry.close_mode.unwrap_or(CloseMode::Graceful))
                }
            }
        };
        match action {
            Action::None => Ok(()),
            Action::Repost(mode) => {
                let Some(executor) = self.executor() else {
                    return Err(EngineError::Closed);
                };
                executor
                    .post_close(TaskPayload::Close {
                        browser,
                        mode,
                        ticket: None,
                    })
                    .map(|_| ())?;
                if let Some(entry) = self.lock().browsers.get_mut(&browser) {
                    entry.close_posted = Some(mode);
                }
                Ok(())
            }
        }
    }

    pub(crate) fn reject_create(&self, browser: BrowserId, error: EngineError) {
        let mut orphan = None;
        let mut capture = None;
        let mut ime_cleared = false;
        let mut ax_tree = None;
        let emit = {
            let mut state = self.lock();
            match state.browsers.get_mut(&browser) {
                Some(entry)
                    if !matches!(
                        entry.session.snapshot().state,
                        SessionState::CreateFailed | SessionState::Closed
                    ) =>
                {
                    entry.session.create_rejected();
                    entry.store.close();
                    entry.close_sent = true;
                    entry.force_sent = true;
                    // A refused instance cannot keep a composition anchor that a
                    // late render callback could still be about to fill in.
                    ime_cleared = entry.ime.invalidate();
                    // A browser that somehow appeared despite the rejection must
                    // not outlive the instance.
                    orphan = entry.native.take();
                    capture = entry.capture.clone();
                    ax_tree = Some(entry.ax_tree.clone());
                    true
                }
                _ => false,
            }
        };
        if let Some(ax_tree) = ax_tree {
            ax_tree.clear();
        }
        if ime_cleared {
            let _ = self.ui.wake();
        }
        if let Some(native) = orphan {
            let _ = self.close_native_ref(&native, true);
        }
        if let Some(capture) = capture
            && let Err(retire_error) = capture.retire()
        {
            capture.record_failure(&retire_error);
        }
        if emit {
            // A rejected instance can no longer own a native continuation.
            self.cancel_requests(self.take_browser_requests(browser.local()));
            self.emit(browser, BrowserEvent::CreateFailed(error));
        }
    }

    /// Called from OnAfterCreated, including a creation that raced shutdown.
    pub(crate) fn after_created(&self, browser: BrowserId, native: Browser) {
        let native = Arc::new(native);
        let native_for_hwnd = native.clone();
        enum Outcome {
            ScheduleClose,
            /// No instance can adopt this native browser (unknown or already
            /// retired): close it and never report an interactable instance.
            Discard(NativeBrowser, Option<Arc<BrowserClientState>>),
            Keep,
        }
        let (outcome, native_child_meta) = 'outcome: {
            let mut state = self.lock();
            let Some(entry) = state.browsers.get_mut(&browser) else {
                // Unknown instance. Never call CEF while holding the registry
                // lock: CloseBrowser can synchronously deliver OnBeforeClose,
                // which takes this same (non-reentrant) lock.
                break 'outcome (Outcome::Discard(native, None), None);
            };
            let native_child_meta = entry
                .native_hwnd
                .as_ref()
                .map(|child| (child.strip_noactivate, child.last_bounds));
            #[cfg(feature = "test-support")]
            if let Some(observed) = entry.late_create_probe.as_mut() {
                *observed = if *observed == 0
                    && entry.native.is_none()
                    && entry.session.snapshot().creation_accepted
                    && entry.close_mode == Some(CloseMode::Force)
                {
                    1
                } else {
                    2
                };
            }
            if entry.close_mode.is_some() {
                entry.session.close_requested();
            }
            entry.session.after_created();
            let terminal = matches!(
                entry.session.snapshot().state,
                SessionState::CreateFailed | SessionState::Closed
            );
            let deferred = entry.close_deferred();
            let closing = entry.session.snapshot().state == SessionState::Closing
                || entry.close_mode.is_some();
            if entry.native.is_none() {
                entry.native = Some(native);
            }
            let outcome = if terminal {
                match entry.native.take() {
                    Some(native) => Outcome::Discard(native, entry.client_state.take()),
                    None => {
                        // No native browser arrived; the handler state is still
                        // ours to release outside the lock.
                        entry.client_state = None;
                        Outcome::Keep
                    }
                }
            } else if closing && deferred {
                // Injected timeout: the Browser stays genuinely alive with a
                // recorded close intent, so the barrier remains blocked.
                Outcome::Keep
            } else if closing && (entry.session.input_reset_pending() || !entry.close_sent) {
                // Alloy still uses its platform delegate and loads the initial
                // URL after this callback, so native close must wait for UI.
                Outcome::ScheduleClose
            } else {
                Outcome::Keep
            };
            (outcome, native_child_meta)
        };
        if let Some((strip_noactivate, initial_bounds)) = native_child_meta {
            let raw_child_hwnd = native_for_hwnd
                .host()
                .and_then(|host| crate::native_hwnd::hwnd_from_cef(host.window_handle()));
            drop(native_for_hwnd);
            let hide_now = initial_bounds.is_empty() || !matches!(outcome, Outcome::Keep);
            let stripped = if let Some(hwnd) = raw_child_hwnd {
                let stripped = if strip_noactivate {
                    crate::native_hwnd::strip_ex_noactivate(hwnd)
                } else {
                    true
                };
                if hide_now {
                    let _ = crate::native_hwnd::hide_native_hwnd(hwnd);
                }
                stripped
            } else {
                false
            };
            if let Some(entry) = self.lock().browsers.get_mut(&browser)
                && let Some(child) = entry.native_hwnd.as_mut()
            {
                child.browser_hwnd = raw_child_hwnd;
                child.noactivate_stripped |= stripped;
                if hide_now {
                    child.visible = false;
                }
            }
        } else {
            drop(native_for_hwnd);
        }
        match outcome {
            Outcome::ScheduleClose => {
                self.mark_created_reported(browser);
                self.emit(browser, BrowserEvent::Created);
                // Keep the native owner and Closing state if the post fails.
                // No extra retry is scheduled here.
                if let Some(executor) = self.executor() {
                    let _ = executor.post_close(TaskPayload::CloseAfterCreated { browser });
                }
                return;
            }
            Outcome::Discard(native, client) => {
                let _ = self.close_native_ref(&native, true);
                drop(client);
            }
            Outcome::Keep => {
                self.mark_created_reported(browser);
                self.emit(browser, BrowserEvent::Created);
                if self
                    .menu_bridge
                    .get(browser)
                    .is_some_and(|b| b.spelling.is_configured())
                    && let Some(executor) = self.executor()
                {
                    let _ = executor.post(TaskPayload::SpellingPreferences { browser });
                }
                let a11y_init = {
                    let state = self.lock();
                    state
                        .browsers
                        .get(&browser)
                        .filter(|e| {
                            !e.cleanup_started
                                && e.session.configuration().options.accessibility_state
                                    == CefFeatureState::Enabled
                        })
                        .and_then(|e| e.native.clone())
                };
                if let Some(native) = a11y_init
                    && native.is_valid() != 0
                    && let Some(host) = native.host()
                {
                    self.audit.record();
                    host.set_accessibility_state(State::ENABLED);
                }
                let auto_cdp_host = {
                    let state = self.lock();
                    state
                        .browsers
                        .get(&browser)
                        .filter(|e| e.headless_cdp_enabled && !e.cleanup_started)
                        .and_then(|e| e.native.clone())
                };
                if let Some(native) = auto_cdp_host
                    && let Some(observer) = self.host_capabilities().cdp_observer
                    && native.is_valid() != 0
                    && let Some(host) = native.host()
                {
                    let _ = self.headless_cdp.attach_observer(
                        true,
                        browser,
                        &host,
                        observer,
                        &self.audit,
                    );
                }
            }
        }
        self.finish_input_reset(browser);
    }

    fn mark_created_reported(&self, browser: BrowserId) {
        if let Some(entry) = self.lock().browsers.get_mut(&browser) {
            entry.created_reported = true;
        }
    }

    /// Called from OnBeforeClose. CEF documents this as the point where the
    /// application must release its CefBrowser reference; the callback runs on
    /// the CEF UI sequence in both modes, so the reference is dropped here,
    /// after the lock is released and outside any host borrow.
    pub(crate) fn before_close(&self, browser: BrowserId) {
        let (report, native, client, store, frame_wake, ime_cleared, ax_tree) = {
            let mut state = self.lock();
            let Some(entry) = state.browsers.get_mut(&browser) else {
                return;
            };
            if matches!(
                entry.session.snapshot().state,
                SessionState::Closed | SessionState::CreateFailed
            ) {
                // CEF delivers OnBeforeClose once, but a repeated notification
                // must not double-release references or duplicate the terminal
                // event in the host.
                return;
            }
            if entry.cleanup_started {
                // Our own release/cancel pass is still running: it owns the one
                // terminal transition.
                return;
            }
            entry.cleanup_started = true;
            if !entry.input_outstanding.is_empty()
                || entry.session.input_reset_pending()
                || entry.input_shadow.has_native_state()
            {
                entry.input_shadow.cleanup_failed = true;
                entry.input_shadow.last_native_error = Some(EngineError::Closed);
                if let Some(input) = entry.input.as_mut() {
                    input.cleanup_failed = true;
                    input.last_native_error = Some(EngineError::Closed);
                }
            }
            // Deliberately not `session.closed()` yet. The instance moves to
            // Closing (never terminal) until every local reference and
            // continuation is gone, so the main thread cannot observe Ready
            // during cleanup.
            entry.session.close_requested();
            entry.close_sent = true;
            entry.force_sent = true;
            // Cleanup owns the native reference from here on, so a callback that
            // arrives during teardown cannot repopulate geometry.
            let ime_cleared = entry.ime.invalidate();
            let frame_wake = entry.store.close();
            if let Some(child) = entry.native_hwnd.as_mut() {
                child.browser_hwnd = None;
                child.visible = false;
            }
            #[cfg(feature = "test-support")]
            {
                entry.frame_probe_final = entry
                    .client_state
                    .as_ref()
                    .map(|client| client.frame_probe_state());
                entry.menu_probe_final = entry.client_state.as_ref().map(|client| {
                    client.menu_probe.retire();
                    client.menu_probe.clone()
                });
            }
            (
                entry.created_reported,
                entry.native.take(),
                entry.client_state.take(),
                entry.store.clone(),
                frame_wake,
                ime_cleared,
                entry.ax_tree.clone(),
            )
        };
        ax_tree.clear();
        let _ = self.outbound_drags.take_active_for_browser(browser);
        // Do not guess that OnBeforeClose still owns a live menu manager.
        // Normal close/drain cancels earlier; unexpected survivors fault/leak.
        self.menus.native_closed(browser);
        let _ = self.sources.cancel(Some(browser), None, None);
        self.finds.browser_closed(browser);
        // Original Browser terminal is not proof of print job/UI retirement.
        self.prints.browser_closed(browser);
        // Parent OnBeforeClose does not prove its DevTools has been destroyed.
        self.request_auxiliary_close(Some(browser), None);
        self.menu_bridge.remove(browser);
        wake_frames(&store, &self.ui, frame_wake);
        if ime_cleared {
            // Outside the lock: the host may read the cleared slot from here.
            let _ = self.ui.wake();
        }
        // Native references and captured continuations are released outside
        // every registry lock: a cancel callback may re-enter the host.
        #[cfg(feature = "test-support")]
        self.retire_probe_dialog(browser);
        #[cfg(feature = "test-support")]
        self.retire_probe_renderer(browser);
        if let Some(client) = client.as_ref() {
            client.recovery.cancel(true);
        }
        if let Some(client) = client.as_ref()
            && let Err(error) = client.capture.retire()
        {
            // Keep cleanup_started and the client: a failed UI retirement may
            // never be promoted to Closed/Ready with native resources alive.
            if let Some(entry) = self.lock().browsers.get_mut(&browser) {
                entry.client_state = Some(client.clone());
            }
            if matches!(
                error,
                EngineError::Capture {
                    kind: rust_cef_core::ports::CaptureFailureKind::Busy,
                    ..
                }
            ) {
                if let Some(entry) = self.lock().browsers.get_mut(&browser) {
                    entry.capture_cleanup_deferred = true;
                }
                let executor = self.executor();
                client.capture.when_idle(move || {
                    if let Some(executor) = executor {
                        // A failed post leaves the native gate closed and the
                        // diagnostic deadline blocked; never pretend Closed.
                        let _ = executor.post_close(TaskPayload::RetireCapture { browser });
                    }
                });
            } else {
                self.capture_failed(browser, error);
            }
            drop(native);
            return;
        }
        drop(native);
        drop(client);
        self.cancel_requests(self.take_browser_requests(browser.local()));
        let dialogs = self.dialogs.take_browser(browser);
        let deferred = self
            .lock()
            .browsers
            .get_mut(&browser)
            .and_then(|entry| entry.deferred_force_dialog.take());
        self.dialog_invalidated.fetch_add(
            dialogs.len() as u64 + u64::from(deferred.is_some()),
            Ordering::AcqRel,
        );
        drop(dialogs);
        drop(deferred);
        let caps = self.host_capabilities();
        self.host_requests
            .cancel_browser_all(browser, true, &self.audit, &caps);
        self.headless_cdp.detach_browser(true, browser);
        self.cancel_browser_evals(browser, EvalError::Closed);
        // Only now is the instance really out of native state; publish the
        // terminal transition.
        {
            let mut state = self.lock();
            let Some(entry) = state.browsers.get_mut(&browser) else {
                return;
            };
            if !entry.cleanup_started {
                return;
            }
            entry.cleanup_started = false;
            if matches!(
                entry.session.snapshot().state,
                SessionState::Closed | SessionState::CreateFailed
            ) {
                // Something else already made this instance terminal; only
                // clear our cleanup marker, without a second terminal event.
                return;
            }
            entry.session.closed();
            entry.close_prompt_cut = None;
            if let Some(completion) = entry.close_completion.take() {
                completion.complete(CloseResult::Closed);
            }
        }
        self.closed.fetch_add(1, Ordering::AcqRel);
        if report {
            self.emit(browser, BrowserEvent::Closed);
        }
    }

    fn attach_close_completion(&self, browser: BrowserId, completion: CloseCompletion) {
        let mut state = self.lock();
        match state.browsers.get_mut(&browser) {
            Some(entry)
                if !matches!(
                    entry.session.snapshot().state,
                    SessionState::Closed | SessionState::CreateFailed
                ) =>
            {
                entry.close_completion = Some(completion);
            }
            _ => completion.complete(CloseResult::Closed),
        }
    }

    /// Records intent before the Close task is posted so a late OnAfterCreated
    /// cannot escape the shutdown barrier. A later Force request upgrades the
    /// recorded intent; `close_now` then issues one additional forceful close.
    pub(crate) fn note_close_intent(&self, browser: BrowserId, mode: CloseMode) {
        self.note_close_intent_for_ticket(browser, mode, None);
    }

    fn note_close_intent_for_ticket(
        &self,
        browser: BrowserId,
        mode: CloseMode,
        ticket: Option<&CloseTicket>,
    ) -> bool {
        let mut state = self.lock();
        if !pending_close_ticket(browser, ticket) {
            return false;
        }
        let mut ime_cleared = false;
        let mut auxiliary_closes = Vec::new();
        if let Some(entry) = state.browsers.get_mut(&browser) {
            // Linearize aux intent with the parent's lifecycle, before posting
            // Close. An issued aux may be created while that post fails/waits.
            auxiliary_closes = self.devtools.request_close(Some(browser), None);
            // Even a cancelled Graceful close invalidates the current print
            // intent, but only a real terminal Browser retires enrollment.
            self.prints.invalidate(browser, None);
            // A close intent retires the current anchor immediately; a graceful
            // close may still be cancelled, but the geometry is already stale.
            ime_cleared = entry.ime.invalidate();
            // A still-Creating instance is left Creating: its queued Create task
            // will reject itself once drainage is visible. If a native browser
            // was already issued, OnAfterCreated observes close_mode and closes
            // it immediately. Only an Alive instance moves to Closing here.
            let was_alive = entry.session.snapshot().state == SessionState::Alive;
            if was_alive {
                entry.session.close_requested();
            }
            if was_alive
                && (entry.input.as_ref().is_some_and(|input| input.admitted > 0)
                    || !entry.input_outstanding.is_empty())
            {
                if let Some(executor) = self.executor() {
                    let _ = Self::queue_reset(browser, entry, &executor, InputResetReason::Closing);
                }
            }
            match entry.close_mode {
                Some(CloseMode::Force) => {}
                Some(CloseMode::Graceful) | None if mode == CloseMode::Force => {
                    entry.close_mode = Some(CloseMode::Force);
                }
                Some(CloseMode::Graceful) => {}
                None => entry.close_mode = Some(mode),
            }
        }
        drop(state);
        self.post_auxiliary_closes(auxiliary_closes);
        self.cancel_browser_evals(browser, EvalError::Closed);
        if ime_cleared {
            let _ = self.ui.wake();
        }
        true
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn probe_late_create_observed(&self, browser: BrowserId) -> Option<bool> {
        self.lock()
            .browsers
            .get(&browser)?
            .late_create_probe
            .map(|count| count == 1)
    }

    pub(crate) fn snapshot(&self, browser: BrowserId) -> BrowserSnapshot {
        let state = self.lock();
        state.browsers.get(&browser).map_or(
            BrowserSnapshot {
                browser,
                creation_accepted: false,
                state: SessionState::CreateFailed,
                navigation: Default::default(),
                renderer: Default::default(),
            },
            |entry| entry.session.snapshot(),
        )
    }

    /// Read-only endpoint retains the very same mailbox as every CEF callback.
    /// It remains a closed snapshot after OnBeforeClose, never a replacement slot.
    pub(crate) fn frame_source(
        &self,
        browser: BrowserId,
    ) -> Result<Arc<dyn FrameSource>, EngineError> {
        self.mailbox(browser)
            .map(|store| store as Arc<dyn FrameSource>)
    }

    /// Test an already-owned VIEW frame against the authoritative closed and
    /// live mailbox publication gates. Does not emulate a CEF callback or
    /// call the observer; no native handle or caller-provided mailbox is used.
    #[cfg(feature = "test-support")]
    pub(crate) fn probe_replay_retired_view(
        &self,
        live: BrowserId,
        frame: &rust_cef_core::ports::OwnedFrame,
    ) -> Result<[Option<String>; 2], EngineError> {
        let original = frame.info.browser;
        if original.runtime() != self.runtime || live.runtime() != self.runtime {
            return Err(EngineError::WrongRuntime);
        }
        if original == live || frame.info.surface != SurfaceKind::View {
            return Err(EngineError::InvalidRequest);
        }
        let (closed_store, live_store) = {
            let state = self.lock();
            if state.draining {
                return Err(EngineError::Closing);
            }
            let closed = state.browsers.get(&original).ok_or(EngineError::Closed)?;
            let successor = state.browsers.get(&live).ok_or(EngineError::Closed)?;
            if closed.session.snapshot().state != SessionState::Closed
                || !successor.session.snapshot().is_ready()
                || successor.cleanup_started
            {
                return Err(EngineError::NotReady);
            }
            (closed.store.clone(), successor.store.clone())
        };
        let rejection =
            |store: &FrameMailbox| store.publish(frame.clone()).err().map(|e| format!("{e:?}"));
        Ok([rejection(&closed_store), rejection(&live_store)])
    }

    /// One-shot view binding, checked entirely under the existing registry
    /// state lock. The complete `BrowserId` (Runtime generation + local) must
    /// designate a live entry that is not draining, not Closing/Closed/
    /// CreateFailed, and whose authoritative frame source is still open.
    /// `Creating` is accepted because creation was already requested by the
    /// Runtime; this never creates or initializes a Browser itself.
    pub(crate) fn claim_view_attachment(
        &self,
        browser: BrowserId,
    ) -> Result<(Arc<dyn FrameSource>, BrowserSurfaceMode), AttachmentError> {
        let mut state = self.lock();
        // Both identity components are validated under the same registry lock as
        // the one-shot flag. `browsers` is keyed by the full BrowserId, so a
        // foreign generation can never match a local entry.
        if browser.runtime() != self.runtime {
            return Err(AttachmentError::Engine(EngineError::WrongRuntime));
        }
        if state.draining {
            return Err(AttachmentError::Engine(EngineError::Closing));
        }
        let entry = state
            .browsers
            .get_mut(&browser)
            .ok_or(AttachmentError::Engine(EngineError::Closed))?;
        match entry.session.snapshot().state {
            SessionState::Creating | SessionState::Alive => {}
            SessionState::Closing => return Err(AttachmentError::Engine(EngineError::Closing)),
            SessionState::Closed | SessionState::CreateFailed => {
                return Err(AttachmentError::Engine(EngineError::Closed));
            }
        }
        if entry.store.is_closed() {
            return Err(AttachmentError::Engine(EngineError::Closed));
        }
        if entry.view_attachment_claimed {
            return Err(AttachmentError::AlreadyAttached);
        }
        entry.view_attachment_claimed = true;
        let source: Arc<dyn FrameSource> = entry.store.clone();
        Ok((source, entry.session.surface_mode()))
    }

    pub(crate) fn native_browser_hwnd(&self, browser: BrowserId) -> Option<NonZeroUsize> {
        if browser.runtime() != self.runtime {
            return None;
        }
        self.lock()
            .browsers
            .get(&browser)
            .and_then(|entry| entry.native_hwnd.as_ref()?.browser_hwnd)
    }

    pub(crate) fn schedule_native_child_destroy(
        &self,
        browser: BrowserId,
        live_hwnd: Option<NonZeroUsize>,
    ) -> bool {
        let stored_hwnd = self
            .lock()
            .browsers
            .get(&browser)
            .and_then(|entry| entry.native_hwnd.as_ref()?.browser_hwnd);
        let Some(hwnd) = live_hwnd.or(stored_hwnd) else {
            return false;
        };
        if !crate::native_hwnd::is_valid_window(hwnd) {
            return false;
        }
        let Some(executor) = self.executor() else {
            return false;
        };
        executor
            .post_close(TaskPayload::DestroyNativeChildHwnd { browser, hwnd })
            .is_ok()
    }

    pub(crate) fn surface_mode(&self, browser: BrowserId) -> Option<BrowserSurfaceMode> {
        if browser.runtime() != self.runtime {
            return None;
        }
        self.lock()
            .browsers
            .get(&browser)
            .map(|entry| entry.session.surface_mode())
    }

    fn update_native_bounds_state(
        &self,
        browser: BrowserId,
        rect: PixelRect,
    ) -> Result<(Option<NonZeroUsize>, bool), EngineError> {
        let mut state = self.lock();
        let entry = state
            .browsers
            .get_mut(&browser)
            .ok_or(EngineError::Closed)?;
        let child = entry
            .native_hwnd
            .as_mut()
            .ok_or(EngineError::InvalidRequest)?;
        child.last_bounds = rect;
        Ok((child.browser_hwnd, child.visible))
    }

    fn native_set_visible_state(
        &self,
        browser: BrowserId,
        visible: bool,
    ) -> Option<(Option<NonZeroUsize>, PixelRect)> {
        let mut state = self.lock();
        let entry = state.browsers.get_mut(&browser)?;
        let child = entry.native_hwnd.as_mut()?;
        child.visible = visible;
        Some((child.browser_hwnd, child.last_bounds))
    }

    pub(crate) fn mailbox(&self, browser: BrowserId) -> Result<Arc<FrameMailbox>, EngineError> {
        if browser.runtime() != self.runtime {
            return Err(EngineError::WrongRuntime);
        }
        self.lock()
            .browsers
            .get(&browser)
            .map(|entry| entry.store.clone())
            .ok_or(EngineError::Closed)
    }

    pub(crate) fn frame_budget_stats(
        &self,
        browser: BrowserId,
    ) -> Result<rust_cef_core::frame_budget::BudgetStats, EngineError> {
        if browser.runtime() != self.runtime {
            return Err(EngineError::WrongRuntime);
        }
        let budget = {
            let state = self.lock();
            state
                .browsers
                .get(&browser)
                .ok_or(EngineError::Closed)?
                .store
                .budget()
        };
        // Reaping an external CPU owner can run its destructor; never do this
        // while a registry/native ownership lock is held.
        Ok(budget.stats())
    }

    pub(crate) fn snapshots(&self) -> Vec<BrowserSnapshot> {
        self.lock()
            .browsers
            .values()
            .map(|entry| entry.session.snapshot())
            .collect()
    }

    /// Every instance released its durable CefBrowser reference and UI-owned
    /// capture resources. Frame/pixel ownership alone is pure data, but a client
    /// can now contain D3D COM refs: those must retire before the barrier.
    pub(crate) fn native_refs_clear(&self) -> bool {
        let clear = self.lock().browsers.values().all(|entry| {
            entry.native.is_none()
                && !entry.cleanup_started
                && entry.deferred_force_dialog.is_none()
                && entry
                    .capture
                    .as_ref()
                    .is_none_or(|capture| capture.native_refs_clear())
        });
        let clear = clear
            && self.prints.pending() == 0
            && self.auxiliary_native.is_empty()
            && self.auxiliary_windows.is_empty()
            && !self.devtools.faulted()
            && self.lock_evals().pending_count() == 0
            && self.host_requests.is_clear()
            && self.pdf_exports.pending_count() == 0
            && self.headless_cdp.active_registrations() == 0
            && self.outbound_drags.is_clear()
            && self.url_requests().is_none_or(|u| u.is_clear());
        #[cfg(feature = "test-support")]
        {
            clear
                && self
                    .probe_dialogs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty()
                && self
                    .probe_renderers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty()
        }
        #[cfg(not(feature = "test-support"))]
        {
            clear
        }
    }

    pub(crate) fn open_browsers(&self) -> Vec<BrowserId> {
        let state = self.lock();
        state
            .browsers
            .values()
            .filter(|entry| {
                !matches!(
                    entry.session.snapshot().state,
                    SessionState::Closed | SessionState::CreateFailed
                )
            })
            .map(|entry| entry.session.snapshot().browser)
            .collect()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn all_terminal(&self) -> bool {
        let state = self.lock();
        state.browsers.values().all(|entry| {
            matches!(
                entry.session.snapshot().state,
                SessionState::Closed | SessionState::CreateFailed
            )
        })
    }

    /// Callback-scoped strings and flags are already owned. Update the latest
    /// snapshot under the identity gate, then publish outside every state lock.
    pub(crate) fn navigation_event(&self, browser: BrowserId, event: BrowserEvent) {
        self.navigation_observation(browser, event, None);
    }

    pub(crate) fn navigation_observation(
        &self,
        browser: BrowserId,
        event: BrowserEvent,
        source_url: Option<String>,
    ) {
        let cancel_evals = matches!(
            &event,
            BrowserEvent::NavigationRequested {
                is_main_frame: true,
                ..
            } | BrowserEvent::LoadStarted {
                is_main_frame: true
            }
        );
        if cancel_evals {
            // A new main-frame document displaces any pending print identity.
            let mut state = self.lock();
            if let Some(entry) = state.browsers.get_mut(&browser) {
                entry.document_epoch = entry.document_epoch.saturating_add(1);
            }
        }
        let displaces_document = matches!(
            event,
            BrowserEvent::NavigationRequested { .. } | BrowserEvent::LoadStarted { .. }
        );
        if displaces_document {
            self.invalidate_native_menu(browser);
        }
        let accepted = {
            let mut state = self.lock();
            let accepted = state.browsers.get_mut(&browser).is_some_and(|entry| {
                let applied = entry
                    .session
                    .apply_navigation_observation(&event, source_url.as_deref());
                if applied && is_ux_event(&event) {
                    if entry.ux_events.len() >= MAX_UX_EVENTS {
                        entry.ux_events.pop_front();
                    }
                    entry.ux_events.push_back(event.clone());
                }
                applied
            });
            if accepted && displaces_document {
                // A confirmation outlives the originating menu. Invalidate it
                // even if no menu is attached; any frame may be its origin.
                self.prints.invalidate(browser, None);
            }
            accepted
        };
        if accepted {
            self.emit(browser, event);
        }
        if cancel_evals {
            self.cancel_browser_evals(browser, EvalError::Navigated);
            let caps = self.host_capabilities();
            self.host_requests
                .cancel_navigation_prompts_for_browser(browser, &self.audit, &caps);
        }
    }

    pub(crate) fn take_ux_events(&self, browser: BrowserId) -> Vec<BrowserEvent> {
        if browser.runtime() != self.runtime {
            return Vec::new();
        }
        let mut state = self.lock();
        let Some(entry) = state.browsers.get_mut(&browser) else {
            return Vec::new();
        };
        entry.ux_events.drain(..).collect()
    }

    pub(crate) fn take_js_events(&self, browser: BrowserId) -> Vec<BrowserEvent> {
        if browser.runtime() != self.runtime {
            return Vec::new();
        }
        let mut state = self.lock();
        let Some(entry) = state.browsers.get_mut(&browser) else {
            return Vec::new();
        };
        let mut out: Vec<BrowserEvent> = entry.eval_events.drain(..).collect();
        out.extend(entry.js_messages.drain(..));
        out
    }

    pub(crate) fn submit_eval(
        &self,
        browser: BrowserId,
        executor: &Executor,
        script: String,
        frame_scope: JsFrameScope,
        timeout_ms: u64,
    ) -> Result<EvalRequestId, EngineError> {
        if browser.runtime() != self.runtime {
            return Err(EngineError::WrongRuntime);
        }
        validate_script(&script).map_err(|_| EngineError::InvalidRequest)?;
        let timeout_ms = normalize_eval_timeout_ms(timeout_ms);
        let mut state = self.lock();
        if state.draining {
            return Err(EngineError::Closing);
        }
        let entry = state
            .browsers
            .get_mut(&browser)
            .ok_or(EngineError::Closed)?;
        if entry.cleanup_started {
            return Err(EngineError::Closing);
        }
        match entry.session.snapshot().state {
            SessionState::Alive => {}
            SessionState::Creating => return Err(EngineError::NotReady),
            SessionState::Closing => return Err(EngineError::Closing),
            SessionState::Closed | SessionState::CreateFailed => return Err(EngineError::Closed),
        }
        let id = self
            .lock_evals()
            .register(
                browser,
                entry.document_epoch,
                entry.js_bridge.max_pending_evals,
            )
            .map_err(|err| match err {
                EvalError::QueueFull => EngineError::Backpressure,
                _ => EngineError::InvalidRequest,
            })?;
        let command = BrowserCommand::EvaluateScript {
            id,
            script,
            frame_scope,
            timeout_ms,
        };
        let envelope = match entry.session.admit_command(command) {
            Ok(env) => env,
            Err(err) => {
                self.lock_evals().rollback(id);
                return Err(err);
            }
        };
        let cmd_id = envelope.id;
        if let Err(err) = executor.post(TaskPayload::Command(envelope)) {
            entry.session.cancel_command(cmd_id);
            self.lock_evals().rollback(id);
            return Err(err);
        }
        Ok(id)
    }

    pub(crate) fn handle_process_message(
        &self,
        browser: BrowserId,
        client_state: &BrowserClientState,
        frame: Option<&mut Frame>,
        message: &ProcessMessage,
    ) -> i32 {
        let Some(action) = crate::js_bridge_native::parse_renderer_process_message(
            browser,
            client_state,
            frame,
            message,
        ) else {
            return 0;
        };
        match action {
            crate::js_bridge_native::RendererIpcAction::PostMessage {
                origin,
                payload_json,
            } => {
                let event = BrowserEvent::JsMessageReceived {
                    origin,
                    payload_json,
                };
                let accepted = {
                    let mut state = self.lock();
                    if state.draining {
                        false
                    } else if let Some(entry) = state.browsers.get_mut(&browser)
                        && !entry.cleanup_started
                        && entry.session.snapshot().state == SessionState::Alive
                    {
                        if entry.js_messages.len() >= MAX_JS_MESSAGE_QUEUE_PER_BROWSER {
                            self.js_messages_dropped.fetch_add(1, Ordering::Relaxed);
                            false
                        } else {
                            entry.js_messages.push_back(event.clone());
                            true
                        }
                    } else {
                        false
                    }
                };
                if accepted {
                    self.emit(browser, event);
                }
                1
            }
            crate::js_bridge_native::RendererIpcAction::EvalResponse { id, outcome } => {
                let expected_epoch = {
                    let state = self.lock();
                    if state.draining {
                        None
                    } else {
                        state
                            .browsers
                            .get(&browser)
                            .filter(|e| {
                                !e.cleanup_started
                                    && matches!(
                                        e.session.snapshot().state,
                                        SessionState::Alive | SessionState::Closing
                                    )
                            })
                            .map(|e| e.document_epoch)
                    }
                };
                if let Some(epoch) = expected_epoch {
                    self.complete_eval(id, Some(epoch), outcome);
                } else {
                    self.complete_eval(id, Some(0), outcome);
                }
                1
            }
        }
    }

    pub(crate) fn renderer_event(&self, browser: BrowserId, event: BrowserEvent) {
        let terminated = matches!(event, BrowserEvent::RendererTerminated { .. });
        if terminated {
            self.invalidate_native_menu(browser);
            self.request_auxiliary_close(Some(browser), None);
        }
        let accepted = {
            let mut state = self.lock();
            state.browsers.get_mut(&browser).and_then(|entry| {
                if entry.cleanup_started || !entry.session.apply_renderer_event(&event) {
                    return None;
                }
                let wake = if matches!(event, BrowserEvent::RendererTerminated { .. }) {
                    self.prints.invalidate(browser, None);
                    // Keep the last VIEW visibly stale, retire POPUP and revoke
                    // every capture token issued before the actual fault.
                    entry.store.invalidate(SurfaceKind::View).ok().flatten()
                } else {
                    None
                };
                Some((entry.store.clone(), wake))
            })
        };
        if let Some((store, wake)) = accepted {
            wake_frames(&store, &self.ui, wake);
            // Lifecycle/navigation event wakes remain independent of frame data.
            self.emit(browser, event);
            if terminated {
                self.cancel_browser_evals(browser, EvalError::RendererTerminated);
                let caps = self.host_capabilities();
                self.host_requests
                    .cancel_non_download_for_browser(browser, &self.audit, &caps);
            }
        }
    }

    /// IO-side denial reporting is bounded to one queued marker per browser.
    /// No host event sink or GPUI is called from a synchronous IO decision.
    pub(crate) fn note_policy_denial(&self, browser: BrowserId, action: PolicyAction) {
        let post = {
            let mut state = self.lock();
            let Some(entry) = state.browsers.get_mut(&browser) else {
                return;
            };
            if entry.cleanup_started
                || matches!(
                    entry.session.snapshot().state,
                    SessionState::Closed | SessionState::CreateFailed
                )
            {
                return;
            }
            entry.policy_pending.insert(action);
            if entry.policy_posted {
                false
            } else {
                entry.policy_posted = true;
                true
            }
        };
        if post
            && self.executor().is_none_or(|executor| {
                executor
                    .post_while_draining(TaskPayload::PolicyEvents { browser })
                    .is_err()
            })
        {
            if let Some(entry) = self.lock().browsers.get_mut(&browser) {
                entry.policy_posted = false;
            }
        }
    }

    fn flush_policy_events(&self, browser: BrowserId) {
        let actions = {
            let mut state = self.lock();
            let Some(entry) = state.browsers.get_mut(&browser) else {
                return;
            };
            entry.policy_posted = false;
            if entry.cleanup_started
                || matches!(
                    entry.session.snapshot().state,
                    SessionState::Closed | SessionState::CreateFailed
                )
            {
                entry.policy_pending.clear();
                return;
            }
            std::mem::take(&mut entry.policy_pending)
        };
        for action in actions {
            self.emit(browser, BrowserEvent::PolicyDenied { action });
        }
    }

    /// A paint callback arrived but its bytes could not be retained. Report it
    /// instead of silently keeping the previous frame.
    pub(crate) fn capture_failed(&self, browser: BrowserId, error: EngineError) {
        let live = self.lock().browsers.get(&browser).is_some_and(|entry| {
            !entry.cleanup_started
                && !entry.store.is_closed()
                && !matches!(
                    entry.session.snapshot().state,
                    SessionState::Closed | SessionState::CreateFailed
                )
        });
        if live {
            self.emit(browser, BrowserEvent::CaptureFailed(error));
        }
    }

    pub(crate) fn note_software_frame(&self) {
        self.frame_count.fetch_add(1, Ordering::AcqRel);
        self.observed.store(1, Ordering::Release);
    }

    pub(crate) fn note_d3d11_frame(&self) {
        self.frame_count.fetch_add(1, Ordering::AcqRel);
        self.observed.store(2, Ordering::Release);
    }

    pub(crate) fn recovery_diagnostics(
        &self,
        browser: BrowserId,
    ) -> Result<crate::resize::FrameRecoveryDiagnostics, EngineError> {
        if browser.runtime() != self.runtime {
            return Err(EngineError::WrongRuntime);
        }
        let recovery = self
            .lock()
            .browsers
            .get(&browser)
            .ok_or(EngineError::Closed)?
            .recovery
            .clone();
        Ok(recovery
            .map(|state| state.diagnostics())
            .unwrap_or_default())
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn frame_probe_state(
        &self,
        browser: BrowserId,
    ) -> Option<crate::test_support::FrameProbeState> {
        let (client, retired) = {
            let state = self.lock();
            let entry = state.browsers.get(&browser)?;
            (entry.client_state.clone(), entry.frame_probe_final.clone())
        };
        client.map(|client| client.frame_probe_state()).or(retired)
    }
    pub(crate) fn schedule_resize(&self, browser: BrowserId, delay_ms: i64) {
        let recovery = {
            let state = self.lock();
            if state.draining {
                return;
            }
            state
                .browsers
                .get(&browser)
                .filter(|entry| {
                    entry.session.snapshot().is_ready()
                        && !entry.cleanup_started
                        && !entry.store.is_closed()
                })
                .and_then(|entry| entry.recovery.clone())
        };
        let Some(recovery) = recovery else {
            return;
        };
        let Some(ticket) = recovery.schedule() else {
            return;
        };
        let posted = {
            // Serialize the final admission/post return with the same shutdown
            // cutpoint as ordinary commands; post never executes inline.
            let state = self.lock();
            if state.draining
                || !state.browsers.get(&browser).is_some_and(|entry| {
                    entry.session.snapshot().is_ready()
                        && !entry.cleanup_started
                        && !entry.store.is_closed()
                })
            {
                Err(EngineError::Closing)
            } else {
                self.executor()
                    .ok_or(EngineError::Closed)
                    .and_then(|executor| {
                        executor
                            .post_delayed(TaskPayload::ResizeRefresh { browser, ticket }, delay_ms)
                    })
            }
        };
        if let Err(error) = posted {
            if matches!(error, EngineError::Closing | EngineError::Closed) {
                recovery.post_cancelled(ticket);
            } else {
                recovery.post_failed(ticket);
                self.capture_failed(browser, error);
            }
        }
    }
    fn resize_tick(&self, browser: BrowserId, ticket: crate::resize::ResizeTicket) {
        let (recovery, native) = {
            let state = self.lock();
            let Some(entry) = state.browsers.get(&browser) else {
                return;
            };
            let Some(recovery) = entry.recovery.clone() else {
                return;
            };
            let native = (!state.draining
                && entry.session.snapshot().is_ready()
                && !entry.cleanup_started
                && !entry.store.is_closed())
            .then(|| entry.native.clone())
            .flatten();
            (recovery, native)
        };
        if native.is_none() {
            recovery.cancel(false);
        }
        let tick = recovery.begin_tick(ticket, std::time::Instant::now());
        if tick.request
            && let Some(native) = native
        {
            self.audit.record();
            if let Some(host) = native.host() {
                recovery.note_invalidate(
                    std::time::Instant::now(),
                    crate::executor::current_thread_id(),
                );
                self.audit.record();
                host.invalidate(PaintElementType::VIEW);
            }
        }
        if tick.exhausted {
            self.capture_failed(
                browser,
                EngineError::Capture {
                    kind: rust_cef_core::ports::CaptureFailureKind::ResizeExhausted,
                    code: None,
                },
            );
        }
        if tick.needs_another {
            self.schedule_resize(browser, 100);
        }
    }

    pub(crate) fn capture_diagnostics(
        &self,
        browser: BrowserId,
    ) -> Result<crate::capture::CaptureDiagnostics, EngineError> {
        if browser.runtime() != self.runtime {
            return Err(EngineError::WrongRuntime);
        }
        let capture = self
            .lock()
            .browsers
            .get(&browser)
            .ok_or(EngineError::Closed)?
            .capture
            .clone();
        Ok(capture
            .map(|capture| capture.diagnostics())
            .unwrap_or_default())
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn probe_arm_view_copy_timing(&self, browser: BrowserId) -> Result<(), EngineError> {
        if browser.runtime() != self.runtime {
            return Err(EngineError::WrongRuntime);
        }
        let capture = {
            let state = self.lock();
            if state.draining {
                return Err(EngineError::Closing);
            }
            state
                .browsers
                .get(&browser)
                .filter(|entry| !entry.cleanup_started && entry.session.snapshot().is_ready())
                .and_then(|entry| entry.capture.clone())
                .ok_or(EngineError::Closed)?
        };
        capture.arm_view_copy_timing()
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn probe_view_copy_samples(
        &self,
        browser: BrowserId,
    ) -> Option<Vec<crate::test_support::ViewCopySample>> {
        if browser.runtime() != self.runtime {
            return None;
        }
        let capture = self.lock().browsers.get(&browser)?.capture.clone()?;
        capture.view_copy_samples()
    }

    pub(crate) fn frame_count(&self) -> u64 {
        self.frame_count.load(Ordering::Acquire)
    }

    pub(crate) fn observed_backend(&self) -> Option<RenderPreference> {
        match self.observed.load(Ordering::Acquire) {
            1 => Some(RenderPreference::SoftwareRequired),
            2 => Some(RenderPreference::D3d11Required),
            _ => None,
        }
    }

    /// Test-only stimulus for the exact trusted fixture. No caller script or
    /// renderer callback is manufactured; the result must come back through CEF.
    #[cfg(feature = "test-support")]
    fn probe_pressure_title_now(&self, browser: BrowserId, step: u16) {
        use crate::test_support::{
            PRESSURE_EVENT_CAPACITY, PRESSURE_FIXTURE_URL, PRESSURE_SOURCE_FIXTURE_URL,
            PRESSURE_TITLE_PREFIX,
        };
        if step == 0 || usize::from(step) > PRESSURE_EVENT_CAPACITY + 1 {
            return;
        }
        let native = {
            let state = self.lock();
            state
                .browsers
                .get(&browser)
                .filter(|e| {
                    !state.draining && !e.cleanup_started && e.session.snapshot().is_ready()
                })
                .and_then(|e| e.native.clone())
        };
        let Some(native) = native else {
            return;
        };
        self.audit.record();
        let Some(frame) = native.main_frame() else {
            return;
        };
        self.audit.record();
        let url = CefString::from(&frame.url()).to_string();
        if !matches!(
            url.as_str(),
            PRESSURE_FIXTURE_URL | PRESSURE_SOURCE_FIXTURE_URL
        ) {
            return;
        }
        let script =
            CefString::from(format!("document.title='{PRESSURE_TITLE_PREFIX}{step}';").as_str());
        self.audit.record();
        frame.execute_java_script(Some(&script), None, 1);
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn post_command_pressure_close(
        &self,
        browser: BrowserId,
        executor: &Executor,
        expectation: crate::test_support::PressureExpectation,
    ) -> Result<(), EngineError> {
        if browser.runtime() != self.runtime {
            return Err(EngineError::WrongRuntime);
        }
        {
            let mut state = self.lock();
            if state.draining {
                return Err(EngineError::Closing);
            }
            if !state.browsers.contains_key(&browser) {
                return Err(EngineError::Closed);
            }
            if !Self::qualifies_pressure_flush(&state, browser, expectation) {
                return Err(EngineError::InvalidRequest);
            }
            let entry = state
                .browsers
                .get_mut(&browser)
                .ok_or(EngineError::Closed)?;
            if !entry.session.snapshot().is_ready()
                || entry.cleanup_started
                || entry.command_pressure.is_some()
            {
                return Err(EngineError::InvalidRequest);
            }
            entry.command_pressure = Some(crate::test_support::CommandPressure::default());
        }
        // Native PostTask is outside the registry borrow. A refusal rolls back
        // only the still-unexecuted reservation on this exact Browser.
        if let Err(error) = executor.post(TaskPayload::ProbeCommandPressureClose {
            browser,
            expectation,
        }) {
            if let Some(entry) = self.lock().browsers.get_mut(&browser)
                && entry
                    .command_pressure
                    .as_ref()
                    .is_some_and(|probe| probe.thread == 0)
            {
                entry.command_pressure = None;
            }
            return Err(error);
        }
        Ok(())
    }

    #[cfg(feature = "test-support")]
    fn qualifies_pressure_flush(
        state: &RegistryState,
        a: BrowserId,
        expectation: crate::test_support::PressureExpectation,
    ) -> bool {
        let Some((profile, sequence)) = expectation.flush() else {
            return true;
        };
        let owner = match expectation.peer_flush() {
            Some(peer)
                if peer.browser != a
                    && peer.browser.runtime() == a.runtime()
                    && profile.local() != 0 =>
            {
                peer.browser
            }
            Some(_) => return false,
            None => a,
        };
        sequence > 0
            && profile.runtime() == a.runtime()
            && state.browsers.get(&owner).is_some_and(|entry| {
                entry.session.snapshot().is_ready()
                    && !entry.cleanup_started
                    && entry.session.configuration().profile == Some(profile)
            })
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn command_pressure_status(
        &self,
        browser: BrowserId,
    ) -> Option<crate::test_support::CommandPressure> {
        self.lock().browsers.get(&browser)?.command_pressure.clone()
    }

    #[cfg(feature = "test-support")]
    fn sample_managed_pressure_cut(
        &self,
        browser: BrowserId,
        cut: crate::test_support::ManagedPressureCut,
    ) -> Result<crate::test_support::ManagedPressureReceipt, String> {
        use crate::test_support::{
            ManagedPressureReceipt, PRESSURE_EVENT_CAPACITY, PRESSURE_TITLE_PREFIX,
        };
        let state = self.snapshot(browser);
        let title = state
            .navigation
            .title
            .strip_prefix(PRESSURE_TITLE_PREFIX)
            .and_then(|value| value.parse::<u64>().ok());
        let menu = self
            .menu_bridge
            .get(browser)
            .ok_or("original managed registration missing")?
            .snapshot();
        if !state.is_ready()
            || title != Some(PRESSURE_EVENT_CAPACITY as u64 + 1)
            || cut.menu.browser != browser
            || !self.menus.is_pending(cut.menu)
            || self.menus.pending() != 1
            || menu.opened != 2
            || menu.published != 2
            || menu.cancelled != 1
            || menu.actions_submitted != 1
            || menu.retired
            || menu.first_failure.is_some()
        {
            return Err("original managed menu/title not present at command pressure".into());
        }
        let source = self.sources.snapshot();
        if cut.source.browser != browser
            || self.sources.held_visitor_probe() != Some(cut.source)
            || source.faulted
            || source.pending != 1
            || !matches!(source.active.as_slice(), [s] if s.operation == cut.source
                && s.native_live && s.host_live && !s.cancelled && s.callback_received)
        {
            return Err("original visitor not pending at command pressure".into());
        }
        if !self.original_peer_find_pending(browser, cut.find) {
            return Err("survivor Find did not remain unresolved at command pressure".into());
        }
        let key = |id: BrowserId, seq| [id.runtime().value(), id.local(), seq];
        Ok(ManagedPressureReceipt {
            menu: key(cut.menu.browser, cut.menu.sequence),
            source: key(cut.source.browser, cut.source.sequence),
            find: key(cut.find.browser, cut.find.sequence),
            title: title.expect("qualified title"),
        })
    }

    #[cfg(feature = "test-support")]
    fn original_peer_find_pending(
        &self,
        a: BrowserId,
        id: rust_cef_core::find::FindOperationId,
    ) -> bool {
        let find = self.finds.snapshot();
        id.browser != a
            && id.browser.runtime() == a.runtime()
            && self.snapshot(id.browser).is_ready()
            && !find.faulted
            && find.first_failure.is_none()
            && find.started == 2
            && find.finished == 1
            && find.pending == 1
            && matches!(find.active.as_slice(), [f] if f.operation==id && f.issued && f.stop_attempted && f.stop_submitted
                && f.outcome.is_none() && f.last_result.is_none() && f.first_failure.is_none())
    }

    #[cfg(feature = "test-support")]
    fn sample_window_menu_cut(
        &self,
        browser: BrowserId,
        request: rust_cef_core::menu::ContextMenuRequestId,
        window: u64,
        source: Option<rust_cef_core::source_view::SourceOperationId>,
        find: Option<rust_cef_core::find::FindOperationId>,
    ) -> Result<crate::test_support::WindowMenuCut, String> {
        let binding = self
            .menu_bridge
            .get(browser)
            .ok_or("original window menu registration missing")?;
        let status = binding.snapshot();
        let lease = binding
            .target
            .lease
            .snapshot()
            .map_err(|e| format!("original window menu lease: {e:?}"))?;
        let prior_source_actions = u64::from(source.is_some());
        if request.browser != browser
            || window == 0
            || lease.binding.browser != browser
            || lease.binding.window != window
            || lease.retired
            || lease.faulted
            || lease.in_flight != 0
            || binding.fixture.load(Ordering::Acquire) != 1
            || status.retired
            || status.first_failure.is_some()
            || status.opened != 1 + prior_source_actions
            || status.published != 1 + prior_source_actions
            || status.cancelled != prior_source_actions
            || status.actions_submitted != prior_source_actions
            || self.menus.pending() != 1
            || !self.menus.is_pending(request)
        {
            return Err("exact original window menu not pending at command pressure".into());
        }
        if let Some(id) = source {
            let d = self.sources.snapshot();
            if id.browser != browser
                || self.sources.held_visitor_probe() != Some(id)
                || d.faulted
                || d.pending != 1
                || d.native_live != 1
                || d.host_live != 1
                || !matches!(d.active.as_slice(), [s] if s.operation == id && s.menu.browser == browser
                    && s.menu.sequence > 0 && s.menu.sequence < request.sequence && s.callback_received
                    && s.native_live && s.host_live && !s.cancelled && s.first_failure.is_none())
            {
                return Err(
                    "original A source visitor did not survive to its pressured close".into(),
                );
            }
        }
        if find.is_some_and(|id| !self.original_peer_find_pending(browser, id)) {
            return Err("original B Find stopped being unresolved at pressured Close".into());
        }
        Ok(crate::test_support::WindowMenuCut {
            menu: [browser.runtime().value(), browser.local(), request.sequence],
            find: find.map(|id| {
                [
                    id.browser.runtime().value(),
                    id.browser.local(),
                    id.sequence,
                ]
            }),
            gpui_window: window,
            source: source.map(|id| {
                [
                    id.browser.runtime().value(),
                    id.browser.local(),
                    id.sequence,
                ]
            }),
        })
    }

    #[cfg(feature = "test-support")]
    fn run_command_pressure_close(
        self: &Arc<Self>,
        browser: BrowserId,
        expectation: crate::test_support::PressureExpectation,
    ) {
        use crate::test_support::{CommandPressure, PressureExpectation};
        let Some(executor) = self.executor() else {
            return;
        };
        let mut w = CommandPressure {
            thread: crate::executor::current_thread_id(),
            ..Default::default()
        };
        w.menu_pending_before = self.menus.pending();
        let outcome = (|| {
            if let PressureExpectation::ManagedMenu(cut) = expectation {
                self.sample_managed_pressure_cut(browser, cut)?;
            } else if let PressureExpectation::WindowMenu(request, window, _, source, find, _) =
                expectation
            {
                self.sample_window_menu_cut(browser, request, window, source, find)?;
            } else if matches!(expectation, PressureExpectation::ProbeMenu(_)) {
                let menu = self
                    .probe_menu_status(browser)
                    .ok_or("original menu status missing")?;
                if w.menu_pending_before != 1
                    || !menu.deferred
                    || menu.copies != 1
                    || menu.cancel_calls != 0
                    || menu.answering.is_some()
                    || menu.error.is_some()
                    || menu
                        .witnesses
                        .first()
                        .is_none_or(|m| m.browser != (browser.runtime().value(), browser.local()))
                {
                    return Err("original menu continuation not pending at pressure entry".into());
                }
            } else if w.menu_pending_before != 0 {
                return Err("no-menu pressure admitted with a pending native menu".into());
            }
            // A single CEF UI turn posts owned commands without executing any
            // of their tasks inline. No UI wait, pump or competing worker.
            for _ in 0..rust_cef_core::session::COMMAND_CAPACITY {
                let id = self
                    .submit_command(browser, &executor, BrowserCommand::StopLoading)
                    .map_err(|e| format!("post pressured command: {e:?}"))?;
                if w.accepted == 0 {
                    w.first_sequence = id.sequence;
                }
                if w.first_sequence.checked_add(w.accepted) != Some(id.sequence) {
                    return Err("noncontiguous admitted command sequence".into());
                }
                w.accepted += 1;
            }
            w.overflow_refused =
                self.submit_command(browser, &executor, BrowserCommand::StopLoading)
                    == Err(EngineError::Backpressure);
            w.pending_at_close = self
                .lock()
                .browsers
                .get(&browser)
                .map_or(0, |e| e.session.pending_commands());
            w.menu_pending_at_close = self.menus.pending();
            if expectation == PressureExpectation::NoMenu && w.menu_pending_at_close != 0 {
                return Err("unexpected menu at no-menu pressure Close".into());
            }
            if let Some((profile, sequence)) = expectation.flush() {
                if expectation.peer_flush().is_some()
                    && !Self::qualifies_pressure_flush(&self.lock(), browser, expectation)
                {
                    return Err("original survivor no longer owns the named flush profile".into());
                }
                if !self
                    .flushes
                    .get()
                    .and_then(Weak::upgrade)
                    .is_some_and(|flushes| {
                        flushes.probe_marker_pending_at_close(profile, sequence, w.thread)
                    })
                {
                    return Err("exact original cookie flush not pending at pressured close".into());
                }
                w.held_cookie_flush_at_close =
                    Some([profile.runtime().value(), profile.local(), sequence]);
                w.peer_flush_browser_at_close = expectation
                    .peer_flush()
                    .map(|f| (f.browser.runtime().value(), f.browser.local()));
            }
            if let PressureExpectation::ManagedMenu(cut) = expectation {
                if !w.overflow_refused
                    || w.pending_at_close != rust_cef_core::session::COMMAND_CAPACITY
                {
                    return Err("managed close requires a full original command queue".into());
                }
                w.held_managed_at_close = Some(self.sample_managed_pressure_cut(browser, cut)?);
            } else if let PressureExpectation::WindowMenu(
                request,
                window,
                _,
                source,
                find,
                prints,
            ) = expectation
            {
                if !w.overflow_refused
                    || w.pending_at_close != rust_cef_core::session::COMMAND_CAPACITY
                {
                    return Err("window menu close requires a full original command queue".into());
                }
                w.held_window_menu_at_close =
                    Some(self.sample_window_menu_cut(browser, request, window, source, find)?);
                if let Some(cut) = prints {
                    let qualified = cut.ids.into_iter().zip(cut.windows).all(|(id, window)| {
                        self.prints.probe_status(id).is_some_and(|status| {
                            status.origin.binding.browser == id.browser
                                && status.origin.binding.window == window
                                && status.phase
                                    == rust_cef_core::print::PrintPhase::AwaitingConfirmation
                                && !status.cancel_requested
                                && !status.resources_retired
                                && status.accepted_job.is_none()
                                && status.outcome.is_none()
                        })
                    });
                    if !qualified {
                        return Err(
                            "original print confirmations not pending at pressure Close cut".into(),
                        );
                    }
                    w.held_window_prints_at_close = Some(cut.identities());
                }
            }
            match self.schedule_browser_close(browser, CloseMode::Force, None) {
                Some(Ok(task)) => w.close_task = Some((task.runtime.value(), task.sequence)),
                Some(Err(error)) => return Err(format!("original close post rejected: {error:?}")),
                None => return Err("original close was not posted".into()),
            }
            w.new_command_refused_after_intent =
                self.submit_command(browser, &executor, BrowserCommand::StopLoading)
                    == Err(EngineError::Closing);
            if !(w.overflow_refused && w.new_command_refused_after_intent) {
                return Err("command backpressure or close slot mismatch".into());
            }
            Ok(())
        })();
        w.error = outcome.err();
        if let Some(entry) = self.lock().browsers.get_mut(&browser) {
            entry.command_pressure = Some(w);
        }
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn probe_menu_status(
        &self,
        browser: BrowserId,
    ) -> Option<crate::test_support::MenuProbeSnapshot> {
        let (client, retired) = {
            let s = self.lock();
            let e = s.browsers.get(&browser)?;
            (e.client_state.clone(), e.menu_probe_final.clone())
        };
        let mut snapshot = client
            .map(|c| c.menu_probe.snapshot())
            .or_else(|| retired.map(|p| p.snapshot()))?;
        snapshot.pending_continuations = self.menus.pending();
        Some(snapshot)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn probe_menu_deferred_enable(
        &self,
        browser: BrowserId,
        lease: rust_cef_core::menu_lease::MenuLease,
    ) -> Result<(), EngineError> {
        let client = {
            let s = self.lock();
            s.browsers
                .get(&browser)
                .filter(|e| !s.draining && !e.cleanup_started && e.session.snapshot().is_ready())
                .and_then(|e| e.client_state.clone())
                .ok_or(EngineError::Closed)?
        };
        let nz = |n| std::num::NonZeroUsize::new(n).unwrap();
        if lease
            .snapshot()
            .map_err(|_| EngineError::InvalidRequest)?
            .binding
            .browser
            != browser
        {
            return Err(EngineError::InvalidRequest);
        }
        self.menus.configure(
            nz(1),
            rust_cef_core::menu::MenuLimits {
                max_entries: nz(32),
                max_label_bytes: nz(256),
                max_total_label_bytes: nz(2048),
            },
        )?;
        client.menu_probe.enable_deferred(lease)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn probe_menu_cancel(&self, browser: BrowserId) -> Result<(), EngineError> {
        let client = self
            .lock()
            .browsers
            .get(&browser)
            .and_then(|e| e.client_state.clone())
            .ok_or(EngineError::Closed)?;
        let (id, binding, revision) = client.menu_probe.reply_scope(browser)?;
        self.post_menu_cancel(id, binding, revision)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn menu_probe_cancel_work(&self, work: crate::menu_native::MenuWork) {
        work.cancel(&self.audit);
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn menu_probe_native_call(&self) {
        self.audit.record();
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn menu_probe_matches(&self, browser: BrowserId, native: &Browser) -> bool {
        let current = {
            let s = self.lock();
            s.browsers
                .get(&browser)
                .filter(|e| !e.cleanup_started && e.session.snapshot().state == SessionState::Alive)
                .and_then(|e| e.native.clone())
        };
        if let Some(current) = current {
            let mut current = current.as_ref().clone();
            self.audit.record();
            native.is_same(Some(&mut current)) != 0
        } else {
            false
        }
    }
    #[cfg(feature = "test-support")]
    fn menu_probe_open_now(&self, browser: BrowserId) {
        let pair = {
            let s = self.lock();
            if s.draining {
                None
            } else {
                s.browsers
                    .get(&browser)
                    .filter(|e| {
                        !e.cleanup_started && e.session.snapshot().state == SessionState::Alive
                    })
                    .and_then(|e| Some((e.native.clone()?, e.client_state.clone()?)))
            }
        };
        let Some((native, client)) = pair else { return };
        if !client.menu_probe.start_input() {
            return;
        }
        let Some(host) = native.host() else {
            client.menu_probe.fail("native host unavailable");
            return;
        };
        self.audit.record();
        host.set_focus(1);
        let mut event = MouseEvent {
            x: 20,
            y: 20,
            modifiers: 0,
        };
        self.audit.record();
        host.send_mouse_move_event(Some(&event), 0);
        event.modifiers = rust_cef_core::keymap::EVENTFLAG_RIGHT_MOUSE_BUTTON;
        client.menu_probe.sent_mouse();
        self.audit.record();
        host.send_mouse_click_event(Some(&event), MouseButtonType::RIGHT, 0, 1);
        event.modifiers = 0;
        client.menu_probe.sent_mouse();
        self.audit.record();
        host.send_mouse_click_event(Some(&event), MouseButtonType::RIGHT, 1, 1);
    }
    /// Enable CEF spellchecking on the browser's own request context. A dictionary
    /// on disk is not enough: Chromium needs `browser.enable_spellchecking` plus
    /// `spellcheck.dictionaries` before it will offer 200..=204 suggestions.
    fn apply_spelling_preferences(&self, browser: BrowserId) {
        let mut detail = String::from("start");
        let native = {
            let s = self.lock();
            s.browsers
                .get(&browser)
                .filter(|e| {
                    !s.draining
                        && !e.cleanup_started
                        && e.session.snapshot().state == SessionState::Alive
                })
                .and_then(|e| e.native.clone())
        };
        let mut ok = false;
        if let Some(native) = native {
            detail = "no-host".into();
            self.audit.record();
            if let Some(host) = native.host() {
                detail = "no-context".into();
                self.audit.record();
                if let Some(context) = host.request_context() {
                    let enable_name = CefString::from("browser.enable_spellchecking");
                    let dicts_name = CefString::from("spellcheck.dictionaries");
                    self.audit.record();
                    let can_enable = context.can_set_preference(Some(&enable_name)) != 0;
                    self.audit.record();
                    let can_dicts = context.can_set_preference(Some(&dicts_name)) != 0;

                    let mut enable = match cef::value_create() {
                        Some(v) => v,
                        None => return,
                    };
                    enable.set_bool(1);
                    let mut error_enabled = WritableCefString::new();
                    let mut error_enabled_cef = error_enabled.as_mut_cef();
                    self.audit.record();
                    let enabled_ok = context.set_preference(
                        Some(&enable_name),
                        Some(&mut enable),
                        Some(&mut error_enabled_cef),
                    ) != 0;

                    let mut list = match cef::list_value_create() {
                        Some(v) => v,
                        None => return,
                    };
                    list.set_size(1);
                    list.set_string(0, Some(&CefString::from("en-US")));
                    let mut dictionaries = match cef::value_create() {
                        Some(v) => v,
                        None => return,
                    };
                    dictionaries.set_list(Some(&mut list));
                    let mut error_dictionaries = WritableCefString::new();
                    let mut error_dictionaries_cef = error_dictionaries.as_mut_cef();
                    self.audit.record();
                    let list_ok = context.set_preference(
                        Some(&dicts_name),
                        Some(&mut dictionaries),
                        Some(&mut error_dictionaries_cef),
                    ) != 0;

                    let mut list_read = String::from("absent");
                    self.audit.record();
                    if let Some(v) = context.preference(Some(&dicts_name))
                        && let Some(list) = v.list()
                    {
                        let count = list.size();
                        let mut langs = Vec::with_capacity(count.min(4));
                        for i in 0..count.min(4) {
                            let s = list.string(i);
                            if let Ok(lang) = crate::menu_model::read_userfree(&s, 32) {
                                langs.push(lang);
                            }
                        }
                        list_read = format!("size={count}[{}]", langs.join(","));
                    }
                    let mut readback = String::from("absent");
                    self.audit.record();
                    if let Some(v) = context.preference(Some(&enable_name)) {
                        readback = format!("{}", v.bool() != 0);
                    }
                    ok = enabled_ok && list_ok;
                    detail = format!(
                        "can_enable={can_enable} enabled={enabled_ok}({}) can_dicts={can_dicts} dictionaries={list_ok}({}) readback={readback} list={list_read}",
                        error_enabled.to_string_lossy(),
                        error_dictionaries.to_string_lossy()
                    );
                }
            }
        }
        if let Some(binding) = self.menu_bridge.get(browser) {
            binding.spelling.note_preferences(ok, &detail);
        }
    }
    #[cfg(feature = "test-support")]
    fn menu_bridge_open_now(&self, browser: BrowserId, kind: u8) {
        let Some(binding) = self.menu_bridge.get(browser) else {
            return;
        };
        let native = {
            let s = self.lock();
            s.browsers
                .get(&browser)
                .filter(|e| {
                    !s.draining
                        && !e.cleanup_started
                        && e.session.snapshot().state == SessionState::Alive
                })
                .and_then(|e| e.native.clone())
        };
        let Some(native) = native else {
            binding.fail("fixture original Browser unavailable");
            return;
        };
        binding.fixture.store(kind, Ordering::Release);
        self.audit.record();
        let Some(host) = native.host() else {
            binding.fail("fixture original host unavailable");
            return;
        };
        self.audit.record();
        host.set_focus(1);
        let mut event = MouseEvent {
            x: 20,
            y: 20,
            modifiers: 0,
        };
        self.audit.record();
        host.send_mouse_move_event(Some(&event), 0);
        event.modifiers = rust_cef_core::keymap::EVENTFLAG_RIGHT_MOUSE_BUTTON;
        self.audit.record();
        host.send_mouse_click_event(Some(&event), MouseButtonType::RIGHT, 0, 1);
        event.modifiers = 0;
        self.audit.record();
        host.send_mouse_click_event(Some(&event), MouseButtonType::RIGHT, 1, 1);
    }
    /// Executed only from a CEF UI task; all native calls below stay here.
    /// Test-support-only stage marker for the managed DevTools open path. Normal
    /// builds compile this to nothing; it never changes admission or ordering.
    #[cfg(feature = "test-support")]
    fn aux_open_stage(stage: &str) {
        eprintln!("aux-open stage={stage}");
    }
    #[cfg(not(feature = "test-support"))]
    fn aux_open_stage(_stage: &str) {}

    pub(crate) fn execute(
        self: &Arc<Self>,
        payload: TaskPayload,
        #[cfg(feature = "test-support")] task: rust_cef_core::ports::TaskId,
    ) {
        let on_ui = currently_on(ThreadId::UI) != 0;
        match payload {
            #[cfg(feature = "test-support")]
            TaskPayload::ProbePrint { browser } => {
                if on_ui {
                    let native = {
                        let state = self.lock();
                        state
                            .browsers
                            .get(&browser)
                            .and_then(|entry| entry.native.clone())
                    };
                    if let Some(host) = native.as_ref().and_then(|b| b.host()) {
                        eprintln!("probe-print host-print");
                        host.print();
                    } else {
                        eprintln!("probe-print no-host");
                    }
                }
            }
            TaskPayload::ResolvePrintDialog { browser, accept } => {
                if on_ui {
                    crate::print_provider::resolve(browser, accept);
                }
            }
            TaskPayload::OpenDevTools { browser } => {
                // Staged diagnostics stay available to real test-support probes;
                // they are a no-op in normal builds and never gate behaviour.
                if !on_ui {
                    Self::aux_open_stage("off-ui");
                } else {
                    let native = {
                        let state = self.lock();
                        state
                            .browsers
                            .get(&browser)
                            .filter(|entry| {
                                entry.session.snapshot().is_ready() && !entry.cleanup_started
                            })
                            .and_then(|entry| entry.native.clone())
                    };
                    let host = native
                        .as_ref()
                        .filter(|n| n.is_valid() != 0)
                        .and_then(|n| n.host());
                    if native.is_none() {
                        Self::aux_open_stage("no-ready-native");
                    } else if host.is_none() {
                        Self::aux_open_stage("no-host");
                    } else if let (Some(native), Some(host)) = (native.as_ref(), host) {
                        let native_id = native.identifier();
                        if native_id <= 0 {
                            Self::aux_open_stage("bad-native-id");
                        } else {
                            match self.devtools.begin_request() {
                                Err(e) => Self::aux_open_stage(&format!("begin-request:{e:?}")),
                                Ok(serial) => match self.devtools.reserve(browser, serial, native_id) {
                                    Err(e) => Self::aux_open_stage(&format!("reserve:{e:?}")),
                                    Ok(id) => match crate::devtools::context::ExpectedContext::capture_host(
                                        self, id, native, native_id,
                                    ) {
                                        Err(e) => {
                                            Self::aux_open_stage(&format!("capture:{e:?}"));
                                            let _ = self.devtools.rollback(id);
                                        }
                                        Ok(context) => match self.devtools.issued(id) {
                                            Err(e) => {
                                                Self::aux_open_stage(&format!("issued:{e:?}"));
                                                drop(context);
                                                let _ = self.devtools.rollback(id);
                                            }
                                            Ok(()) => match crate::devtools::native::build_for_host(
                                                self, id,
                                                std::sync::Arc::downgrade(&context),
                                            ) {
                                                Err(e) => {
                                                    Self::aux_open_stage(&format!("build:{e:?}"));
                                                    drop(context);
                                                    let _ = self.devtools.abandon_issued(id);
                                                }
                                                Ok(mut client) => {
                                                    let info = crate::devtools::show::window_info(
                                                        0, "GPUI CEF DevTools",
                                                    );
                                                    let settings = crate::devtools::show::settings();
                                                    self.note_auxiliary_native_call();
                                                    host.show_dev_tools(
                                                        Some(&info), Some(&mut client), Some(&settings), None,
                                                    );
                                                    // Same association check CefSharp uses
                                                    // (CefBrowserHostWrapper::HasDevTools): a real
                                                    // DevTools browser must be associated with this one.
                                                    let associated = host.has_dev_tools();
                                                    self.devtools
                                                        .note_associated(id, associated != 0);
                                                    drop(client);
                                                    Self::aux_open_stage(if associated != 0 {
                                                        "show-associated"
                                                    } else {
                                                        "show-unassociated"
                                                    });
                                                }
                                            },
                                        },
                                    },
                                },
                            }
                        }
                    }
                }
            }
            #[cfg(feature = "test-support")]
            TaskPayload::ProbeRendererControl { browser, action } => {
                if on_ui {
                    let native = {
                        let state = self.lock();
                        state
                            .browsers
                            .get(&browser)
                            .filter(|entry| entry.session.snapshot().is_ready())
                            .and_then(|entry| entry.native.clone())
                    };
                    if let Some(native) = native
                        && let Some(host) = native.host()
                    {
                        let previous = self
                            .probe_renderers
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&browser);
                        let probe = previous.unwrap_or_else(|| {
                            crate::renderer_probe::RendererProbe::new(&native, &host, &self.audit)
                        });
                        probe.send(&host, action, &self.audit);
                        let keep = self.lock().browsers.get(&browser).is_some_and(|entry| {
                            entry.native.is_some()
                                && !entry.cleanup_started
                                && !matches!(
                                    entry.session.snapshot().state,
                                    SessionState::Closed | SessionState::CreateFailed
                                )
                        });
                        if keep {
                            let previous = self
                                .probe_renderers
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(browser, probe);
                            if let Some(previous) = previous {
                                previous.retire();
                            }
                        } else {
                            probe.retire();
                        }
                    }
                }
            }
            #[cfg(feature = "test-support")]
            TaskPayload::ProbeDialogControl { browser, action } => {
                if on_ui {
                    let native = {
                        let state = self.lock();
                        state
                            .browsers
                            .get(&browser)
                            .filter(|entry| entry.session.snapshot().is_ready())
                            .and_then(|entry| entry.native.clone())
                    };
                    if let Some(native) = native
                        && let Some(host) = native.host()
                    {
                        // No native SDK call or observer registration destruction
                        // while the registry/probe table lock is held.
                        let previous = self
                            .probe_dialogs
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&browser);
                        let probe = previous.unwrap_or_else(|| {
                            crate::dialog_probe::DialogProbe::new(&native, &host, &self.audit)
                        });
                        probe.send(&host, action, &self.audit);
                        let keep = self.lock().browsers.get(&browser).is_some_and(|entry| {
                            entry.native.is_some()
                                && !entry.cleanup_started
                                && !matches!(
                                    entry.session.snapshot().state,
                                    SessionState::Closed | SessionState::CreateFailed
                                )
                        });
                        if keep {
                            let replaced = self
                                .probe_dialogs
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(browser, probe);
                            if let Some(previous) = replaced {
                                previous.retire();
                            }
                        } else {
                            probe.retire();
                        }
                    }
                }
            }
            #[cfg(feature = "test-support")]
            TaskPayload::ProbeDataNavigation { browser, target } => {
                if on_ui {
                    let native = {
                        let state = self.lock();
                        state
                            .browsers
                            .get(&browser)
                            .filter(|entry| entry.session.snapshot().state == SessionState::Alive)
                            .and_then(|entry| entry.native.clone())
                    };
                    if let Some(native) = native {
                        self.audit.record();
                        if let Some(frame) = native.main_frame() {
                            self.audit.record();
                            if CefString::from(&frame.url())
                                .to_string()
                                .starts_with("data:")
                            {
                                // Deliberately no approve_host_navigation: native CEF
                                // must pass through this Browser's actual policy callback.
                                // This is not the ordinary host Navigate command.
                                self.audit.record();
                                frame.load_url(Some(&CefString::from(target.as_str())));
                            }
                        }
                    }
                }
            }
            #[cfg(feature = "test-support")]
            TaskPayload::ProbeMenuOpen { browser } => {
                if on_ui {
                    self.menu_probe_open_now(browser);
                }
            }
            #[cfg(feature = "test-support")]
            TaskPayload::ProbeMouse { browser, x, y, up } => {
                if on_ui {
                    let native = {
                        let state = self.lock();
                        state
                            .browsers
                            .get(&browser)
                            .filter(|entry| entry.session.snapshot().state == SessionState::Alive)
                            .and_then(|entry| entry.native.clone())
                    };
                    if let Some(native) = native
                        && let Some(host) = native.host()
                    {
                        let event = MouseEvent {
                            x,
                            y,
                            modifiers: if up {
                                0
                            } else {
                                rust_cef_core::keymap::EVENTFLAG_LEFT_MOUSE_BUTTON
                            },
                        };
                        self.audit.record();
                        host.send_mouse_move_event(Some(&event), 0);
                        self.audit.record();
                        host.send_mouse_click_event(
                            Some(&event),
                            MouseButtonType::LEFT,
                            up as i32,
                            1,
                        );
                    }
                }
            }
            TaskPayload::ResizeRefresh { browser, ticket } => {
                if on_ui {
                    self.resize_tick(browser, ticket);
                }
            }
            #[cfg(feature = "test-support")]
            TaskPayload::CreateDeviceFailureProbe { browser, config } => {
                if on_ui {
                    self.create_now_with_capture(
                        browser,
                        config,
                        0xFFFF_FFFF,
                        crate::capture::CaptureEndpoint::prepare_device_failure_probe,
                    );
                } else {
                    self.reject_create(browser, EngineError::WrongThread);
                }
            }
            #[cfg(feature = "test-support")]
            TaskPayload::CreateThenCloseProbe { browser, config } => {
                if !on_ui {
                    self.reject_create(browser, EngineError::WrongThread);
                    return;
                }
                self.create_now(browser, config);
                let armed = self.lock().browsers.get_mut(&browser).is_some_and(|entry| {
                    let snapshot = entry.session.snapshot();
                    if snapshot.state != SessionState::Creating
                        || !snapshot.creation_accepted
                        || entry.native.is_some()
                        || entry.close_mode.is_some()
                    {
                        return false;
                    }
                    entry.late_create_probe = Some(0);
                    true
                });
                if armed {
                    // Same UI task, after the native call accepted the request.
                    // Leave the real callback to adopt and close the Browser.
                    self.note_close_intent(browser, CloseMode::Force);
                }
            }
            #[cfg(feature = "test-support")]
            TaskPayload::CreateCaptureProbe { browser, config } => {
                if on_ui {
                    self.create_now_with_capture(
                        browser,
                        config,
                        0,
                        crate::capture::CaptureEndpoint::prepare,
                    );
                } else {
                    self.reject_create(browser, EngineError::WrongThread);
                }
            }
            #[cfg(feature = "test-support")]
            TaskPayload::ProbePressureTitle { browser, step } => {
                if on_ui {
                    self.probe_pressure_title_now(browser, step);
                }
            }
            #[cfg(feature = "test-support")]
            TaskPayload::ProbeCommandPressureClose {
                browser,
                expectation,
            } => {
                if on_ui {
                    self.run_command_pressure_close(browser, expectation);
                }
            }
            #[cfg(feature = "test-support")]
            TaskPayload::ProbeFrameControl { browser, control } => {
                if on_ui {
                    use crate::test_support::FrameProbeControl;
                    let (client, invalidate) = {
                        let state = self.lock();
                        let Some(entry) = state
                            .browsers
                            .get(&browser)
                            .filter(|entry| entry.session.snapshot().is_ready())
                        else {
                            return;
                        };
                        let invalidate = matches!(
                            control,
                            FrameProbeControl::CaptureFaultAndInvalidate
                                | FrameProbeControl::ResumeCaptureAndInvalidate
                                | FrameProbeControl::InvalidateView
                        ) && !state.draining
                            && !entry.cleanup_started;
                        (
                            entry.client_state.clone(),
                            invalidate.then(|| entry.native.clone()).flatten(),
                        )
                    };
                    if let Some(client) = client
                        && client.frame_probe_control(control)
                        && let Some(native) = invalidate
                    {
                        self.audit.record();
                        if let Some(host) = native.host() {
                            self.audit.record();
                            host.invalidate(PaintElementType::VIEW);
                        }
                    }
                }
            }
            TaskPayload::RetireCapture { browser } => {
                if on_ui {
                    let resume = {
                        let mut state = self.lock();
                        state.browsers.get_mut(&browser).is_some_and(|entry| {
                            if entry.cleanup_started && entry.capture_cleanup_deferred {
                                entry.cleanup_started = false;
                                entry.capture_cleanup_deferred = false;
                                true
                            } else {
                                false
                            }
                        })
                    };
                    if resume {
                        self.before_close(browser);
                    }
                }
            }
            TaskPayload::ProfilePolicyIoBarrier { handle } => {
                if currently_on(ThreadId::IO) != 0 {
                    if let Some(profiles) = self.profiles() {
                        profiles.policy_io_ready(handle);
                    }
                }
            }
            TaskPayload::ProfilePolicyReady { handle } => {
                if on_ui && let Some(profiles) = self.profiles() {
                    profiles.policy_ready(handle);
                }
            }
            TaskPayload::RegisterProfileScheme {
                sequence,
                handle,
                binding,
            } => {
                if let Some(profiles) = self.profiles() {
                    if on_ui {
                        profiles.register_scheme_now(sequence, handle, binding);
                    } else {
                        profiles.fail_scheme_registration(
                            sequence,
                            crate::profiles::ProfileCreateError::PostFailed(
                                EngineError::WrongThread,
                            ),
                        );
                    }
                }
            }
            TaskPayload::ProfileSchemeIoBarrier { sequence } => {
                if currently_on(ThreadId::IO) != 0
                    && let Some(profiles) = self.profiles()
                {
                    profiles.scheme_io_ready(sequence);
                }
            }
            TaskPayload::ProfileSchemeReady { sequence } => {
                if on_ui && let Some(profiles) = self.profiles() {
                    profiles.scheme_ready(sequence);
                }
            }
            TaskPayload::PolicyEvents { browser } => {
                if on_ui {
                    self.flush_policy_events(browser);
                }
            }
            TaskPayload::BeforeUnloadSweep { request } => {
                if on_ui {
                    self.sweep_before_unload(request);
                }
            }
            TaskPayload::Create { browser, config } => {
                if !on_ui {
                    self.reject_create(browser, EngineError::WrongThread);
                    return;
                }
                self.create_now(browser, config);
            }
            TaskPayload::Command(envelope) => {
                if !on_ui {
                    return;
                }
                self.run_command(envelope);
            }
            TaskPayload::InputReset(id) => {
                if !on_ui {
                    return;
                }
                self.run_input_reset(id);
            }
            TaskPayload::Close {
                browser,
                mode,
                ticket,
            } => {
                if !on_ui {
                    return;
                }
                self.close_now_inner(
                    browser,
                    mode,
                    ticket.as_ref(),
                    #[cfg(feature = "test-support")]
                    Some(task),
                );
            }
            TaskPayload::CloseDevTools { browser } => {
                if on_ui {
                    let native = {
                        let state = self.lock();
                        state
                            .browsers
                            .get(&browser)
                            .and_then(|entry| entry.native.clone())
                    };
                    let host = native
                        .as_ref()
                        .filter(|n| n.is_valid() != 0)
                        .and_then(|n| n.host());
                    if let Some(host) = host {
                        self.note_auxiliary_native_call();
                        host.close_dev_tools();
                        // The association may stay true until the DevTools browser
                        // actually goes away, so this is a sample, not a verdict.
                        let still = host.has_dev_tools();
                        self.devtools.note_after_close(browser, still != 0);
                        if let Some(id) = self.devtools.row_id(browser) {
                            self.devtools.note_associated(id, still != 0);
                        }
                    }
                }
            }
            TaskPayload::CloseAfterCreated { browser } => {
                if on_ui {
                    self.finish_input_reset(browser);
                    let mode = self
                        .lock()
                        .browsers
                        .get(&browser)
                        .and_then(|entry| entry.close_mode);
                    if let Some(mode) = mode {
                        self.close_now(browser, mode);
                    }
                }
            }
            #[cfg(feature = "test-support")]
            TaskPayload::ProbeMenuSpellingOpen {
                browser,
                dictionary,
            } => {
                if on_ui {
                    self.menu_bridge_open_now(browser, if dictionary { 6 } else { 5 });
                }
            }
            TaskPayload::SpellingPreferences { browser } => {
                if on_ui {
                    self.apply_spelling_preferences(browser);
                }
            }
            #[cfg(feature = "test-support")]
            TaskPayload::ProbeMenuFindOpen { browser } => {
                if on_ui {
                    self.menu_bridge_open_now(browser, 4);
                }
            }
            #[cfg(feature = "test-support")]
            TaskPayload::ProbeMenuSourceOpen { browser } => {
                if on_ui {
                    self.menu_bridge_open_now(browser, 3);
                }
            }
            #[cfg(feature = "test-support")]
            TaskPayload::ProbeMenuBridgeOpen { browser, print } => {
                if on_ui {
                    self.menu_bridge_open_now(browser, if print { 2 } else { 1 });
                }
            }
            TaskPayload::SourceResult { id, result } => {
                if on_ui {
                    self.deliver_source(id, result);
                } else {
                    self.sources
                        .fault(id, rust_cef_core::source_view::SourceFailure::WrongThread);
                }
            }
            TaskPayload::FindCancel(id) => {
                if on_ui {
                    self.cancel_find_now(id);
                } else {
                    self.finds.fault();
                }
            }
            TaskPayload::AuxiliaryClose(ticket) => {
                if !on_ui {
                    self.devtools.fault();
                } else {
                    // The auxiliary DevTools browser lives in its own top-level
                    // window; closing only that Browser can leave CEF holding
                    // the Client. Ask the original host to close its DevTools
                    // association first, then keep the per-ticket Browser close.
                    let parent = {
                        let state = self.lock();
                        state
                            .browsers
                            .get(&ticket.id.owner)
                            .and_then(|entry| entry.native.clone())
                    };
                    if let Some(host) = parent.as_ref().and_then(|b| b.host()) {
                        self.note_auxiliary_native_call();
                        host.close_dev_tools();
                    }
                    if self.devtools.take_close(ticket)
                        && self.auxiliary_native.close(self, ticket).is_err()
                    {
                        self.devtools.fault();
                    }
                }
            }
            TaskPayload::AuxiliaryNativeReleased { id, native_id } => {
                if !on_ui
                    || self
                        .devtools
                        .native_references_released(id, native_id)
                        .is_err()
                    || self.observe_auxiliary_window(id, native_id).is_err()
                {
                    self.devtools.fault();
                }
            }
            TaskPayload::AuxiliaryReferencesReleased { id, native_id } => {
                if !on_ui
                    || self.devtools.references_released(id, native_id).is_err()
                    || self.observe_auxiliary_window(id, native_id).is_err()
                {
                    self.devtools.fault();
                }
            }
            TaskPayload::AuxiliaryClientReleased { id, native_id } => {
                if !on_ui
                    || self.devtools.client_returned(id, native_id).is_err()
                    || self.observe_auxiliary_window(id, native_id).is_err()
                {
                    self.devtools.fault();
                }
            }
            #[cfg(feature = "test-support")]
            TaskPayload::ProbeReleaseSourceVisitor(id) => {
                if on_ui {
                    self.sources.release_visitor_probe(id);
                } else {
                    self.sources
                        .fault(id, rust_cef_core::source_view::SourceFailure::WrongThread);
                }
            }
            TaskPayload::SourceReleased(id) => {
                if on_ui {
                    self.sources.released(id);
                } else {
                    self.sources
                        .fault(id, rust_cef_core::source_view::SourceFailure::WrongThread);
                }
            }
            #[cfg(feature = "test-support")]
            TaskPayload::ProbeAuxiliaryPreparation {
                request,
                reservation,
                witness,
            } => {
                let result = if !on_ui {
                    self.menus.rollback(reservation);
                    Err(EngineError::WrongThread)
                } else if let Some(work) = self.menus.take_cancel(reservation) {
                    let result = self.probe_prepare_unissued_auxiliary(request, &work);
                    work.cancel(&self.audit);
                    result
                } else {
                    Err(EngineError::InvalidRequest)
                };
                witness.complete(result);
            }
            TaskPayload::MenuReply(ticket) => {
                if on_ui {
                    if let Some((work, response)) = self.menus.take_reply(ticket) {
                        self.perform_menu_reply(work, response);
                    }
                }
            }
            TaskPayload::RetireMenuBinding { browser, serial } => {
                if on_ui {
                    let _ = self.sources.cancel(Some(browser), Some(serial), None);
                    self.cancel_finds(Some(browser), Some(serial));
                }
                if on_ui
                    && self
                        .menu_bridge
                        .get(browser)
                        .is_some_and(|b| b.serial == serial)
                {
                    if let Some(work) = self.menus.take_browser(browser) {
                        work.cancel(&self.audit);
                    }
                }
            }
            TaskPayload::MenuCancel(ticket) => {
                if on_ui {
                    if let Some(work) = self.menus.take_cancel(ticket) {
                        work.cancel(&self.audit);
                    }
                }
            }
            TaskPayload::DrainRequests => {
                if !on_ui {
                    return;
                }
                self.cancel_all_requests(true);
            }
            TaskPayload::CreateProfile { handle } => {
                if !on_ui {
                    // Plain-data failure transition: no CEF call, and the
                    // profile must not stay Pending (which would block the
                    // shutdown gate forever).
                    if let Some(profiles) = self.profiles() {
                        profiles.fail_creation(handle);
                    }
                    return;
                }
                // Drain is sampled on this UI sequence, so a task that never
                // issued a native context terminates as Failed instead of
                // starting a new context during shutdown.
                let draining = self.is_draining();
                if let Some(profiles) = self.profiles() {
                    profiles.create_profile_now(handle, draining);
                }
            }
            TaskPayload::CompareContexts {
                sequence,
                left,
                right,
            } => {
                if !on_ui {
                    if let Some(profiles) = self.profiles() {
                        profiles.record_comparison(
                            sequence,
                            Err(crate::profiles::ProfileCreateError::PostFailed(
                                EngineError::WrongThread,
                            )),
                        );
                    }
                    return;
                }
                if let Some(profiles) = self.profiles() {
                    profiles.run_comparison(sequence, left, right);
                }
            }
            TaskPayload::ProfileInitialized { handle } => {
                if !on_ui {
                    return;
                }
                if let Some(profiles) = self.profiles() {
                    // Runs after the CEF callback frame returned: the handler's
                    // borrowed context is already gone.
                    profiles.mark_initialized(handle);
                }
            }
            TaskPayload::BeginCookieFlush { sequence, handle } => {
                if on_ui && let Some(flushes) = self.flushes.get().and_then(Weak::upgrade) {
                    flushes.begin_now(sequence, handle);
                }
            }
            TaskPayload::CookieManagerReady { sequence, handle } => {
                if on_ui && let Some(flushes) = self.flushes.get().and_then(Weak::upgrade) {
                    flushes.manager_ready(sequence, handle);
                }
            }
            TaskPayload::CookieFlushCompleted { sequence } => {
                if on_ui && let Some(flushes) = self.flushes.get().and_then(Weak::upgrade) {
                    flushes.completed(sequence);
                }
            }
            TaskPayload::EvalTimeout { id } => {
                if on_ui {
                    self.complete_eval(id, None, EvalOutcome::Err(EvalError::Timeout));
                }
            }
            TaskPayload::StartDownload { browser, url } => {
                if !on_ui {
                    return;
                }
                let native = {
                    let state = self.lock();
                    state
                        .browsers
                        .get(&browser)
                        .filter(|entry| {
                            !entry.cleanup_started
                                && !matches!(
                                    entry.session.snapshot().state,
                                    SessionState::Closed
                                        | SessionState::Closing
                                        | SessionState::CreateFailed
                                )
                        })
                        .and_then(|entry| entry.native.clone())
                };
                if let Some(host) = native
                    .as_ref()
                    .filter(|n| n.is_valid() != 0)
                    .and_then(|n| n.host())
                {
                    self.audit.record();
                    host.start_download(Some(&CefString::from(url.as_str())));
                }
            }
            TaskPayload::ConfirmDownload { id, destination } => {
                if !on_ui {
                    return;
                }
                if let Some(policy) = self.policy.get() {
                    self.host_requests.resolve_confirm_download(
                        id,
                        destination,
                        policy,
                        &self.audit,
                    );
                }
            }
            TaskPayload::CancelDownload { id } => {
                if on_ui {
                    self.host_requests.resolve_cancel_download(id, &self.audit);
                }
            }
            TaskPayload::ResolveFileDialog { id, decision } => {
                if on_ui {
                    self.host_requests
                        .resolve_file_dialog(id, decision, &self.audit);
                }
            }
            TaskPayload::ResolveJsDialog { id, decision } => {
                if on_ui {
                    self.host_requests
                        .resolve_js_dialog(id, decision, &self.audit);
                }
            }
            TaskPayload::ResolvePermission { id, decision } => {
                if on_ui {
                    self.host_requests
                        .resolve_permission(id, decision, &self.audit);
                }
            }
            TaskPayload::ResolveAuth { id, decision } => {
                if on_ui {
                    self.host_requests.resolve_auth(id, decision, &self.audit);
                }
            }
            TaskPayload::ResolveCertificate { id, decision } => {
                if on_ui {
                    self.host_requests
                        .resolve_certificate(id, decision, &self.audit);
                }
            }
            TaskPayload::VisitProfileCookies {
                request_id,
                handle,
                url,
                include_http_only,
            } => {
                if let Some(cookies) = self.profile_cookies.get().and_then(Weak::upgrade) {
                    cookies.run_visit_cookies(on_ui, request_id, handle, url, include_http_only);
                }
            }
            TaskPayload::SetProfileCookie {
                request_id,
                handle,
                url,
                cookie,
            } => {
                if let Some(cookies) = self.profile_cookies.get().and_then(Weak::upgrade) {
                    cookies.run_set_cookie(on_ui, request_id, handle, url, cookie);
                }
            }
            TaskPayload::DeleteProfileCookies {
                request_id,
                handle,
                url,
                cookie_name,
            } => {
                if let Some(cookies) = self.profile_cookies.get().and_then(Weak::upgrade) {
                    cookies.run_delete_cookies(on_ui, request_id, handle, url, cookie_name);
                }
            }
            TaskPayload::ClearProfileHttpAuth { request_id, handle } => {
                if let Some(cookies) = self.profile_cookies.get().and_then(Weak::upgrade) {
                    cookies.run_clear_http_auth(on_ui, request_id, handle);
                }
            }
            TaskPayload::SetProfilePreference {
                request_id,
                handle,
                name,
                value,
            } => {
                if let Some(cookies) = self.profile_cookies.get().and_then(Weak::upgrade) {
                    cookies.run_set_preference(on_ui, request_id, handle, name, value);
                }
            }
            TaskPayload::PrintToPdf {
                id,
                destination,
                options,
            } => {
                let native = if on_ui {
                    self.ready_browser_native(id.browser)
                } else {
                    None
                };
                self.pdf_exports.run_print_to_pdf(
                    on_ui,
                    id,
                    destination,
                    options,
                    native,
                    &self.audit,
                    &self.ui,
                );
            }
            TaskPayload::AttachCdpObserver { browser } => {
                let Some(observer) = self.headless_cdp.take_pending_observer(browser) else {
                    return;
                };
                if let Some(host) = on_ui.then(|| self.ready_cdp_host(browser)).flatten() {
                    let _ = self.headless_cdp.attach_observer(
                        true,
                        browser,
                        &host,
                        observer,
                        &self.audit,
                    );
                }
            }
            TaskPayload::DetachCdpObserver { browser } => {
                self.headless_cdp.detach_browser(on_ui, browser);
            }
            TaskPayload::SendCdpMessage {
                browser,
                json_message,
            } => {
                if let Some(host) = on_ui.then(|| self.ready_cdp_host(browser)).flatten() {
                    let _ = self
                        .headless_cdp
                        .send_message(true, &host, &json_message, &self.audit);
                }
            }
            TaskPayload::ExecuteCdpMethod {
                id,
                method,
                params_json,
            } => {
                if let Some(host) = on_ui.then(|| self.ready_cdp_host(id.browser)).flatten() {
                    let _ = self.headless_cdp.execute_method(
                        true,
                        &host,
                        &method,
                        params_json.as_deref(),
                        id.message_id,
                        &self.audit,
                    );
                }
            }
            TaskPayload::StartUrlRequest { id, spec } => {
                if let Some(urls) = self.url_requests() {
                    urls.run_start(on_ui, id, spec, self);
                }
            }
            TaskPayload::CancelUrlRequest { id } => {
                if let Some(urls) = self.url_requests() {
                    urls.run_cancel(on_ui, id, false);
                }
            }
            TaskPayload::TimeoutUrlRequest { id } => {
                if let Some(urls) = self.url_requests() {
                    urls.run_cancel(on_ui, id, true);
                }
            }
            TaskPayload::DestroyNativeChildHwnd { browser, hwnd } => {
                if on_ui {
                    let expected_parent = self
                        .lock()
                        .browsers
                        .get(&browser)
                        .and_then(|entry| entry.native_hwnd.as_ref())
                        .map(|child| child.parent_hwnd);
                    if let Some(parent) = expected_parent {
                        self.audit.record();
                        let _ = crate::native_hwnd::destroy_native_child_hwnd(hwnd, parent);
                    }
                }
            }
        }
    }

    pub(crate) fn ready_browser_native(&self, browser: BrowserId) -> Option<Browser> {
        let state = self.lock();
        state
            .browsers
            .get(&browser)
            .filter(|entry| !entry.cleanup_started && entry.session.snapshot().is_ready())
            .and_then(|entry| entry.native.as_ref().map(|arc| (**arc).clone()))
    }

    fn ready_cdp_host(&self, browser: BrowserId) -> Option<BrowserHost> {
        let native = {
            let state = self.lock();
            state
                .browsers
                .get(&browser)
                .filter(|entry| {
                    entry.headless_cdp_enabled
                        && !entry.cleanup_started
                        && entry.session.snapshot().is_ready()
                })
                .and_then(|entry| entry.native.clone())
        }?;
        (native.is_valid() != 0).then(|| native.host()).flatten()
    }

    fn create_now(self: &Arc<Self>, browser: BrowserId, config: BrowserConfig) {
        self.create_now_with_capture(
            browser,
            config,
            0xFFFF_FFFF,
            crate::capture::CaptureEndpoint::prepare,
        );
    }

    fn create_now_with_capture(
        self: &Arc<Self>,
        browser: BrowserId,
        config: BrowserConfig,
        background_color: u32,
        prepare_capture: impl FnOnce(
            &crate::capture::CaptureEndpoint,
            RenderPreference,
        ) -> Result<(), EngineError>,
    ) {
        let (store, viewport, draining) = {
            let state = self.lock();
            let Some(entry) = state.browsers.get(&browser) else {
                return;
            };
            if entry.session.snapshot().state != SessionState::Creating {
                return;
            }
            (entry.store.clone(), entry.viewport.clone(), state.draining)
        };
        if draining {
            // The host accepted the create before drainage began. Do not issue a
            // new native browser during shutdown; the instance is terminal.
            self.reject_create(browser, EngineError::Closing);
            return;
        }
        // A browser is never bound to an implicit fallback context: the selected
        // profile must be Ready on the UI sequence, and its durable context is
        // the only one passed to CEF. `None` was already normalized to the
        // explicit default handle by the runtime, so a missing handle here is a
        // rejected instance, never a silent fallback. The clone stays
        // task-local and is dropped after the native call.
        let Some(profile) = config.profile else {
            self.reject_create(browser, EngineError::NotReady);
            return;
        };
        let Some(profiles) = self.profiles() else {
            self.reject_create(browser, EngineError::NotReady);
            return;
        };
        if profiles.state_of(profile).ok() != Some(rust_cef_core::profiles::ProfileState::Ready) {
            self.reject_create(browser, EngineError::NotReady);
            return;
        }
        let Some(mut request_context) = profiles.context_for(profile) else {
            self.reject_create(browser, EngineError::NotReady);
            return;
        };
        let ax_tree = {
            // Mark the in-flight native creation before issuing it so a close
            // racing this task waits for OnAfterCreated instead of retiring the
            // instance as if no native browser existed. Re-check the state: a
            // close task could have retired this instance between the two locks.
            let mut state = self.lock();
            let Some(entry) = state.browsers.get_mut(&browser) else {
                return;
            };
            if entry.session.snapshot().state != SessionState::Creating || entry.create_issued {
                return;
            }
            entry.create_issued = true;
            entry.ax_tree.clone()
        };
        let Some(policy) = self.policy() else {
            self.reject_create(browser, EngineError::PolicyDenied);
            return;
        };
        let client_state = Arc::new(
            BrowserClientState::new(
                browser,
                viewport,
                store,
                self.frames.clone(),
                self.ui.clone(),
                Arc::downgrade(self),
                policy,
                &config.initial_url,
            )
            .with_options(config.options.clone())
            .with_accessibility(ax_tree),
        );
        let native_cfg = config.options.surface_mode.native_child_hwnd();
        if let Some(ref cfg) = native_cfg
            && !crate::native_hwnd::is_valid_window(cfg.parent_hwnd)
        {
            self.reject_create(browser, EngineError::InvalidRequest);
            return;
        }
        if let Some(entry) = self.lock().browsers.get_mut(&browser) {
            entry.capture = native_cfg.is_none().then(|| client_state.capture.clone());
            entry.recovery = Some(client_state.recovery.clone());
            entry.client_state = Some(client_state.clone());
        }
        if native_cfg.is_none()
            && let Err(error) = prepare_capture(&client_state.capture, config.render_preference)
        {
            self.reject_create(browser, error);
            return;
        }
        // Every browser needs its own client/handler set; reusing one would mix
        // paint and lifecycle callbacks.
        let mut client =
            BrowserClientBuilder::build_for_surface(client_state.clone(), native_cfg.is_some());
        let window_info = match native_cfg {
            Some(ref cfg) => crate::native_hwnd::build_child_window_info(cfg),
            None => WindowInfo {
                windowless_rendering_enabled: 1,
                shared_texture_enabled: i32::from(
                    config.render_preference == RenderPreference::D3d11Required,
                ),
                runtime_style: RuntimeStyle::ALLOY,
                ..Default::default()
            },
        };
        let settings = build_browser_settings(&config.options, background_color);
        let mut extra_info = crate::js_bridge_native::build_browser_extra_info(
            browser,
            &config.options,
            &client_state.policy.rules,
        );
        let url = CefString::from(config.initial_url.as_str());
        if client_state.capture.is_abandoned() {
            self.reject_create(browser, EngineError::Closed);
            return;
        }
        if self.is_draining() {
            self.reject_create(browser, EngineError::Closing);
            return;
        }
        self.audit.record();
        let accepted = browser_host_create_browser(
            Some(&window_info),
            Some(&mut client),
            Some(&url),
            Some(&settings),
            extra_info.as_mut(),
            Some(&mut request_context),
        );
        // The task-local reference is not needed after the call; the profile
        // table keeps its own until the pre-shutdown release.
        drop(request_context);
        // Our wrapper reference is not needed after the call; CEF holds its own.
        drop(client);
        if accepted == 0 {
            // Terminal: no native browser will ever arrive for this instance.
            self.reject_create(browser, EngineError::Backend { code: None });
            return;
        }
        let mut state = self.lock();
        if let Some(entry) = state.browsers.get_mut(&browser)
            && entry.session.snapshot().state == SessionState::Creating
        {
            // Handler state was registered before issuing native creation, so
            // synchronous callbacks and later host data navigation share it.
            entry.session.accept_create_request();
        }
    }

    /// Serializes reservation and asynchronous post with close intent. Native
    /// command execution happens only in a later UI task, outside this lock.
    fn submit_command(
        &self,
        browser: BrowserId,
        executor: &Executor,
        command: BrowserCommand,
    ) -> Result<CommandId, EngineError> {
        if browser.runtime() != self.runtime {
            return Err(EngineError::WrongRuntime);
        }
        if let BrowserCommand::ResolveBeforeUnload { request, allow } = command {
            return self.submit_before_unload_response(browser, executor, request, allow);
        }
        if let BrowserCommand::FinishOutboundDrag(finish) = command {
            return self.submit_outbound_drag_finish(browser, executor, finish);
        }
        if matches!(&command, BrowserCommand::EvaluateScript { id, .. } if !self.eval_is_pending(*id))
        {
            return Err(EngineError::InvalidRequest);
        }
        if let BrowserCommand::ResetInput(reason) = command {
            let mut state = self.lock();
            let entry = state
                .browsers
                .get_mut(&browser)
                .ok_or(EngineError::Closed)?;
            // The slot is cleared here so the outside-lock wake can carry the
            // cleared revision; `queue_reset` clears it again as the shared
            // admission point for every other reset reason (a no-op here).
            let ime_cleared = entry.ime.invalidate();
            let result = Self::queue_reset(browser, entry, executor, reason)
                .and_then(|id| Self::retry_reset(entry, executor, id));
            drop(state);
            if ime_cleared {
                let _ = self.ui.wake();
            }
            return result;
        }
        if let BrowserCommand::SetViewport(viewport) = &command {
            pixel_extent(viewport.width, viewport.height, viewport.scale)
                .map_err(|_| EngineError::InvalidRequest)?;
        }
        if let BrowserCommand::SetNativeBounds(rect) = &command {
            rect.validate().map_err(|_| EngineError::InvalidRequest)?;
        }
        if matches!(
            &command,
            BrowserCommand::SetFocus(false) | BrowserCommand::SetVisible(false)
        ) {
            // Focus/visibility loss ends the usable composition geometry even
            // before the reset completion runs. Own lock, wake after it.
            self.invalidate_ime(browser);
        }
        if let BrowserCommand::Navigate(ref url) = command
            && let Some(policy) = self.policy()
            && !policy.rules.allows_host_navigation(url)
        {
            policy.deny(Some(browser), PolicyAction::Navigation);
            return Err(EngineError::PolicyDenied);
        }
        let mut state = self.lock();
        if state.draining && !matches!(&command, BrowserCommand::Input(_)) {
            return Err(EngineError::Closing);
        }
        let draining = state.draining;
        let entry = state
            .browsers
            .get_mut(&browser)
            .ok_or(EngineError::Closed)?;
        let is_native_child = entry.session.surface_mode().is_native_child_hwnd();
        let ime_revision = entry.ime.revision();
        if !is_native_child
            && entry.session.snapshot().state == SessionState::Alive
            && matches!(
                &command,
                BrowserCommand::SetFocus(false) | BrowserCommand::SetVisible(false)
            )
        {
            let _ = Self::queue_reset(browser, entry, executor, InputResetReason::FocusLost);
        }
        let is_input = matches!(&command, BrowserCommand::Input(_));
        let is_touch = matches!(&command, BrowserCommand::TouchEvent(_));
        let affects_attempt = is_input
            || is_touch
            || matches!(&command, BrowserCommand::MouseClick(_))
            || (!is_native_child && matches!(&command, BrowserCommand::SetFocus(true)));
        let validation_error = match &command {
            BrowserCommand::Input(event) => crate::input::validate_native_input(event).err(),
            BrowserCommand::TouchEvent(touch) => touch.validate_and_normalize().err(),
            _ => None,
        };
        let invalid_epoch = matches!(&command, BrowserCommand::Input(event) if event.epoch != entry.session.input_epoch());
        let result = if let Some(error) = validation_error {
            Err(error)
        } else if draining {
            Err(EngineError::Closing)
        } else if is_input && (entry.session.input_reset_pending() || invalid_epoch) {
            Err(EngineError::Closing)
        } else if entry.session.input_reset_pending()
            && (matches!(
                &command,
                BrowserCommand::MouseClick(_) | BrowserCommand::TouchEvent(_)
            ) || (!is_native_child && matches!(&command, BrowserCommand::SetFocus(true))))
        {
            Err(EngineError::Closing)
        } else {
            entry.session.admit_command(command)
        };
        let envelope = match result {
            Ok(envelope) => envelope,
            Err(error) => {
                if affects_attempt {
                    entry.input_shadow.rejected = entry.input_shadow.rejected.saturating_add(1);
                    if let Some(input) = entry.input.as_mut() {
                        input.rejected = entry.input_shadow.rejected;
                    }
                }
                if is_input {
                    // A full normal lane is a retry opportunity, not a cleanup cutpoint.
                    // Stale epochs and an existing cutpoint cannot advance it again.
                    if entry.session.snapshot().state == SessionState::Alive
                        && !invalid_epoch
                        && !matches!(&error, EngineError::Backpressure | EngineError::Closing)
                    {
                        let _ =
                            Self::queue_reset(browser, entry, executor, InputResetReason::Rejected);
                    }
                }
                let ime_changed = entry.ime.revision() != ime_revision;
                drop(state);
                if ime_changed {
                    let _ = self.ui.wake();
                }
                return Err(error);
            }
        };
        let affects_input = is_input
            || is_touch
            || matches!(&envelope.command, BrowserCommand::MouseClick(_))
            || (!is_native_child && matches!(&envelope.command, BrowserCommand::SetFocus(true)));
        if affects_input {
            entry.input_outstanding.insert(envelope.id.sequence);
        }
        if is_input {
            entry.input_shadow.admitted = entry.input_shadow.admitted.saturating_add(1);
            if let Some(input) = entry.input.as_mut() {
                input.admitted = entry.input_shadow.admitted;
            }
        }
        let id = envelope.id;
        // Holding the same state lock through post's return orders accepted
        // sequence numbers with the close cutpoint. post_task never executes
        // this command inline or waits for its UI execution.
        if let Err(error) = executor.post(TaskPayload::Command(envelope)) {
            entry.session.cancel_command(id);
            if affects_input {
                entry.input_outstanding.remove(&id.sequence);
                entry.input_shadow.rejected = entry.input_shadow.rejected.saturating_add(1);
                if let Some(input) = entry.input.as_mut() {
                    input.rejected = entry.input_shadow.rejected;
                }
            }
            if is_input {
                let _ = Self::queue_reset(browser, entry, executor, InputResetReason::Rejected);
            }
            let ime_changed = entry.ime.revision() != ime_revision;
            drop(state);
            if ime_changed {
                let _ = self.ui.wake();
            }
            return Err(error);
        }
        let ime_changed = entry.ime.revision() != ime_revision;
        drop(state);
        if ime_changed {
            let _ = self.ui.wake();
        }
        Ok(id)
    }

    fn take_command(
        &self,
        envelope: CommandEnvelope,
    ) -> Option<(
        AcceptedCommand,
        Option<NativeBrowser>,
        Arc<ViewportState>,
        Option<Arc<BrowserClientState>>,
    )> {
        let mut state = self.lock();
        let entry = state.browsers.get_mut(&envelope.id.browser)?;
        let command = entry.session.consume_command(envelope)?;
        Some((
            command,
            entry.native.clone(),
            entry.viewport.clone(),
            entry.client_state.clone(),
        ))
    }

    fn run_command(&self, envelope: CommandEnvelope) {
        let browser = envelope.id.browser;
        let id = envelope.id;
        let input_event = match &envelope.command {
            BrowserCommand::Input(event) => Some(event.clone()),
            _ => None,
        };
        let touch_event = match &envelope.command {
            BrowserCommand::TouchEvent(touch) => Some(*touch),
            _ => None,
        };
        // Commit/finish/cancel stop CEF's composition monitoring, which does not
        // guarantee an empty range callback. Their geometry is cleared after the
        // call actually succeeded; an ImeSet instead clears the known old
        // geometry before dispatch so a synchronous callback it triggers is not
        // erased afterwards. Missing/old geometry is never faked as a new one.
        let ime_effect = ime_dispatch_effect(input_event.as_ref());
        let clears_before_dispatch = ime_effect == ImeDispatchEffect::ClearBefore;
        let clears_after_dispatch = ime_effect == ImeDispatchEffect::ClearAfter;
        let mut affects_input = input_event.is_some()
            || touch_event.is_some()
            || matches!(
                &envelope.command,
                BrowserCommand::MouseClick(_) | BrowserCommand::SetFocus(true)
            );
        let mut ledger = if affects_input {
            let mut state = self.lock();
            let Some(entry) = state.browsers.get_mut(&browser) else {
                return;
            };
            if entry.native_hwnd.is_some()
                && matches!(&envelope.command, BrowserCommand::SetFocus(true))
            {
                affects_input = false;
                None
            } else {
                if !entry.input_outstanding.contains(&id.sequence)
                    || !entry.session.has_pending_command(id)
                {
                    return;
                }
                if entry.input.is_none() || entry.input_outstanding.first() != Some(&id.sequence) {
                    // No CEF FIFO assumption: a later task waits for every smaller
                    // accepted input-affecting sequence and for the native call to retire.
                    if !entry.input_deferred.iter().any(|item| item.id == id)
                        && entry.input_deferred.len() < rust_cef_core::session::COMMAND_CAPACITY
                    {
                        entry.input_deferred.push(envelope);
                    }
                    return;
                }
                let ledger = entry.input.take();
                entry.input_shadow = ledger.as_ref().expect("checked input gate").clone();
                ledger
            }
        } else {
            None
        };
        let accepted = self.take_command(envelope);
        if clears_before_dispatch && accepted.is_some() {
            // Owned invalidation only; the wake happens outside every lock.
            self.invalidate_ime(browser);
        }
        // A new ImeSet can synchronously produce a callback during the native
        // call below. Remember the pre-dispatch revision so a successful
        // commit/finish/cancel only clears geometry that no callback replaced.
        let pre_dispatch_revision = if clears_after_dispatch {
            self.ime_revision(browser)
        } else {
            None
        };
        let result = if let Some((command, native, viewport, client_state)) = accepted {
            if let Some(native) = native {
                if let (Some(input), Some(ledger)) = (&input_event, &mut ledger) {
                    native.host().ok_or(EngineError::Closed).and_then(|host| {
                        crate::input::validate_file_drag_viewport(input, viewport.get())?;
                        ledger.dispatch(input, &host, &self.audit)
                    })
                } else if let (Some(touch), Some(ledger)) = (&touch_event, &mut ledger) {
                    native
                        .host()
                        .ok_or(EngineError::Closed)
                        .and_then(|host| ledger.dispatch_touch(touch, &host, &self.audit))
                } else {
                    command.execute(&mut NativeBrowserEngine {
                        browser,
                        native: &native,
                        viewport: &viewport,
                        audit: &self.audit,
                        registry: self,
                        client_state: client_state.as_deref(),
                    })
                }
            } else {
                Err(EngineError::Closed)
            }
        } else {
            Err(EngineError::Closed)
        };
        #[cfg(feature = "test-support")]
        if let Some(w) = self
            .lock()
            .browsers
            .get_mut(&browser)
            .and_then(|e| e.command_pressure.as_mut())
            && w.first_sequence != 0
            && id.sequence >= w.first_sequence
            && id.sequence - w.first_sequence < w.accepted
        {
            if w.first_sequence.checked_add(w.dispatched) != Some(id.sequence) || result.is_err() {
                w.error.get_or_insert_with(|| {
                    format!(
                        "command {} did not dispatch in sequence: {result:?}",
                        id.sequence
                    )
                });
            } else {
                w.dispatched += 1;
            }
        }
        if clears_after_dispatch
            && result.is_ok()
            && let Some(observed) = pre_dispatch_revision
        {
            self.invalidate_stale_ime(browser, observed);
        }
        if affects_input {
            let (next, ime_changed) = {
                let mut state = self.lock();
                let Some(entry) = state.browsers.get_mut(&browser) else {
                    return;
                };
                let ime_revision = entry.ime.revision();
                let failed = result.err();
                if let Some(mut ledger) = ledger {
                    if let Some(error) = &failed {
                        ledger.last_native_error = Some(error.clone());
                    }
                    entry.restore_input(ledger);
                }
                entry.input_outstanding.remove(&id.sequence);
                if failed.is_some() {
                    if let Some(executor) = self.executor() {
                        let _ = Self::queue_reset(
                            browser,
                            entry,
                            &executor,
                            InputResetReason::Rejected,
                        );
                    }
                }
                // Reentrant execution of a later task cannot move its session
                // token until all earlier native input has actually returned.
                let first = entry.input_outstanding.first().copied();
                let next = first
                    .and_then(|sequence| {
                        entry
                            .input_deferred
                            .iter()
                            .position(|item| item.id.sequence == sequence)
                    })
                    .map(|position| entry.input_deferred.remove(position));
                (next, entry.ime.revision() != ime_revision)
            };
            if ime_changed {
                let _ = self.ui.wake();
            }
            if let Some(next) = next {
                self.run_command(next);
            }
            self.finish_input_reset(browser);
        }
    }

    fn run_input_reset(&self, id: CommandId) {
        let mut state = self.lock();
        if let Some(entry) = state.browsers.get_mut(&id.browser)
            && entry.session.input_reset_id() == Some(id)
        {
            entry.input_reset_posted = false;
            entry.input_reset_ready = true;
        }
        drop(state);
        self.finish_input_reset(id.browser);
    }

    fn finish_input_reset(&self, browser: BrowserId) {
        let (mut ledger, native, id) = {
            let mut state = self.lock();
            let Some(entry) = state.browsers.get_mut(&browser) else {
                return;
            };
            if !entry.input_reset_ready
                || entry.input_cleanup_running
                || !entry.input_outstanding.is_empty()
                || !entry.input_deferred.is_empty()
            {
                return;
            }
            let Some(id) = entry.session.input_reset_id() else {
                return;
            };
            let Some(ledger) = entry.input.take() else {
                return;
            };
            entry.input_shadow = ledger.clone();
            entry.input_cleanup_running = true;
            (ledger, entry.native.clone(), id)
        };
        let result = native
            .and_then(|native| native.host())
            .ok_or(EngineError::Closed)
            .map(|host| {
                ledger.cleanup(&host, &self.audit);
            });
        let mut state = self.lock();
        let succeeded = result.is_ok();
        let mut deferred_close = None;
        let mut ime_cleared = false;
        if let Some(entry) = state.browsers.get_mut(&browser) {
            let terminal = matches!(
                entry.session.snapshot().state,
                SessionState::Closed | SessionState::CreateFailed
            ) || entry.cleanup_started;
            if let Err(error) = result {
                ledger.cleanup_failed = true;
                ledger.last_native_error = Some(error);
            } else if !terminal {
                entry.session.complete_input_reset(id);
                entry.input_reset_ready = false;
                ledger.cleanup_failed = false;
                entry.input_shadow.cleanup_failed = false;
            } else {
                ledger.cleanup_failed = true;
                ledger.last_native_error = Some(EngineError::Closed);
            }
            // The native composition was actually released: clear any geometry
            // that a callback retained before the reset completed.
            ime_cleared = entry.ime.invalidate();
            entry.restore_input(ledger);
            entry.input_cleanup_running = false;
            if succeeded && !entry.session.input_reset_pending() {
                deferred_close = entry.close_waiting_input.take();
            }
        }
        drop(state);
        if ime_cleared {
            let _ = self.ui.wake();
        }
        if let Some(close) = deferred_close {
            self.close_now_inner(
                browser,
                close.mode,
                close.ticket.as_ref(),
                #[cfg(feature = "test-support")]
                None,
            );
        }
    }

    #[cfg(feature = "test-support")]
    fn cancel_menu_from_close_task(
        &self,
        work: crate::menu_native::MenuWork,
        id: rust_cef_core::ports::TaskId,
    ) {
        let request = work.request();
        let menu = [
            request.browser.runtime().value(),
            request.browser.local(),
            request.sequence,
        ];
        let task = (id.runtime.value(), id.sequence);
        let mut returned = false;
        // Same cancellation and terminal path as cancel(); the marker callback
        // runs only after native Cancel returns and its wrapper is released.
        work.finish(&self.audit, || returned = true);
        if returned
            && let Some(w) = self
                .lock()
                .browsers
                .get_mut(&request.browser)
                .and_then(|e| e.command_pressure.as_mut())
            && w.close_task == Some(task)
            && (w
                .held_managed_at_close
                .as_ref()
                .is_some_and(|cut| cut.menu == menu)
                || w.held_window_menu_at_close
                    .is_some_and(|cut| cut.menu == menu))
        {
            w.managed_menu_cancelled_by_close =
                Some(crate::test_support::ManagedCloseCancel { task, menu });
        }
    }

    fn close_now(&self, browser: BrowserId, mode: CloseMode) {
        self.close_now_inner(
            browser,
            mode,
            None,
            #[cfg(feature = "test-support")]
            None,
        );
    }

    fn close_now_inner(
        &self,
        browser: BrowserId,
        mode: CloseMode,
        ticket: Option<&CloseTicket>,
        #[cfg(feature = "test-support")] task: Option<rust_cef_core::ports::TaskId>,
    ) {
        enum Outcome {
            Native(NativeBrowser),
            RetireNoNative,
            WaitForCreation,
            Nothing,
        }
        let outcome = {
            let mut state = self.lock();
            let Some(entry) = state.browsers.get_mut(&browser) else {
                return;
            };
            // A queued or input-deferred caller close is still the original
            // attempt, not authority to close whichever state is current now.
            if !pending_close_ticket(browser, ticket) {
                return;
            }
            if entry.session.input_retired() || entry.session.input_reset_pending() {
                entry.close_waiting_input = Some(DeferredClose {
                    mode,
                    ticket: ticket.cloned(),
                });
                if entry.session.input_retired() {
                    return;
                }
                // This close task provides one bounded opportunity to recover a
                // rejected cleanup post. No timer and no repeated hidden retry.
                if !entry.input_reset_posted && !entry.input_reset_ready {
                    if let (Some(id), Some(executor)) =
                        (entry.session.input_reset_id(), self.executor())
                    {
                        let _ = Self::retry_reset(entry, &executor, id);
                    }
                }
                return;
            }
            if matches!(
                entry.session.snapshot().state,
                SessionState::CreateFailed | SessionState::Closed
            ) {
                // Keep reference/handler cleanup idempotent: nothing new to do.
                Outcome::Nothing
            } else if entry.close_deferred() {
                // Injected close timeout: keep the intent and tombstone, leave
                // close_sent unconsumed, and leave the Browser genuinely alive.
                entry.session.close_requested();
                if mode == CloseMode::Force {
                    entry.close_mode = Some(CloseMode::Force);
                } else if entry.close_mode.is_none() {
                    entry.close_mode = Some(mode);
                }
                Outcome::Nothing
            } else if entry.session.snapshot().state == SessionState::Creating
                && !entry.create_issued
            {
                // No native browser was ever requested for this instance, so
                // there is nothing to close asynchronously. Record the intent and
                // go straight to a terminal state; `create_rejected` alone only
                // accepts Creating, so requesting the close first would leave a
                // permanent Closing instance with no native callback coming.
                entry.close_mode.get_or_insert(mode);
                entry.close_sent = true;
                entry.force_sent = true;
                entry.session.create_rejected();
                entry.store.close();
                if let Some(completion) = entry.close_completion.take() {
                    completion.complete(CloseResult::Closed);
                }
                Outcome::RetireNoNative
            } else if entry.close_sent && mode == CloseMode::Force && !entry.force_sent {
                // A Force escalation after an already dispatched Graceful close
                // must reach the native instance exactly once.
                entry.session.close_requested();
                entry.close_mode = Some(CloseMode::Force);
                match entry.native.clone() {
                    Some(native) => {
                        entry.force_sent = true;
                        Outcome::Native(native)
                    }
                    None => Outcome::Nothing,
                }
            } else {
                entry.session.close_requested();
                entry.close_mode.get_or_insert(mode);
                if entry.close_sent {
                    Outcome::Nothing
                } else {
                    match entry.native.clone() {
                        Some(native) => {
                            entry.close_sent = true;
                            entry.force_sent = mode == CloseMode::Force;
                            Outcome::Native(native)
                        }
                        // Creation was already issued and OnAfterCreated has not
                        // arrived: wait for the callback, which applies the
                        // close intent.
                        None => {
                            if entry.close_posted == Some(mode) {
                                entry.close_posted = None;
                            }
                            Outcome::WaitForCreation
                        }
                    }
                }
            }
        };
        match outcome {
            Outcome::Native(native) => {
                self.request_auxiliary_close(Some(browser), None);
                self.headless_cdp.detach_browser(true, browser);
                let _ = self.sources.cancel(Some(browser), None, None);
                self.cancel_finds(Some(browser), None);
                if let Some(done) = self.outbound_drags.take_active_for_browser(browser)
                    && let Some(host) = native.host()
                {
                    self.audit.record();
                    host.drag_source_ended_at(
                        done.x,
                        done.y,
                        crate::outbound_drag::core_mask_to_cef(done.operation),
                    );
                    self.audit.record();
                    host.drag_source_system_drag_ended();
                }
                // Still before native CloseBrowser: retain accounting through
                // synchronous Cancel/dismissal and the last wrapper Release.
                if let Some(work) = self.menus.take_browser(browser) {
                    #[cfg(feature = "test-support")]
                    if let Some(task) = task.filter(|_| work.is_managed_native()) {
                        self.cancel_menu_from_close_task(work, task);
                    } else {
                        work.cancel(&self.audit);
                    }
                    #[cfg(not(feature = "test-support"))]
                    work.cancel(&self.audit);
                }
                let pending = if mode == CloseMode::Force {
                    let caps = self.host_capabilities();
                    self.host_requests
                        .cancel_browser_all(browser, false, &self.audit, &caps);
                    self.dialogs.take_browser(browser)
                } else {
                    Vec::new()
                };
                // For an existing pending dialog, Alloy only marks ACCEPTED;
                // it still needs its callback released AFTER this upgrade.
                let closed = self
                    .close_native_ref(&native, mode == CloseMode::Force)
                    .is_some();
                if mode == CloseMode::Force && closed {
                    self.finish_force_close(browser);
                }
                for request in pending {
                    self.audit.record();
                    request.answer(false);
                    self.dialog_answered.fetch_add(1, Ordering::Relaxed);
                }
            }
            Outcome::RetireNoNative => {
                // The instance went straight to a terminal state: a pending
                // composition may never be reported again.
                self.invalidate_ime(browser);
                self.emit(browser, BrowserEvent::CreateFailed(EngineError::Closing));
            }
            Outcome::WaitForCreation | Outcome::Nothing => {}
        }
    }

    fn finish_force_close(&self, browser: BrowserId) {
        let pending = {
            let mut state = self.lock();
            let Some(entry) = state.browsers.get_mut(&browser) else {
                return;
            };
            entry.force_close_returned = true;
            entry.deferred_force_dialog.take()
        };
        if let Some(pending) = pending {
            self.audit.record();
            pending.answer(false);
            self.dialog_answered.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Owner is being dropped before the barrier completed. Native objects are
    /// leaked deliberately: releasing them here would be an invalid CEF call
    /// sequence and the documented contract requires process exit instead.
    pub(crate) fn abandon_native_refs(&self) {
        self.dialogs.abandon();
        self.menus.abandon();
        if let Some(urls) = self.url_requests() {
            urls.abandon_all();
        }
        #[cfg(feature = "test-support")]
        {
            let probes =
                std::mem::take(&mut *self.probe_dialogs.lock().unwrap_or_else(|e| e.into_inner()));
            drop(probes); // Probe Drop abandons Registration, never native Release here.
            let renderers = std::mem::take(
                &mut *self
                    .probe_renderers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            );
            drop(renderers);
        }
        let (natives, clients, deferred_dialogs) = {
            let mut state = self.lock();
            let mut natives = Vec::new();
            let mut clients = Vec::new();
            let mut deferred_dialogs = Vec::new();
            for entry in state.browsers.values_mut() {
                entry.store.close();
                if let Some(native) = entry.native.take() {
                    natives.push(native);
                }
                if let Some(client) = entry.client_state.take() {
                    clients.push(client);
                }
                if let Some(dialog) = entry.deferred_force_dialog.take() {
                    deferred_dialogs.push(dialog);
                }
            }
            (natives, clients, deferred_dialogs)
        };
        for dialog in deferred_dialogs {
            dialog.abandon();
        }
        for native in natives {
            std::mem::forget(native);
        }
        for client in &clients {
            client.recovery.cancel(true);
            client.capture.abandon();
        }
        drop(clients);
    }

    /// Called after the barrier: clears remaining native/handler references.
    /// Any continuation still present is cancelled outside the lock.
    pub(crate) fn release_all_refs(&self, on_ui: bool) {
        let (natives, clients) = {
            let mut state = self.lock();
            let mut natives = Vec::new();
            let mut clients = Vec::new();
            for entry in state.browsers.values_mut() {
                if let Some(native) = entry.native.take() {
                    natives.push(native);
                }
                if let Some(client) = entry.client_state.take() {
                    clients.push(client);
                }
                entry.store.close();
            }
            (natives, clients)
        };
        // The last native references are dropped after the state lock, so a
        // release callback can never re-enter the registry from under it.
        drop(natives);
        drop(clients);
        self.cancel_all_requests(on_ui);
    }

    pub(crate) fn close_browser(self: &Arc<Self>, browser: BrowserId, mode: CloseMode) {
        let _ = self.schedule_browser_close(browser, mode, None);
    }

    /// The original close-slot admission and post. Return only the result of
    /// this actual post, not a close/retirement acknowledgment.
    fn schedule_browser_close(
        self: &Arc<Self>,
        browser: BrowserId,
        mode: CloseMode,
        ticket: Option<&CloseTicket>,
    ) -> Option<Result<rust_cef_core::ports::TaskId, EngineError>> {
        if !self.note_close_intent_for_ticket(browser, mode, ticket) {
            return None;
        }
        {
            let mut state = self.lock();
            // The lock-free notification above may cancel the exact ticket or
            // post a Force upgrade through the same handle. Do not turn a
            // cancelled request into a fresh close of the original Browser.
            if !pending_close_ticket(browser, ticket) {
                return None;
            }
            let entry = state.browsers.get_mut(&browser)?;
            let terminal = matches!(
                entry.session.snapshot().state,
                SessionState::Closed | SessionState::CreateFailed
            );
            if terminal {
                return None;
            }
            let already = match entry.close_posted {
                Some(CloseMode::Force) => true,
                Some(CloseMode::Graceful) => mode == CloseMode::Graceful,
                None => false,
            };
            if already {
                return None;
            }
            entry.close_posted = Some(mode);
        }
        let executor = self.executor()?;
        let posted = executor.post_close(TaskPayload::Close {
            browser,
            mode,
            ticket: ticket.cloned(),
        });
        if posted.is_err() {
            // A failed post must not pretend the close was scheduled: clear the
            // marker so a later host request can retry.
            let mut state = self.lock();
            if pending_close_ticket(browser, ticket)
                && let Some(entry) = state.browsers.get_mut(&browser)
                && entry.close_posted == Some(mode)
            {
                entry.close_posted = None;
            }
        }
        Some(posted)
    }

    fn emit(&self, browser: BrowserId, event: BrowserEvent) -> PublishResult {
        let sequence = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let published = self.events.publish(BrowserEventEnvelope {
            browser,
            sequence,
            event,
        });
        // Wake only with owned data; this never borrows GPUI from an FFI frame.
        let _ = self.ui.wake();
        published
    }

    /// The only close path. It records the native entry after confirming we are
    /// on the CEF UI sequence; the guard above is not a native call.
    fn close_native_ref(&self, native: &Browser, force: bool) -> Option<()> {
        if currently_on(ThreadId::UI) == 0 {
            return None;
        }
        self.audit.record();
        native.host().map(|host| host.close_browser(force as i32))
    }
}

/// How one accepted input command affects the retained IME anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImeDispatchEffect {
    /// Keys, mouse and other commands leave geometry untouched.
    None,
    /// A new composition retires the previous anchor before dispatch.
    ClearBefore,
    /// A commit/finish/cancel retires the anchor only after a successful call.
    ClearAfter,
}

fn ime_dispatch_effect(event: Option<&InputEvent>) -> ImeDispatchEffect {
    match event.map(|event| &event.kind) {
        Some(InputKind::ImeSet { .. }) => ImeDispatchEffect::ClearBefore,
        Some(
            InputKind::Text(_)
            | InputKind::ReplaceText { .. }
            | InputKind::ImeFinish { .. }
            | InputKind::ImeCancel,
        ) => ImeDispatchEffect::ClearAfter,
        _ => ImeDispatchEffect::None,
    }
}

/// Primitive, owned target for the single supported pointer gesture. Production
/// forwards each call to CEF; the unit test records. The gesture order exists
/// only in `dispatch_mouse_click`, so tests never re-implement it.
trait MouseClickTarget {
    fn focus(&mut self, focused: bool);
    fn move_to(&mut self, x: i32, y: i32, modifiers: u32);
    fn button(&mut self, x: i32, y: i32, modifiers: u32, up: bool, click_count: i32);
}

/// One complete left click: focus, move, down, up. Coordinates stay in DIPs;
/// no viewport scale is applied here. Every native entry is audited immediately
/// before the call, exactly like the other commands.
fn dispatch_mouse_click(click: MouseClick, audit: &NativeAudit, target: &mut dyn MouseClickTarget) {
    let modifiers = rust_cef_core::keymap::modifier_flags(
        click.modifiers.shift,
        click.modifiers.control,
        click.modifiers.alt,
        click.modifiers.platform,
    );
    let click_count = i32::from(click.click_count.get());
    audit.record();
    target.focus(true);
    audit.record();
    target.move_to(click.x, click.y, modifiers);
    audit.record();
    target.button(click.x, click.y, modifiers, false, click_count);
    audit.record();
    target.button(click.x, click.y, modifiers, true, click_count);
}

struct NativeMouseClickTarget<'a> {
    host: &'a BrowserHost,
}

impl MouseClickTarget for NativeMouseClickTarget<'_> {
    fn focus(&mut self, focused: bool) {
        self.host.set_focus(focused as i32);
    }

    fn move_to(&mut self, x: i32, y: i32, modifiers: u32) {
        let event = MouseEvent { x, y, modifiers };
        self.host.send_mouse_move_event(Some(&event), 0);
    }

    fn button(&mut self, x: i32, y: i32, modifiers: u32, up: bool, click_count: i32) {
        let event = MouseEvent { x, y, modifiers };
        self.host.send_mouse_click_event(
            Some(&event),
            MouseButtonType::LEFT,
            up as i32,
            click_count,
        );
    }
}

/// Task-local native adapter. No registry lock is held during engine execution.
struct NativeBrowserEngine<'a> {
    browser: BrowserId,
    native: &'a Browser,
    viewport: &'a ViewportState,
    audit: &'a NativeAudit,
    registry: &'a UiRegistry,
    client_state: Option<&'a BrowserClientState>,
}

impl BrowserEngine for NativeBrowserEngine<'_> {
    fn execute(&mut self, envelope: CommandEnvelope) -> Result<(), EngineError> {
        let host = self.native.host().ok_or(EngineError::Closed)?;
        match envelope.command {
            BrowserCommand::Navigate(url) => {
                if let Some(state) = self.client_state {
                    state.approve_host_navigation(&url);
                }
                let frame = self.native.main_frame().ok_or(EngineError::NotReady)?;
                self.audit.record();
                frame.load_url(Some(&CefString::from(url.as_str())));
            }
            BrowserCommand::Back => {
                self.audit.record();
                self.native.go_back();
            }
            BrowserCommand::Forward => {
                self.audit.record();
                self.native.go_forward();
            }
            BrowserCommand::Reload => {
                self.audit.record();
                self.native.reload();
            }
            BrowserCommand::StopLoading => {
                self.audit.record();
                self.native.stop_load();
            }
            BrowserCommand::SetFocus(focused) => {
                self.audit.record();
                host.set_focus(focused as i32);
            }
            BrowserCommand::SetVisible(visible) => {
                if let Some((browser_hwnd, rect)) = self
                    .registry
                    .native_set_visible_state(self.browser, visible)
                {
                    if let Some(hwnd) = browser_hwnd {
                        self.audit.record();
                        if visible {
                            let _ = crate::native_hwnd::show_native_hwnd(hwnd, rect);
                        } else {
                            let _ = crate::native_hwnd::hide_native_hwnd(hwnd);
                        }
                    }
                } else {
                    if let Some(state) = self.client_state {
                        state
                            .set_visible(visible)
                            .map_err(|_| EngineError::Closed)?;
                    }
                    self.audit.record();
                    host.was_hidden(!visible as i32);
                }
            }
            BrowserCommand::SetViewport(next) => {
                let scale_changed = self.viewport.get().scale != next.scale;
                if let Some(state) = self.client_state {
                    state
                        .set_viewport(next)
                        .map_err(|_| EngineError::InvalidRequest)?;
                } else {
                    self.viewport.set(next);
                }
                if scale_changed {
                    self.audit.record();
                    host.notify_screen_info_changed();
                }
                self.audit.record();
                host.was_resized();
            }
            BrowserCommand::SetNativeBounds(rect) => {
                rect.validate().map_err(|_| EngineError::InvalidRequest)?;
                let (browser_hwnd, visible) = self
                    .registry
                    .update_native_bounds_state(self.browser, rect)?;
                if let Some(hwnd) = browser_hwnd {
                    self.audit.record();
                    if !visible || rect.is_empty() {
                        let _ = crate::native_hwnd::hide_native_hwnd(hwnd);
                    } else {
                        let _ = crate::native_hwnd::apply_native_bounds(hwnd, rect);
                    }
                }
            }
            BrowserCommand::NotifyParentMoved => {
                self.audit.record();
                host.notify_move_or_resize_started();
            }
            BrowserCommand::MouseClick(click) => {
                let mut target = NativeMouseClickTarget { host: &host };
                dispatch_mouse_click(click, self.audit, &mut target);
            }
            BrowserCommand::Input(_) | BrowserCommand::ResetInput(_) => {
                return Err(EngineError::InvalidRequest);
            }
            BrowserCommand::SetZoomLevel(level) => {
                let requested_milli =
                    zoom_level_to_milli(level).map_err(|_| EngineError::InvalidRequest)?;
                self.audit.record();
                host.set_zoom_level(level);
                let applied_milli =
                    zoom_level_to_milli(host.zoom_level()).unwrap_or(requested_milli);
                self.registry
                    .navigation_event(self.browser, BrowserEvent::ZoomLevelChanged(applied_milli));
            }
            BrowserCommand::SetAudioMuted(muted) => {
                self.audit.record();
                host.set_audio_muted(i32::from(muted));
                let applied = host.is_audio_muted() != 0;
                self.registry
                    .navigation_event(self.browser, BrowserEvent::AudioMutedChanged(applied));
            }
            BrowserCommand::ResolveBeforeUnload { request, allow } => {
                self.registry.answer_before_unload(request, allow)?;
            }
            BrowserCommand::ExecuteJavaScript {
                script,
                frame_scope,
            } => {
                crate::js_bridge_native::execute_javascript(
                    self.native,
                    self.audit,
                    self.client_state,
                    &script,
                    &frame_scope,
                )?;
            }
            BrowserCommand::EvaluateScript {
                id,
                script,
                frame_scope,
                timeout_ms,
            } => {
                if self.registry.eval_is_pending(id) {
                    if let Some(executor) = self.registry.executor() {
                        let delay_ms = i64::try_from(timeout_ms).unwrap_or(i64::MAX);
                        let _ = executor.post_delayed(TaskPayload::EvalTimeout { id }, delay_ms);
                    }
                    if let Err(err) = crate::js_bridge_native::send_eval_request(
                        self.browser,
                        self.native,
                        self.audit,
                        self.client_state,
                        id,
                        &script,
                        &frame_scope,
                    ) {
                        self.registry.complete_eval(id, None, EvalOutcome::Err(err));
                    }
                }
            }
            BrowserCommand::PostJsMessage { payload_json } => {
                crate::js_bridge_native::send_post_js_message(
                    self.browser,
                    self.native,
                    self.audit,
                    self.client_state,
                    &payload_json,
                )?;
            }
            BrowserCommand::FinishOutboundDrag(finish) => {
                let done = self.registry.outbound_drags.complete(finish)?;
                self.audit.record();
                host.drag_source_ended_at(
                    done.x,
                    done.y,
                    crate::outbound_drag::core_mask_to_cef(done.operation),
                );
                self.audit.record();
                host.drag_source_system_drag_ended();
            }
            BrowserCommand::SetAccessibilityState(feature_state) => {
                if let Some(state) = self.client_state {
                    state
                        .accessibility
                        .set_enabled(feature_state == CefFeatureState::Enabled);
                }
                self.audit.record();
                host.set_accessibility_state(crate::accessibility::cef_feature_to_state(
                    feature_state,
                ));
            }
            BrowserCommand::TouchEvent(_) => {
                return Err(EngineError::InvalidRequest);
            }
        }
        Ok(())
    }
}

/// Owned by the public `BrowserHandle`; it never exposes a native pointer.
pub(crate) struct RuntimeBrowserControl {
    browser: BrowserId,
    executor: Arc<Executor>,
    registry: Arc<UiRegistry>,
    next_close: AtomicU64,
    active_close: Mutex<Option<CloseTicket>>,
}

impl RuntimeBrowserControl {
    pub(crate) fn new(
        browser: BrowserId,
        executor: Arc<Executor>,
        registry: Arc<UiRegistry>,
    ) -> Arc<Self> {
        Arc::new(Self {
            browser,
            executor,
            registry,
            next_close: AtomicU64::new(1),
            active_close: Mutex::new(None),
        })
    }
}

impl BrowserControl for RuntimeBrowserControl {
    fn submit(&self, command: BrowserCommand) -> Result<CommandId, EngineError> {
        if self.executor.is_stopped() {
            return Err(EngineError::Closed);
        }
        self.registry
            .submit_command(self.browser, &self.executor, command)
    }

    fn snapshot(&self) -> BrowserSnapshot {
        self.registry.snapshot(self.browser)
    }

    fn input_snapshot(&self) -> Result<InputSnapshot, EngineError> {
        self.registry.input_snapshot(self.browser)
    }

    fn ime_snapshot(&self) -> Result<ImeSnapshot, EngineError> {
        self.registry.ime_snapshot(self.browser)
    }

    fn take_ux_events(&self) -> Vec<BrowserEvent> {
        self.registry.take_ux_events(self.browser)
    }

    fn take_js_events(&self) -> Vec<BrowserEvent> {
        self.registry.take_js_events(self.browser)
    }

    fn evaluate_script(
        &self,
        script: String,
        frame_scope: JsFrameScope,
        timeout_ms: u64,
    ) -> Result<EvalRequestId, EngineError> {
        if self.executor.is_stopped() {
            return Err(EngineError::Closed);
        }
        self.registry.submit_eval(
            self.browser,
            &self.executor,
            script,
            frame_scope,
            timeout_ms,
        )
    }

    fn request_close(&self, mode: CloseMode) -> CloseTicket {
        let ticket = {
            let mut active = self
                .active_close
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(ticket) = active.as_ref()
                && ticket.result() == CloseResult::Pending
            {
                ticket.clone()
            } else {
                let sequence = self.next_close.fetch_add(1, Ordering::Relaxed);
                let (ticket, completion) = CloseTicket::pending(self.browser, sequence);
                self.registry
                    .attach_close_completion(self.browser, completion);
                *active = Some(ticket.clone());
                ticket
            }
        };
        // Drop the ticket lock before a host wake or native-task admission.
        // Keep the registered completion alive for a nested request to reuse.
        let _ = self
            .registry
            .schedule_browser_close(self.browser, mode, Some(&ticket));
        ticket
    }
}

/// Call under the registry state lock, which also protects ticket completion.
/// An unticketed Runtime drain/probe keeps the existing Browser-wide behavior.
fn pending_close_ticket(browser: BrowserId, ticket: Option<&CloseTicket>) -> bool {
    ticket.is_none_or(|ticket| ticket.browser == browser && ticket.result() == CloseResult::Pending)
}

/// One pending frame/state wake belongs to the mailbox, not to the observer's
/// acceptance result. Neither the CEF callback nor this function borrows GPUI.
pub(crate) fn wake_frames(
    mailbox: &FrameMailbox,
    ui: &Arc<dyn UiDispatcher>,
    ticket: Option<WakeTicket>,
) {
    if let Some(ticket) = ticket {
        if ui.wake().is_err() {
            mailbox.wake_failed(ticket);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attachment::BrowserAttachment;
    use rust_cef_core::ime::{ImeRect, ImeRejection, ImeStatus, MAX_IME_CHARACTER_BOUNDS};
    use rust_cef_core::ports::{BrowserHandle, OwnedFrame};
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct Sinks {
        events: StdMutex<Vec<BrowserEvent>>,
    }
    impl BrowserEventSink for Sinks {
        fn publish(&self, envelope: BrowserEventEnvelope) -> PublishResult {
            self.events.lock().unwrap().push(envelope.event);
            PublishResult::Published
        }
    }
    impl FrameSink for Sinks {
        fn publish(&self, _frame: OwnedFrame) -> PublishResult {
            PublishResult::Published
        }
    }
    impl UiDispatcher for Sinks {
        fn wake(&self) -> Result<(), EngineError> {
            Ok(())
        }
    }

    /// Same owned sinks plus a wake counter, so an IME update can be proven to
    /// notify the host without any frame or pump.
    #[derive(Default)]
    struct ImeSinks {
        events: StdMutex<Vec<BrowserEvent>>,
        wakes: std::sync::atomic::AtomicUsize,
        registry: StdMutex<Weak<UiRegistry>>,
        on_wake: StdMutex<Option<Box<dyn FnOnce() + Send>>>,
    }
    impl BrowserEventSink for ImeSinks {
        fn publish(&self, envelope: BrowserEventEnvelope) -> PublishResult {
            self.events.lock().unwrap().push(envelope.event);
            PublishResult::Published
        }
    }
    impl FrameSink for ImeSinks {
        fn publish(&self, _frame: OwnedFrame) -> PublishResult {
            PublishResult::Published
        }
    }
    impl UiDispatcher for ImeSinks {
        fn wake(&self) -> Result<(), EngineError> {
            if let Some(registry) = self.registry.lock().unwrap().upgrade() {
                assert!(
                    registry.state.try_lock().is_ok(),
                    "IME wake held the registry lock"
                );
            }
            self.wakes.fetch_add(1, Ordering::AcqRel);
            let on_wake = self.on_wake.lock().unwrap().take();
            if let Some(on_wake) = on_wake {
                on_wake();
            }
            Ok(())
        }
    }

    fn ime_registry() -> (Arc<UiRegistry>, Arc<ImeSinks>) {
        let sinks = Arc::new(ImeSinks::default());
        let registry = UiRegistry::new(
            RuntimeGeneration::new(1),
            sinks.clone(),
            sinks.clone(),
            sinks.clone(),
            NativeAudit::new(),
        );
        *sinks.registry.lock().unwrap() = Arc::downgrade(&registry);
        (registry, sinks)
    }

    fn ime_rect(x: i32, y: i32, width: i32, height: i32) -> ImeRect {
        ImeRect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn auxiliary_created_supplement_never_retries_a_refused_close_post() {
        let (r, _, _, executor) = registry_with_posting(false);
        let owner = BrowserId::new(RuntimeGeneration::new(1), 78);
        alive(&r, owner);
        r.devtools
            .configure(std::num::NonZeroUsize::new(1).unwrap())
            .unwrap();
        let id = r.devtools.reserve(owner, 1, 101).unwrap();
        r.devtools.issued(id).unwrap();
        r.devtools.created(id, 201).unwrap();
        r.devtools.native_adopted(id, 201).unwrap();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = attempts.clone();
        executor.set_test_post(Arc::new(move |payload| {
            if matches!(payload, TaskPayload::AuxiliaryClose(_)) {
                seen.fetch_add(1, Ordering::AcqRel);
            }
            false
        }));
        r.note_close_intent(owner, CloseMode::Force);
        assert_eq!(attempts.load(Ordering::Acquire), 1);
        assert_eq!(r.devtools.close_post_failures(), 1);
        r.schedule_auxiliary_close(id);
        r.set_draining();
        assert_eq!(attempts.load(Ordering::Acquire), 1);
        assert_eq!(r.pending_requests(), 1);
        r.devtools.retry_close(id).unwrap();
        r.schedule_auxiliary_close(id);
        assert_eq!(attempts.load(Ordering::Acquire), 2);
        assert_eq!(r.devtools.close_post_failures(), 2);
        assert_eq!(r.pending_requests(), 1);
    }

    #[test]
    fn auxiliary_close_post_is_outside_parent_lifecycle_lock_and_coalesced() {
        let (r, _, _, executor) = registry_with_posting(true);
        let owner = BrowserId::new(RuntimeGeneration::new(1), 77);
        alive(&r, owner);
        r.devtools
            .configure(std::num::NonZeroUsize::new(1).unwrap())
            .unwrap();
        let id = r.devtools.reserve(owner, 1, 101).unwrap();
        r.devtools.issued(id).unwrap();
        r.devtools.created(id, 201).unwrap();
        r.devtools.native_adopted(id, 201).unwrap();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let output = seen.clone();
        let weak = Arc::downgrade(&r);
        executor.set_test_post(Arc::new(move |payload| {
            let r = weak.upgrade().unwrap();
            assert!(r.state.try_lock().is_ok());
            assert_eq!(r.devtools.pending(), 1);
            if let TaskPayload::AuxiliaryClose(ticket) = payload {
                output.lock().unwrap().push(*ticket);
            }
            true
        }));
        r.note_close_intent(owner, CloseMode::Force);
        assert_eq!(r.snapshot(owner).state, SessionState::Closing);
        assert_eq!(seen.lock().unwrap().len(), 1);
        r.note_close_intent(owner, CloseMode::Force);
        r.set_draining();
        assert_eq!(seen.lock().unwrap().len(), 1);
        let ticket = seen.lock().unwrap()[0];
        assert!(r.devtools.take_close(ticket));
        assert!(!r.devtools.take_close(ticket));
        assert_eq!(r.pending_requests(), 1);
    }

    #[test]
    fn auxiliary_original_profile_metadata_never_falls_back_after_parent_close() {
        let (r, _) = registry();
        let a = BrowserId::new(RuntimeGeneration::new(1), 75);
        let b = BrowserId::new(RuntimeGeneration::new(1), 76);
        let profile = rust_cef_core::contracts::ProfileHandle::new(RuntimeGeneration::new(1), 7);
        let mut configured = config();
        configured.profile = Some(profile);
        r.begin_create(a, configured).unwrap();
        assert_eq!(r.recorded_auxiliary_profile(a), Ok(profile));
        r.before_close(a);
        assert_eq!(r.recorded_auxiliary_profile(a), Ok(profile));
        r.begin_create(b, config()).unwrap();
        assert_eq!(r.recorded_auxiliary_profile(b), Err(EngineError::NotReady));
        assert_eq!(
            r.recorded_auxiliary_profile(BrowserId::new(RuntimeGeneration::new(2), 75)),
            Err(EngineError::WrongRuntime)
        );
    }

    #[test]
    fn devtools_close_admission_covers_refused_post_before_late_aux_creation() {
        let (r, _, posted, _executor) = registry_with_posting(false);
        let a = BrowserId::new(RuntimeGeneration::new(1), 73);
        let b = BrowserId::new(RuntimeGeneration::new(1), 74);
        alive(&r, a);
        alive(&r, b);
        r.devtools
            .configure(std::num::NonZeroUsize::new(2).unwrap())
            .unwrap();
        let aux = r.devtools.reserve(a, 1, 101).unwrap();
        let peer = r.devtools.reserve(b, 2, 102).unwrap();
        r.devtools.issued(aux).unwrap();
        r.devtools.issued(peer).unwrap();
        r.close_browser(a, CloseMode::Force);
        assert_eq!(r.snapshot(a).state, SessionState::Closing);
        assert!(posted.lock().unwrap().is_empty());
        assert!(r.lock().browsers[&a].close_posted.is_none());
        assert!(r.devtools.created(aux, 201).unwrap());
        assert!(!r.devtools.created(peer, 202).unwrap());
        assert_eq!(r.pending_requests(), 2);
        assert_eq!(r.devtools.pending_for(a), 1);
        assert_eq!(r.devtools.pending_for(b), 1);
    }

    #[test]
    fn devtools_registry_barrier_keeps_aux_after_parent_close_and_drain() {
        let (r, _) = registry();
        let a = BrowserId::new(RuntimeGeneration::new(1), 71);
        let b = BrowserId::new(RuntimeGeneration::new(1), 72);
        alive(&r, a);
        alive(&r, b);
        r.devtools
            .configure(std::num::NonZeroUsize::new(2).unwrap())
            .unwrap();
        let aux = r.devtools.reserve(a, 1, 101).unwrap();
        let peer = r.devtools.reserve(b, 2, 102).unwrap();
        r.devtools.issued(aux).unwrap();
        r.devtools.issued(peer).unwrap();
        r.before_close(a);
        assert_eq!(r.pending_requests(), 2);
        assert!(r.devtools.created(aux, 201).unwrap());
        assert!(!r.devtools.created(peer, 202).unwrap());
        r.set_draining();
        r.cancel_all_requests(true);
        assert_eq!(r.pending_requests(), 2);
        r.devtools.before_close(aux, 201).unwrap();
        r.devtools.references_released(aux, 201).unwrap();
        assert_eq!(r.pending_requests(), 2);
        r.devtools.callback_returned(aux, 201).unwrap();
        assert_eq!(r.devtools.pending_for(a), 0);
        assert_eq!(r.pending_requests(), 1);
        r.before_close(b);
        assert_eq!(r.pending_requests(), 1);
        r.devtools.before_close(peer, 202).unwrap();
        r.devtools.callback_returned(peer, 202).unwrap();
        assert_eq!(r.pending_requests(), 1);
        r.devtools.references_released(peer, 202).unwrap();
        assert_eq!(r.pending_requests(), 0);
        assert_eq!(r.devtools.reserve(a, 3, 101), Err(EngineError::Closing));
    }

    #[test]
    fn frame_source_identity_viewport_admission_and_same_mailbox_close() {
        use rust_cef_core::contracts::Viewport;
        let (r, _, posted, executor) = registry_with_posting(true);
        let id = BrowserId::new(RuntimeGeneration::new(1), 51);
        alive(&r, id);
        let source = r.frame_source(id).unwrap();
        let foreign = BrowserId::new(RuntimeGeneration::new(2), 51);
        assert!(matches!(
            r.frame_source(foreign),
            Err(EngineError::WrongRuntime)
        ));
        assert_eq!(
            r.frame_budget_stats(foreign),
            Err(EngineError::WrongRuntime)
        );
        let absent = BrowserId::new(RuntimeGeneration::new(1), 52);
        assert!(matches!(r.frame_source(absent), Err(EngineError::Closed)));
        let mut invalid = config();
        invalid.viewport.scale = f32::NAN;
        assert_eq!(
            r.begin_create(absent, invalid),
            Err(CreateError::InvalidViewport)
        );
        assert!(matches!(r.frame_source(absent), Err(EngineError::Closed)));
        assert_eq!(
            r.submit_command(
                id,
                &executor,
                BrowserCommand::SetViewport(Viewport {
                    width: 8193,
                    height: 8192,
                    scale: 1.0,
                })
            ),
            Err(EngineError::InvalidRequest)
        );
        assert!(posted.lock().unwrap().is_empty());
        r.before_close(id);
        assert!(source.snapshot().closed);
        assert!(source.snapshot().view.frame.is_none());
        assert!(Arc::ptr_eq(&source, &r.frame_source(id).unwrap()));
        assert_eq!(r.frame_budget_stats(id).unwrap().used, 0);
    }

    #[test]
    fn renderer_observations_require_the_full_live_browser_identity() {
        use rust_cef_core::contracts::RendererState;
        let (r, sinks) = registry();
        let id = BrowserId::new(RuntimeGeneration::new(1), 1);
        alive(&r, id);
        r.renderer_event(
            BrowserId::new(RuntimeGeneration::new(2), 1),
            BrowserEvent::Unresponsive,
        );
        r.renderer_event(
            BrowserId::new(RuntimeGeneration::new(1), 2),
            BrowserEvent::Unresponsive,
        );
        assert!(sinks.events.lock().unwrap().is_empty());
        r.renderer_event(
            id,
            BrowserEvent::RendererTerminated {
                native_status: 701,
                code: Some(-987),
            },
        );
        assert_eq!(r.snapshot(id).state, SessionState::Alive);
        assert_eq!(r.snapshot(id).renderer.state, RendererState::Terminated);
        assert_eq!(sinks.events.lock().unwrap().len(), 1);
        r.lock().browsers.get_mut(&id).unwrap().cleanup_started = true;
        r.renderer_event(id, BrowserEvent::RenderViewReady);
        assert_eq!(sinks.events.lock().unwrap().len(), 1);
        r.lock().browsers.get_mut(&id).unwrap().session.closed();
        r.renderer_event(id, BrowserEvent::Unresponsive);
        assert_eq!(sinks.events.lock().unwrap().len(), 1);
    }

    struct AuxiliaryMenuTarget {
        lease: rust_cef_core::menu_lease::MenuLease,
    }
    impl rust_cef_core::ports::ContextMenuDeliverySink for AuxiliaryMenuTarget {
        fn capture(
            &self,
        ) -> Result<
            rust_cef_core::menu_lease::MenuLeaseStamp,
            rust_cef_core::ports::MenuDeliveryError,
        > {
            self.lease
                .capture()
                .map_err(rust_cef_core::ports::MenuDeliveryError::Lease)
        }
        fn publish(
            &self,
            _: rust_cef_core::menu_context::MenuPresentation,
            _: Arc<dyn rust_cef_core::ports::ContextMenuResponseSink>,
            _: rust_cef_core::menu_lease::MenuLeaseStamp,
        ) -> Result<(), rust_cef_core::ports::MenuDeliveryError> {
            Ok(())
        }
        fn dismiss(
            &self,
            _: rust_cef_core::menu::ContextMenuRequestId,
        ) -> Result<(), rust_cef_core::ports::MenuDeliveryError> {
            Ok(())
        }
    }
    fn auxiliary_menu_scope_fixture() -> (
        Arc<UiRegistry>,
        rust_cef_core::menu_lease::MenuLeaseOwner,
        Arc<crate::menu_bridge::Binding>,
        crate::menu_bridge::Scope,
    ) {
        use rust_cef_core::{
            menu::MenuLimits,
            menu_context::MenuContextLimits,
            menu_lease::MenuLeaseOwner,
            ports::{ContextMenuTarget, WindowBinding},
        };
        let nz = |v| std::num::NonZeroUsize::new(v).unwrap();
        let (r, _) = registry();
        let id = BrowserId::new(RuntimeGeneration::new(1), 77);
        let mut c = config();
        c.profile = Some(rust_cef_core::contracts::ProfileHandle::new(
            RuntimeGeneration::new(1),
            7,
        ));
        r.begin_create(id, c).unwrap();
        r.lock()
            .browsers
            .get_mut(&id)
            .unwrap()
            .session
            .after_created();
        r.devtools.configure(nz(1)).unwrap();
        r.configure_context_menus(crate::ContextMenuConfig {
            capacity: nz(1),
            model_limits: MenuLimits {
                max_entries: nz(8),
                max_label_bytes: nz(64),
                max_total_label_bytes: nz(256),
            },
            context_limits: MenuContextLimits {
                max_frame_id_bytes: nz(64),
                max_text_bytes: nz(64),
                max_suggestions: nz(5),
                max_total_bytes: nz(256),
            },
        })
        .unwrap();
        let owner = MenuLeaseOwner::new(
            WindowBinding {
                browser: id,
                window: 9,
            },
            nz(1),
        )
        .unwrap();
        let lease = owner.lease();
        let target = ContextMenuTarget {
            lease: lease.clone(),
            delivery: Arc::new(AuxiliaryMenuTarget {
                lease: lease.clone(),
            }),
        };
        let registration = r.bind_context_menu(target).unwrap();
        let b = registration.binding.clone();
        let stamp = lease.capture().unwrap();
        let context = Arc::new(rust_cef_core::menu_context::MenuContext {
            binding: stamp.binding(),
            binding_revision: stamp.revision(),
            x: 17,
            y: 23,
            frame_id: "synthetic-original-frame".into(),
            selection: String::new(),
            misspelled_word: String::new(),
            dictionary_suggestions: Vec::new(),
        });
        let scope = crate::menu_bridge::Scope {
            browser: id,
            native_id: 101,
            frame_id: "synthetic-original-frame".into(),
            context,
            lease,
            stamp,
            registry: Arc::downgrade(&r),
            registration: Arc::downgrade(&b),
            serial: b.serial,
            epoch: b.epoch.load(Ordering::Acquire),
            spelling_opening: b.spelling.opening(),
            delivery: Arc::downgrade(&b.target.delivery),
        };
        (r, owner, b, scope)
    }
    #[test]
    fn auxiliary_menu_reservation_uses_original_scope_and_exact_preissue_cleanup() {
        let (r, owner, b, scope) = auxiliary_menu_scope_fixture();
        let menu = rust_cef_core::menu::ContextMenuRequestId {
            browser: scope.browser,
            sequence: 5,
        };
        assert!(scope.admission());
        let permit = scope.lease.try_enter(&scope.stamp).unwrap();
        let id = r.reserve_auxiliary_menu(&scope, menu).unwrap();
        assert_eq!(id.owner, scope.browser);
        assert_eq!(id.serial, b.serial);
        assert_eq!(r.devtools.pending(), 1);
        assert_eq!(
            r.reserve_auxiliary_menu(&scope, menu),
            Err(EngineError::Backpressure)
        );
        r.devtools.abandon_preissue(id).unwrap();
        assert_eq!(r.devtools.pending(), 0);
        assert_eq!(owner.lease().snapshot().unwrap().in_flight, 1);
        drop(permit);
        assert_eq!(owner.lease().snapshot().unwrap().in_flight, 0);
        let next = r.reserve_auxiliary_menu(&scope, menu).unwrap();
        assert_ne!(id, next);
        assert_eq!(
            r.devtools.abandon_preissue(id),
            Err(EngineError::InvalidRequest)
        );
        assert_eq!(r.devtools.pending(), 1);
        r.devtools.abandon_preissue(next).unwrap();
    }
    #[test]
    fn auxiliary_menu_reservation_rejects_foreign_stamp_epoch_registry_and_draining() {
        use rust_cef_core::{menu::ContextMenuRequestId, menu_lease::MenuLeaseOwner};
        let (r, mut owner, b, mut scope) = auxiliary_menu_scope_fixture();
        let menu = ContextMenuRequestId {
            browser: scope.browser,
            sequence: 5,
        };
        let (foreign, _) = registry();
        assert_eq!(
            foreign.reserve_auxiliary_menu(&scope, menu),
            Err(EngineError::WrongRuntime)
        );
        let imposter = MenuLeaseOwner::new(
            scope.stamp.binding(),
            std::num::NonZeroUsize::new(1).unwrap(),
        )
        .unwrap();
        let original = scope.stamp.clone();
        scope.stamp = imposter.lease().capture().unwrap();
        assert_eq!(
            r.reserve_auxiliary_menu(&scope, menu),
            Err(EngineError::InvalidRequest)
        );
        scope.stamp = original;
        b.advance();
        assert_eq!(
            r.reserve_auxiliary_menu(&scope, menu),
            Err(EngineError::InvalidRequest)
        );
        scope.epoch = b.epoch.load(Ordering::Acquire);
        owner.invalidate().unwrap();
        assert_eq!(
            r.reserve_auxiliary_menu(&scope, menu),
            Err(EngineError::InvalidRequest)
        );
        scope.stamp = scope.lease.capture().unwrap();
        Arc::make_mut(&mut scope.context).binding_revision = scope.stamp.revision();
        r.lock().draining = true;
        assert_eq!(
            r.reserve_auxiliary_menu(&scope, menu),
            Err(EngineError::Closing)
        );
        assert_eq!(r.devtools.pending(), 0);
    }
    #[test]
    fn auxiliary_preparation_rechecks_frame_owner_and_revoked_epoch_before_attach() {
        let (r, _owner, binding, mut scope) = auxiliary_menu_scope_fixture();
        let menu = rust_cef_core::menu::ContextMenuRequestId {
            browser: scope.browser,
            sequence: 5,
        };
        let permit = scope.lease.try_enter(&scope.stamp).unwrap();
        let id = r.reserve_auxiliary_menu(&scope, menu).unwrap();
        assert_eq!(r.preflight_auxiliary_menu_preparation(&scope, id), Ok(()));

        scope.native_id += 1;
        assert_eq!(
            r.attach_auxiliary_menu_context(&scope, id),
            Err(EngineError::InvalidRequest)
        );
        scope.native_id -= 1;
        scope.frame_id.push('x');
        assert_eq!(
            r.attach_auxiliary_menu_context(&scope, id),
            Err(EngineError::InvalidRequest)
        );
        scope.frame_id.pop();
        assert_eq!(r.preflight_auxiliary_menu_preparation(&scope, id), Ok(()));
        // Model a navigation/reentrant getter between initial preflight and
        // the caller's return with newly-owned native values.
        binding.advance();
        assert_eq!(
            r.attach_auxiliary_menu_context(&scope, id),
            Err(EngineError::InvalidRequest)
        );
        assert_eq!(
            r.attach_auxiliary_menu_client(&scope, id),
            Err(EngineError::InvalidRequest)
        );
        r.devtools.abandon_preissue(id).unwrap();
        assert_eq!(
            r.devtools.pending(),
            0,
            "no unqualified native owners were counted"
        );
        drop(permit);
    }

    #[test]
    fn auxiliary_preparation_refuses_after_close_intent_even_if_menu_lease_is_current() {
        let (r, _owner, _binding, scope) = auxiliary_menu_scope_fixture();
        let menu = rust_cef_core::menu::ContextMenuRequestId {
            browser: scope.browser,
            sequence: 5,
        };
        let _permit = scope.lease.try_enter(&scope.stamp).unwrap();
        let id = r.reserve_auxiliary_menu(&scope, menu).unwrap();
        assert_eq!(r.preflight_auxiliary_menu_preparation(&scope, id), Ok(()));
        r.note_close_intent(scope.browser, CloseMode::Force);
        assert!(scope.current());
        assert_eq!(
            r.preflight_auxiliary_menu_preparation(&scope, id),
            Err(EngineError::Closing)
        );
        assert_eq!(
            r.attach_auxiliary_menu_context(&scope, id),
            Err(EngineError::Closing)
        );
        assert_eq!(
            r.attach_auxiliary_menu_client(&scope, id),
            Err(EngineError::Closing)
        );
        assert_eq!(r.devtools.pending(), 1);
        r.devtools.abandon_preissue(id).unwrap();
        assert_eq!(r.devtools.pending(), 0);
    }

    fn registry() -> (Arc<UiRegistry>, Arc<Sinks>) {
        let sinks = Arc::new(Sinks::default());
        let registry = UiRegistry::new(
            RuntimeGeneration::new(1),
            sinks.clone(),
            sinks.clone(),
            sinks.clone(),
            NativeAudit::new(),
        );
        (registry, sinks)
    }

    fn config() -> BrowserConfig {
        BrowserConfig {
            initial_url: "data:text/html,fixture".into(),
            profile: None,
            render_preference: RenderPreference::SoftwareRequired,
            viewport: rust_cef_core::contracts::Viewport {
                width: 320,
                height: 240,
                scale: 1.0,
            },
            options: Default::default(),
        }
    }

    fn registry_with_posting(
        accept: bool,
    ) -> (
        Arc<UiRegistry>,
        Arc<Sinks>,
        Arc<StdMutex<Vec<TaskPayload>>>,
        Arc<Executor>,
    ) {
        let (registry, sinks) = registry();
        let executor = Executor::new(registry.runtime_generation(), &registry, NativeAudit::new());
        registry.attach_executor(&executor);
        let posted = Arc::new(StdMutex::new(Vec::new()));
        let observed = posted.clone();
        executor.set_test_post(Arc::new(move |payload| {
            if accept {
                observed.lock().unwrap().push(payload.clone());
            }
            accept
        }));
        (registry, sinks, posted, executor)
    }

    fn alive(registry: &UiRegistry, id: BrowserId) {
        registry.begin_create(id, config()).unwrap();
        registry
            .lock()
            .browsers
            .get_mut(&id)
            .unwrap()
            .session
            .after_created();
        registry.mark_created_reported(id);
    }

    #[test]
    fn unready_or_terminal_reset_does_not_poison_input_cleanup() {
        let (registry, _, posted, executor) = registry_with_posting(true);
        let browser = BrowserId::new(RuntimeGeneration::new(1), 72);
        registry.begin_create(browser, config()).unwrap();
        assert_eq!(
            registry.submit_command(
                browser,
                &executor,
                BrowserCommand::ResetInput(InputResetReason::FocusLost)
            ),
            Err(EngineError::NotReady)
        );
        let before = registry.input_snapshot(browser).unwrap();
        assert!(!before.cleanup_pending && !before.cleanup_failed);
        assert_eq!(before.last_native_error, None);
        assert_eq!(before.epoch, 1);
        assert!(posted.lock().unwrap().is_empty());
        assert!(!registry.input_cleanup_pending());
        registry
            .lock()
            .browsers
            .get_mut(&browser)
            .unwrap()
            .session
            .after_created();
        let ready = registry.input_snapshot(browser).unwrap();
        assert!(!ready.cleanup_failed && ready.last_native_error.is_none());
        registry
            .lock()
            .browsers
            .get_mut(&browser)
            .unwrap()
            .session
            .closed();
        assert_eq!(
            registry.submit_command(
                browser,
                &executor,
                BrowserCommand::ResetInput(InputResetReason::FocusLost)
            ),
            Err(EngineError::Closed)
        );
        assert!(posted.lock().unwrap().is_empty());
        assert!(!registry.input_cleanup_pending());
    }

    #[test]
    fn input_post_rejection_requests_one_durable_cleanup_slot() {
        use rust_cef_core::input::{InputEvent, InputKind};
        let (registry, _, _, executor) = registry_with_posting(false);
        let browser = BrowserId::new(RuntimeGeneration::new(1), 73);
        alive(&registry, browser);
        let error = registry.submit_command(
            browser,
            &executor,
            BrowserCommand::Input(InputEvent {
                epoch: 1,
                kind: InputKind::Text("hi".into()),
            }),
        );
        assert_eq!(error, Err(EngineError::PostRejected));
        let snapshot = registry.input_snapshot(browser).unwrap();
        assert_eq!(snapshot.admitted, 1);
        assert_eq!(snapshot.rejected, 1);
        assert_eq!(snapshot.inflight, 0);
        assert!(snapshot.cleanup_pending && snapshot.cleanup_failed);
        assert_eq!(snapshot.epoch, 2);
        assert!(registry.input_cleanup_pending());
        assert!(!executor.try_seal_if_idle());
        let posted = Arc::new(StdMutex::new(Vec::<TaskPayload>::new()));
        let captured = posted.clone();
        executor.set_test_post(Arc::new(move |payload| {
            captured.lock().unwrap().push(payload.clone());
            true
        }));
        let first = registry
            .submit_command(
                browser,
                &executor,
                BrowserCommand::ResetInput(InputResetReason::Rejected),
            )
            .unwrap();
        let second = registry
            .submit_command(
                browser,
                &executor,
                BrowserCommand::ResetInput(InputResetReason::FocusLost),
            )
            .unwrap();
        assert_eq!(first, second);
        assert!(
            matches!(posted.lock().unwrap().as_slice(), [TaskPayload::InputReset(id)] if *id == first)
        );
        assert!(registry.input_snapshot(browser).unwrap().cleanup_pending);
    }

    #[test]
    fn full_normal_lane_keeps_input_epoch_for_explicit_host_retry() {
        use rust_cef_core::input::{InputEvent, InputKind};
        let (registry, _, _, executor) = registry_with_posting(true);
        let browser = BrowserId::new(RuntimeGeneration::new(1), 76);
        alive(&registry, browser);
        for _ in 0..rust_cef_core::session::COMMAND_CAPACITY {
            registry
                .submit_command(browser, &executor, BrowserCommand::Back)
                .unwrap();
        }
        assert_eq!(
            registry.submit_command(
                browser,
                &executor,
                BrowserCommand::Input(InputEvent {
                    epoch: 1,
                    kind: InputKind::Text("z".into()),
                })
            ),
            Err(EngineError::Backpressure)
        );
        let snapshot = registry.input_snapshot(browser).unwrap();
        assert_eq!(snapshot.epoch, 1);
        assert!(!snapshot.cleanup_pending);
        assert_eq!(snapshot.rejected, 1);
    }

    #[test]
    fn checked_out_native_ledger_keeps_admission_counters_and_pressed_snapshot() {
        use rust_cef_core::input::{InputEvent, InputKind};
        let (registry, _, _, executor) = registry_with_posting(true);
        let browser = BrowserId::new(RuntimeGeneration::new(1), 74);
        alive(&registry, browser);
        let mut ledger = {
            let mut state = registry.lock();
            let entry = state.browsers.get_mut(&browser).unwrap();
            let mut ledger = entry.input.take().unwrap();
            ledger.admitted = 2;
            entry.input_shadow = ledger.clone();
            ledger
        };
        assert!(
            registry
                .input_snapshot(browser)
                .unwrap()
                .native_execution_unconfirmed
        );
        registry
            .submit_command(
                browser,
                &executor,
                BrowserCommand::Input(InputEvent {
                    epoch: 1,
                    kind: InputKind::Text("a".into()),
                }),
            )
            .unwrap();
        assert_eq!(registry.input_snapshot(browser).unwrap().admitted, 3);
        // The same production restore merges concurrently accepted counters.
        let mut state = registry.lock();
        let entry = state.browsers.get_mut(&browser).unwrap();
        ledger.executed = 1;
        entry.restore_input(ledger);
        drop(state);
        let snapshot = registry.input_snapshot(browser).unwrap();
        assert_eq!((snapshot.admitted, snapshot.executed), (3, 1));
        assert!(!snapshot.native_execution_unconfirmed);
        assert_eq!(snapshot.inflight, 1);
    }

    #[test]
    fn cleanup_cutpoint_waits_for_legacy_click_and_new_input() {
        use rust_cef_core::input::{InputEvent, InputKind};
        let (registry, _, _, executor) = registry_with_posting(true);
        let browser = BrowserId::new(RuntimeGeneration::new(1), 77);
        alive(&registry, browser);
        registry
            .submit_command(
                browser,
                &executor,
                BrowserCommand::MouseClick(MouseClick {
                    x: 1,
                    y: 2,
                    modifiers: Default::default(),
                    click_count: std::num::NonZeroU8::new(1).unwrap(),
                }),
            )
            .unwrap();
        registry
            .submit_command(
                browser,
                &executor,
                BrowserCommand::Input(InputEvent {
                    epoch: 1,
                    kind: InputKind::Text("x".into()),
                }),
            )
            .unwrap();
        let reset = registry
            .submit_command(
                browser,
                &executor,
                BrowserCommand::ResetInput(InputResetReason::FocusLost),
            )
            .unwrap();
        registry.run_input_reset(reset);
        let state = registry.lock();
        let entry = &state.browsers[&browser];
        assert!(entry.input_reset_ready);
        assert_eq!(entry.input_outstanding.len(), 2);
        assert!(entry.session.input_reset_pending());
        assert!(!entry.input_shadow.cleanup_failed);
        drop(state);
        assert_eq!(
            registry.submit_command(
                browser,
                &executor,
                BrowserCommand::MouseClick(MouseClick {
                    x: 1,
                    y: 2,
                    modifiers: Default::default(),
                    click_count: std::num::NonZeroU8::new(1).unwrap(),
                })
            ),
            Err(EngineError::Closing)
        );
        assert_eq!(
            registry.submit_command(browser, &executor, BrowserCommand::SetFocus(true)),
            Err(EngineError::Closing)
        );
        assert_eq!(registry.input_snapshot(browser).unwrap().rejected, 2);
    }

    #[test]
    fn out_of_order_input_task_waits_for_earlier_native_execution() {
        use rust_cef_core::input::{InputEvent, InputKind};
        let (registry, _, posted, executor) = registry_with_posting(true);
        let browser = BrowserId::new(RuntimeGeneration::new(1), 75);
        alive(&registry, browser);
        for _ in 0..2 {
            registry
                .submit_command(
                    browser,
                    &executor,
                    BrowserCommand::Input(InputEvent {
                        epoch: 1,
                        kind: InputKind::Text("x".into()),
                    }),
                )
                .unwrap();
        }
        let second = match &posted.lock().unwrap()[1] {
            TaskPayload::Command(command) => command.clone(),
            _ => unreachable!(),
        };
        registry.run_command(second.clone());
        registry.run_command(second);
        let state = registry.lock();
        let entry = &state.browsers[&browser];
        assert_eq!(entry.input_deferred.len(), 1);
        assert_eq!(entry.input_outstanding.len(), 2);
        assert_eq!(entry.session.pending_commands(), 2);
    }

    #[test]
    fn force_upgrades_a_graceful_close_intent() {
        let (registry, _, _, _executor) = registry_with_posting(true);
        let id = BrowserId::new(RuntimeGeneration::new(1), 10);
        alive(&registry, id);
        registry.note_close_intent(id, CloseMode::Graceful);
        assert_eq!(
            registry.lock().browsers[&id].close_mode,
            Some(CloseMode::Graceful)
        );
        registry.note_close_intent(id, CloseMode::Force);
        assert_eq!(
            registry.lock().browsers[&id].close_mode,
            Some(CloseMode::Force),
            "a host Force escalation must not be discarded by the first intent"
        );
    }

    #[test]
    fn deferred_close_is_not_consumed_by_close_sent() {
        let (registry, _, posted, _executor) = registry_with_posting(true);
        let id = BrowserId::new(RuntimeGeneration::new(1), 11);
        alive(&registry, id);
        registry.note_close_intent(id, CloseMode::Graceful);
        registry.set_close_deferred(id, true).unwrap();
        // The queued close task observes the deferred flag and must keep the
        // tombstone without consuming close_sent.
        registry.close_now(id, CloseMode::Graceful);
        {
            let state = registry.lock();
            let entry = &state.browsers[&id];
            assert!(entry.close_deferred);
            assert!(!entry.close_sent);
            assert_eq!(entry.session.snapshot().state, SessionState::Closing);
        }
        assert_eq!(registry.open_browsers(), vec![id]);
        registry.set_close_deferred(id, false).unwrap();
        assert_eq!(posted.lock().unwrap().len(), 1);
        assert!(matches!(
            posted.lock().unwrap().first(),
            Some(TaskPayload::Close { .. })
        ));
        registry.set_close_deferred(id, false).unwrap();
        assert_eq!(
            posted.lock().unwrap().len(),
            1,
            "clearing the deferral must schedule one release, not one per call"
        );
        assert!(!registry.lock().browsers[&id].close_deferred);
    }

    #[test]
    fn pending_request_is_cancelled_once_outside_the_lock() {
        let (registry, _, _, _executor) = registry_with_posting(true);
        let id = BrowserId::new(RuntimeGeneration::new(1), 12);
        alive(&registry, id);
        let cancelled = Arc::new(AtomicU64::new(0));
        let callback = cancelled.clone();
        let request = registry
            .inject_request(id, move || {
                callback.fetch_add(1, Ordering::AcqRel);
            })
            .unwrap();
        assert_eq!(request.browser, id);
        assert_eq!(registry.pending_requests(), 1);
        registry.set_draining();
        assert!(matches!(
            registry.inject_request(id, || {}),
            Err(EngineError::Closing)
        ));
        assert_eq!(registry.cancelled_requests(), 0);
        registry.cancel_all_requests(true);
        assert_eq!(cancelled.load(Ordering::Acquire), 1);
        assert_eq!(registry.pending_requests(), 0);
        assert_eq!(registry.cancelled_requests(), 1);
        registry.cancel_all_requests(true);
        assert_eq!(
            cancelled.load(Ordering::Acquire),
            1,
            "a drained continuation must not be cancelled twice"
        );
    }

    #[test]
    fn rejected_drain_post_keeps_requests_pending() {
        let (registry, _, _, _executor) = registry_with_posting(false);
        let id = BrowserId::new(RuntimeGeneration::new(1), 13);
        alive(&registry, id);
        registry.inject_request(id, || {}).unwrap();
        registry.set_draining();
        assert_eq!(
            registry.post_drain_requests().unwrap_err(),
            EngineError::PostRejected
        );
        assert_eq!(registry.pending_requests(), 1);
        assert_eq!(registry.cancelled_requests(), 0);
    }

    #[test]
    fn before_close_cancels_only_its_browser_requests_once() {
        let (registry, _, _, _executor) = registry_with_posting(true);
        let first = BrowserId::new(RuntimeGeneration::new(1), 14);
        let second = BrowserId::new(RuntimeGeneration::new(1), 15);
        alive(&registry, first);
        alive(&registry, second);
        registry.inject_request(first, || {}).unwrap();
        registry.inject_request(second, || {}).unwrap();
        registry.before_close(first);
        assert_eq!(registry.pending_requests(), 1);
        assert_eq!(registry.cancelled_requests(), 1);
        registry.before_close(first);
        assert_eq!(registry.pending_requests(), 1);
        assert_eq!(registry.cancelled_requests(), 1);
        registry.before_close(second);
        assert_eq!(registry.pending_requests(), 0);
        assert_eq!(registry.cancelled_requests(), 2);
    }

    #[test]
    fn two_browsers_one_closed_is_not_ready() {
        let (registry, _, _, _executor) = registry_with_posting(true);
        let first = BrowserId::new(RuntimeGeneration::new(1), 16);
        let second = BrowserId::new(RuntimeGeneration::new(1), 17);
        alive(&registry, first);
        alive(&registry, second);
        let now = std::time::Instant::now();
        let mut barrier = rust_cef_core::runtime::ShutdownBarrier::new(
            rust_cef_core::contracts::InitializeOutcome::Accepted,
        );
        barrier
            .request(now + std::time::Duration::from_secs(1))
            .unwrap();
        registry.note_close_intent(first, CloseMode::Force);
        registry.before_close(first);
        let observed = barrier.observe(&registry.snapshots(), 0, registry.pending_requests(), now);
        assert!(matches!(
            observed,
            rust_cef_core::runtime::ShutdownDecision::Draining { ref open_browsers, .. }
                if open_browsers == &[second]
        ));
        assert!(!barrier.may_finish());
        registry.before_close(second);
        assert_eq!(
            barrier.observe(&registry.snapshots(), 0, registry.pending_requests(), now),
            rust_cef_core::runtime::ShutdownDecision::Ready
        );
        assert!(barrier.may_finish());
    }

    #[test]
    fn creating_instance_keeps_the_barrier_until_terminal() {
        let (registry, _, _, _executor) = registry_with_posting(true);
        let id = BrowserId::new(RuntimeGeneration::new(1), 18);
        registry.begin_create(id, config()).unwrap();
        let now = std::time::Instant::now();
        let mut barrier = rust_cef_core::runtime::ShutdownBarrier::new(
            rust_cef_core::contracts::InitializeOutcome::Accepted,
        );
        barrier
            .request(now + std::time::Duration::from_secs(1))
            .unwrap();
        registry.note_close_intent(id, CloseMode::Force);
        assert_eq!(registry.snapshot(id).state, SessionState::Creating);
        assert!(matches!(
            barrier.observe(&registry.snapshots(), 0, 0, now),
            rust_cef_core::runtime::ShutdownDecision::Draining { ref open_browsers, .. }
                if open_browsers == &[id]
        ));
        assert!(!barrier.may_finish());
        // The queued Create rejects itself during drainage; the tombstone is
        // terminal only then, never merely because close was requested.
        registry.reject_create(id, EngineError::Closing);
        assert_eq!(
            barrier.observe(&registry.snapshots(), 0, 0, now),
            rust_cef_core::runtime::ShutdownDecision::Ready
        );
    }

    #[test]
    fn close_post_failure_does_not_make_the_instance_terminal() {
        let (registry, _, _, _executor) = registry_with_posting(false);
        let id = BrowserId::new(RuntimeGeneration::new(1), 19);
        alive(&registry, id);
        registry.close_browser(id, CloseMode::Graceful);
        assert_eq!(registry.snapshot(id).state, SessionState::Closing);
        assert!(!registry.all_terminal());
        assert_eq!(registry.open_browsers(), vec![id]);
        let now = std::time::Instant::now();
        let mut barrier = rust_cef_core::runtime::ShutdownBarrier::new(
            rust_cef_core::contracts::InitializeOutcome::Accepted,
        );
        barrier
            .request(now + std::time::Duration::from_secs(1))
            .unwrap();
        assert!(matches!(
            barrier.observe(&registry.snapshots(), 0, 0, now),
            rust_cef_core::runtime::ShutdownDecision::Draining { .. }
        ));
        assert!(!barrier.may_finish());
    }

    #[test]
    fn closing_an_unissued_creation_is_terminal() {
        let (registry, _, _, _executor) = registry_with_posting(true);
        let id = BrowserId::new(RuntimeGeneration::new(1), 29);
        registry.begin_create(id, config()).unwrap();
        registry.close_now(id, CloseMode::Force);
        // No native browser was ever issued, so there is nothing to wait for:
        // the instance must reach a terminal state, not stay Closing forever.
        assert_eq!(registry.snapshot(id).state, SessionState::CreateFailed);
        assert!(registry.all_terminal());
        assert_eq!(registry.open_browsers(), Vec::<BrowserId>::new());
    }

    #[test]
    fn before_close_stays_closing_until_callbacks_return() {
        let (registry, sinks) = registry();
        let id = BrowserId::new(RuntimeGeneration::new(1), 30);
        alive(&registry, id);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        registry
            .inject_request(id, move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
            .unwrap();
        let worker_registry = registry.clone();
        let worker = std::thread::spawn(move || worker_registry.before_close(id));
        started_rx.recv().unwrap();
        // The cancel callback is deliberately still running. The instance must
        // not be reported terminal, and the native-reference predicate must not
        // claim cleanup finished.
        assert_eq!(registry.snapshot(id).state, SessionState::Closing);
        assert!(!registry.native_refs_clear());
        let now = std::time::Instant::now();
        let mut barrier = rust_cef_core::runtime::ShutdownBarrier::new(
            rust_cef_core::contracts::InitializeOutcome::Accepted,
        );
        barrier
            .request(now + std::time::Duration::from_secs(1))
            .unwrap();
        assert!(matches!(
            barrier.observe(&registry.snapshots(), 0, 0, now),
            rust_cef_core::runtime::ShutdownDecision::Draining { .. }
        ));
        assert!(!barrier.may_finish());
        // A duplicate notification while cleanup is pinned must be ignored.
        registry.before_close(id);
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(registry.snapshot(id).state, SessionState::Closed);
        assert!(registry.native_refs_clear());
        assert_eq!(registry.pending_requests(), 0);
        assert_eq!(registry.cancelled_requests(), 1);
        assert_eq!(
            sinks
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| **event == BrowserEvent::Closed)
                .count(),
            1
        );
        assert_eq!(
            barrier.observe(&registry.snapshots(), 0, 0, now),
            rust_cef_core::runtime::ShutdownDecision::Ready
        );
    }

    #[test]
    fn repeated_before_close_reports_one_closed_event() {
        let (registry, sinks) = registry();
        let id = BrowserId::new(RuntimeGeneration::new(1), 20);
        alive(&registry, id);
        registry.before_close(id);
        registry.before_close(id);
        assert_eq!(registry.snapshot(id).state, SessionState::Closed);
        let events = sinks.events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| **event == BrowserEvent::Closed)
                .count(),
            1
        );
    }

    #[test]
    fn repeated_close_posts_one_graceful_task_and_one_force_upgrade() {
        let (registry, _, posted, _executor) = registry_with_posting(true);
        let id = BrowserId::new(RuntimeGeneration::new(1), 27);
        alive(&registry, id);
        registry.close_browser(id, CloseMode::Graceful);
        registry.close_browser(id, CloseMode::Graceful);
        assert_eq!(
            posted.lock().unwrap().len(),
            1,
            "a repeated graceful close must not schedule duplicate work"
        );
        // A host Force escalation must reach the instance exactly once.
        registry.close_browser(id, CloseMode::Force);
        registry.close_browser(id, CloseMode::Force);
        let tasks = posted.lock().unwrap();
        assert_eq!(tasks.len(), 2);
        assert!(matches!(
            tasks[0],
            TaskPayload::Close {
                mode: CloseMode::Graceful,
                ..
            }
        ));
        assert!(matches!(
            tasks[1],
            TaskPayload::Close {
                mode: CloseMode::Force,
                ..
            }
        ));
        drop(tasks);
        let entry = &registry.lock().browsers[&id];
        assert_eq!(entry.close_mode, Some(CloseMode::Force));
        assert_eq!(entry.close_posted, Some(CloseMode::Force));
    }

    #[test]
    fn double_close_delivers_one_closed_event() {
        let (registry, sinks) = registry();
        let id = BrowserId::new(RuntimeGeneration::new(1), 28);
        alive(&registry, id);
        // A second close intent after the first must be idempotent; the
        // terminal callback remains the only Closed source.
        registry.note_close_intent(id, CloseMode::Graceful);
        registry.note_close_intent(id, CloseMode::Graceful);
        registry.before_close(id);
        registry.before_close(id);
        let events = sinks.events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| **event == BrowserEvent::Closed)
                .count(),
            1
        );
        assert_eq!(registry.open_browsers(), Vec::<BrowserId>::new());
    }

    #[test]
    fn pending_requests_keep_the_barrier_from_ready_after_all_browsers_close() {
        let (registry, _, _, _executor) = registry_with_posting(true);
        let id = BrowserId::new(RuntimeGeneration::new(1), 23);
        alive(&registry, id);
        registry.inject_request(id, || {}).unwrap();
        registry.note_close_intent(id, CloseMode::Graceful);
        registry.before_close(id);
        // OnBeforeClose terminates that browser's continuation only; an
        // unrelated pending request must still block the ready decision.
        let other = BrowserId::new(RuntimeGeneration::new(1), 24);
        assert_eq!(registry.pending_requests(), 0);
        alive(&registry, other);
        registry.inject_request(other, || {}).unwrap();
        registry.before_close(other);
        assert_eq!(registry.pending_requests(), 0);
        let now = std::time::Instant::now();
        let mut barrier = rust_cef_core::runtime::ShutdownBarrier::new(
            rust_cef_core::contracts::InitializeOutcome::Accepted,
        );
        barrier
            .request(now + std::time::Duration::from_secs(1))
            .unwrap();
        assert_eq!(
            barrier.observe(&registry.snapshots(), 0, 1, now),
            rust_cef_core::runtime::ShutdownDecision::Draining {
                open_browsers: vec![],
                pending_tasks: 0,
                pending_requests: 1,
            }
        );
        assert!(!barrier.may_finish());
        assert_eq!(
            barrier.observe(&registry.snapshots(), 0, 0, now),
            rust_cef_core::runtime::ShutdownDecision::Ready
        );
    }

    #[test]
    fn drain_request_scheduling_uses_the_private_payload() {
        let (registry, _, posted, _executor) = registry_with_posting(true);
        let id = BrowserId::new(RuntimeGeneration::new(1), 25);
        alive(&registry, id);
        registry.inject_request(id, || {}).unwrap();
        registry.set_draining();
        registry.post_drain_requests().unwrap();
        assert!(matches!(
            posted.lock().unwrap().as_slice(),
            [TaskPayload::DrainRequests]
        ));
        assert_eq!(
            registry.pending_requests(),
            1,
            "scheduling alone must not pretend the continuation was cancelled"
        );
    }

    #[test]
    fn deferred_close_cannot_be_set_after_close_was_already_dispatched() {
        let (registry, _, _, _executor) = registry_with_posting(true);
        let id = BrowserId::new(RuntimeGeneration::new(1), 26);
        registry.begin_create(id, config()).unwrap();
        // No native browser was issued, so this close retires the tombstone and
        // consumes close_sent. A later deferral would silently undo the intent.
        registry.close_now(id, CloseMode::Force);
        assert!(registry.lock().browsers[&id].close_sent);
        assert_eq!(
            registry.set_close_deferred(id, true).unwrap_err(),
            EngineError::Closing
        );
        assert!(registry.lock().browsers[&id].close_deferred == false);
    }

    #[test]
    fn native_refs_are_clear_after_terminal_instances() {
        let (registry, _, _, _executor) = registry_with_posting(true);
        let closed = BrowserId::new(RuntimeGeneration::new(1), 21);
        alive(&registry, closed);
        registry.before_close(closed);
        let failed = BrowserId::new(RuntimeGeneration::new(1), 22);
        registry.begin_create(failed, config()).unwrap();
        registry.reject_create(failed, EngineError::Closing);
        assert!(registry.all_terminal());
        assert!(registry.native_refs_clear());
    }

    #[test]
    fn acceptance_is_not_alive_and_failure_is_terminal() {
        let (registry, sinks) = registry();
        let id = BrowserId::new(RuntimeGeneration::new(1), 1);
        registry.begin_create(id, config()).unwrap();
        assert_eq!(registry.snapshot(id).state, SessionState::Creating);
        assert!(!registry.snapshot(id).is_ready());
        registry.reject_create(id, EngineError::PostRejected);
        assert_eq!(registry.snapshot(id).state, SessionState::CreateFailed);
        assert!(registry.all_terminal());
        assert_eq!(
            sinks.events.lock().unwrap().last(),
            Some(&BrowserEvent::CreateFailed(EngineError::PostRejected))
        );
    }

    #[test]
    fn close_intent_keeps_creating_instance_inside_the_barrier() {
        let (registry, _) = registry();
        let id = BrowserId::new(RuntimeGeneration::new(1), 2);
        registry.begin_create(id, config()).unwrap();
        assert!(!registry.all_terminal());
        assert_eq!(registry.open_browsers(), vec![id]);
        registry.note_close_intent(id, CloseMode::Force);
        // Still Creating: the queued Create rejects itself during drainage and
        // a native OnAfterCreated would observe the close intent.
        assert_eq!(registry.snapshot(id).state, SessionState::Creating);
        assert!(!registry.snapshot(id).creation_accepted);
        registry.before_close(id);
        assert_eq!(registry.snapshot(id).state, SessionState::Closed);
        assert!(registry.all_terminal());
    }

    #[test]
    fn policy_denials_coalesce_and_never_publish_after_closed() {
        let (registry, sinks, posted, _executor) = registry_with_posting(true);
        let id = BrowserId::new(registry.runtime_generation(), 59);
        alive(&registry, id);
        for _ in 0..1000 {
            registry.note_policy_denial(id, PolicyAction::Resource);
        }
        assert_eq!(posted.lock().unwrap().len(), 1);
        registry.before_close(id);
        let count = sinks.events.lock().unwrap().len();
        registry.flush_policy_events(id);
        assert_eq!(sinks.events.lock().unwrap().len(), count);
        assert!(
            !sinks
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, BrowserEvent::PolicyDenied { .. }))
        );
    }

    #[test]
    fn before_unload_reply_has_its_own_lane_and_preserves_the_cancelled_ticket() {
        let (registry, _, posted, executor) = registry_with_posting(true);
        let id = BrowserId::new(registry.runtime_generation(), 60);
        alive(&registry, id);
        let control = RuntimeBrowserControl::new(id, executor.clone(), registry.clone());
        for _ in 0..256 {
            control.submit(BrowserCommand::Reload).unwrap();
        }
        let cancelled = control.request_close(CloseMode::Graceful);
        let called = Arc::new(AtomicU64::new(0));
        let (reenter, count) = (registry.clone(), called.clone());
        let request = registry
            .dialogs
            .insert_test(
                id,
                close_prompt_cut_in_test(&registry, id),
                std::time::Instant::now(),
                Box::new(move |allow| {
                    assert!(!allow);
                    assert_eq!(reenter.pending_requests(), 0);
                    assert_eq!(reenter.snapshot(id).state, SessionState::Alive);
                    count.fetch_add(1, Ordering::Relaxed);
                }),
            )
            .ok()
            .unwrap();
        registry.set_draining();
        executor.begin_drain();
        control
            .submit(BrowserCommand::ResolveBeforeUnload {
                request,
                allow: false,
            })
            .unwrap();
        assert_eq!(
            control.submit(BrowserCommand::ResolveBeforeUnload {
                request,
                allow: false
            }),
            Err(EngineError::InvalidRequest)
        );
        assert!(matches!(
            posted.lock().unwrap().last(),
            Some(TaskPayload::Command(_))
        ));
        registry.answer_before_unload(request, false).unwrap();
        assert_eq!(called.load(Ordering::Acquire), 1);
        assert_eq!(cancelled.result(), CloseResult::Cancelled);
        assert_eq!(
            control.submit(BrowserCommand::Reload),
            Err(EngineError::Closing),
            "drain must not reopen"
        );
        let force = control.request_close(CloseMode::Force);
        assert_ne!(force.sequence, cancelled.sequence);
        registry.before_close(id);
        assert_eq!(force.result(), CloseResult::Closed);
        assert_eq!(cancelled.result(), CloseResult::Cancelled);
    }

    #[test]
    fn stale_before_unload_cannot_revoke_or_join_successor_close() {
        for mode in [CloseMode::Graceful, CloseMode::Force] {
            let (registry, sinks, posted, executor) = registry_with_posting(true);
            let browser = BrowserId::new(registry.runtime_generation(), 88);
            alive(&registry, browser);
            let control = RuntimeBrowserControl::new(browser, executor, registry.clone());
            let old = control.request_close(CloseMode::Graceful);
            let old_cut = close_prompt_cut_in_test(&registry, browser).unwrap();
            let answers = Arc::new(AtomicU64::new(0));
            let (seen, r) = (answers.clone(), registry.clone());
            let stale_id = registry
                .dialogs
                .insert_test(
                    browser,
                    Some(old_cut.clone()),
                    std::time::Instant::now(),
                    Box::new(move |allow| {
                        assert!(!allow);
                        assert!(
                            r.state.try_lock().is_ok(),
                            "stale callback answered under Registry lock"
                        );
                        seen.fetch_add(1, Ordering::Relaxed);
                    }),
                )
                .ok()
                .unwrap();
            // The old continuation was taken out of the bounded table before
            // the original close was settled. It still owns its old cut.
            let stale = registry.dialogs.take(stale_id).unwrap();
            cancel_graceful_in_test(&registry, browser);
            assert_eq!(old.result(), CloseResult::Cancelled);

            let successor = control.request_close(mode);
            assert_ne!(successor.sequence, old.sequence);
            if mode == CloseMode::Graceful {
                let fresh = close_prompt_cut_in_test(&registry, browser).unwrap();
                assert!(!old_cut.same_attempt(&fresh));
            }
            let before = posted.lock().unwrap().len();
            registry.finish_dialog(stale, mode == CloseMode::Force);
            assert_eq!(answers.load(Ordering::Acquire), 1);
            assert_eq!(posted.lock().unwrap().len(), before);
            assert_eq!(registry.snapshot(browser).state, SessionState::Closing);
            assert_eq!(successor.result(), CloseResult::Pending);
            {
                let state = registry.lock();
                let entry = &state.browsers[&browser];
                assert_eq!(entry.close_mode, Some(mode));
                assert!(entry.deferred_force_dialog.is_none());
            }
            assert_eq!(
                sinks
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|e| **e == BrowserEvent::CloseCancelled)
                    .count(),
                1
            );
            registry.before_close(browser);
            assert_eq!(old.result(), CloseResult::Cancelled);
            assert_eq!(successor.result(), CloseResult::Closed);
        }
    }

    #[test]
    fn before_unload_on_same_attempt_waits_for_force_return() {
        let (registry, _, _, executor) = registry_with_posting(true);
        let browser = BrowserId::new(registry.runtime_generation(), 89);
        alive(&registry, browser);
        let control = RuntimeBrowserControl::new(browser, executor, registry.clone());
        let original = control.request_close(CloseMode::Graceful);
        let cut = close_prompt_cut_in_test(&registry, browser);
        let answers = Arc::new(AtomicU64::new(0));
        let (seen, reenter) = (answers.clone(), registry.clone());
        let request = registry
            .dialogs
            .insert_test(
                browser,
                cut,
                std::time::Instant::now(),
                Box::new(move |allow| {
                    assert!(!allow);
                    assert!(reenter.state.try_lock().is_ok());
                    seen.fetch_add(1, Ordering::Relaxed);
                }),
            )
            .ok()
            .unwrap();
        let force = control.request_close(CloseMode::Force);
        assert_eq!(original.sequence, force.sequence);
        registry.answer_before_unload(request, false).unwrap();
        assert_eq!(answers.load(Ordering::Acquire), 0);
        assert!(
            registry.lock().browsers[&browser]
                .deferred_force_dialog
                .is_some()
        );
        // This isolated fixture has no native Browser. Inject the return cut;
        // it exercises the shared take/answer path, not a CEF call.
        registry.finish_force_close(browser);
        registry.finish_force_close(browser);
        assert_eq!(answers.load(Ordering::Acquire), 1);
        assert_eq!(original.result(), CloseResult::Pending);
        registry.before_close(browser);
        assert_eq!(original.result(), CloseResult::Closed);
    }

    #[test]
    fn before_unload_held_force_callback_retires_on_terminal_or_abandon() {
        struct DropWitness(Weak<UiRegistry>, Arc<AtomicU64>);
        impl Drop for DropWitness {
            fn drop(&mut self) {
                let registry = self.0.upgrade().unwrap();
                assert!(
                    registry.state.try_lock().is_ok(),
                    "callback dropped under Registry lock"
                );
                self.1.fetch_add(1, Ordering::Relaxed);
            }
        }
        for abandon in [false, true] {
            let (registry, _, _, executor) = registry_with_posting(true);
            let browser = BrowserId::new(registry.runtime_generation(), 90);
            alive(&registry, browser);
            let control = RuntimeBrowserControl::new(browser, executor, registry.clone());
            let close = control.request_close(CloseMode::Graceful);
            let cut = close_prompt_cut_in_test(&registry, browser);
            let dropped = Arc::new(AtomicU64::new(0));
            let witness = DropWitness(Arc::downgrade(&registry), dropped.clone());
            let request = registry
                .dialogs
                .insert_test(
                    browser,
                    cut,
                    std::time::Instant::now(),
                    Box::new(move |_| {
                        drop(witness);
                        panic!(
                            "terminal/abandon must not explicitly answer an unresolved callback"
                        );
                    }),
                )
                .ok()
                .unwrap();
            assert_eq!(
                control.request_close(CloseMode::Force).sequence,
                close.sequence
            );
            registry.answer_before_unload(request, false).unwrap();
            assert_eq!(registry.dialogs.pending(), 0);
            assert_eq!(registry.pending_requests(), 1);
            assert!(!registry.native_refs_clear());
            assert_eq!(dropped.load(Ordering::Acquire), 0);
            if abandon {
                registry.abandon_native_refs();
                assert_eq!(close.result(), CloseResult::Pending);
                assert_eq!(registry.dialog_stats().2, 0);
            } else {
                registry.before_close(browser);
                assert_eq!(close.result(), CloseResult::Closed);
                assert_eq!(registry.dialog_stats().2, 1);
            }
            registry.finish_force_close(browser);
            assert_eq!(registry.pending_requests(), 0);
            assert!(registry.native_refs_clear());
            assert_eq!(dropped.load(Ordering::Acquire), 1);
            assert_eq!(registry.dialog_stats().1, 0);
        }
    }

    #[test]
    fn before_unload_post_failure_rolls_back_the_one_shot_reservation() {
        let (registry, _, _, executor) = registry_with_posting(false);
        let id = BrowserId::new(registry.runtime_generation(), 61);
        alive(&registry, id);
        let request = registry
            .dialogs
            .insert_test(id, None, std::time::Instant::now(), Box::new(|_| {}))
            .ok()
            .unwrap();
        let control = RuntimeBrowserControl::new(id, executor.clone(), registry.clone());
        assert_eq!(
            control.submit(BrowserCommand::ResolveBeforeUnload {
                request,
                allow: false
            }),
            Err(EngineError::PostRejected)
        );
        assert!(registry.dialogs.contains(request));
        executor.set_test_post(Arc::new(|_| true));
        control
            .submit(BrowserCommand::ResolveBeforeUnload {
                request,
                allow: false,
            })
            .unwrap();
        registry.answer_before_unload(request, false).unwrap();
        assert_eq!(
            control.submit(BrowserCommand::ResolveBeforeUnload {
                request,
                allow: true
            }),
            Err(EngineError::InvalidRequest)
        );
    }

    #[test]
    fn before_unload_reset_invalidates_replies_without_reviving_terminal_state() {
        let (registry, _) = registry();
        let id = BrowserId::new(registry.runtime_generation(), 62);
        alive(&registry, id);
        registry.note_close_intent(id, CloseMode::Graceful);
        let request = registry
            .dialogs
            .insert_test(
                id,
                close_prompt_cut_in_test(&registry, id),
                std::time::Instant::now(),
                Box::new(|_| {}),
            )
            .ok()
            .unwrap();
        registry.reset_dialogs(id);
        assert!(!registry.dialogs.contains(request));
        assert_eq!(
            registry.answer_before_unload(request, true),
            Err(EngineError::InvalidRequest)
        );
        assert_eq!(registry.snapshot(id).state, SessionState::Alive);
        registry.before_close(id);
        registry.reset_dialogs(id);
        assert_eq!(registry.snapshot(id).state, SessionState::Closed);
    }

    #[test]
    fn before_unload_timeout_and_sweep_post_rejection_default_to_denial() {
        for expired in [false, true] {
            let (registry, _, _, _executor) = registry_with_posting(false);
            let id = BrowserId::new(registry.runtime_generation(), 63);
            alive(&registry, id);
            registry.note_close_intent(id, CloseMode::Graceful);
            let called = Arc::new(AtomicU64::new(0));
            let counter = called.clone();
            let now = if expired {
                std::time::Instant::now() - std::time::Duration::from_secs(12)
            } else {
                std::time::Instant::now()
            };
            let request = registry
                .dialogs
                .insert_test(
                    id,
                    close_prompt_cut_in_test(&registry, id),
                    now,
                    Box::new(move |allow| {
                        assert!(!allow);
                        counter.fetch_add(1, Ordering::Relaxed);
                    }),
                )
                .ok()
                .unwrap();
            registry.sweep_before_unload(request);
            assert_eq!(called.load(Ordering::Acquire), 1);
            assert_eq!(registry.pending_requests(), 0);
            assert_eq!(registry.snapshot(id).state, SessionState::Alive);
            assert_eq!(registry.dialog_stats().3, u64::from(expired));
        }
    }

    #[test]
    fn navigation_updates_snapshot_and_publishes_outside_the_registry_lock() {
        struct ObservingSink(StdMutex<Weak<UiRegistry>>);
        impl BrowserEventSink for ObservingSink {
            fn publish(&self, event: BrowserEventEnvelope) -> PublishResult {
                let registry = self.0.lock().unwrap().upgrade().unwrap();
                assert!(
                    registry.state.try_lock().is_ok(),
                    "host callback under registry lock"
                );
                assert_eq!(
                    registry.snapshot(event.browser).navigation.title,
                    "owned title"
                );
                // Even a dropped ordinary event must leave a resyncable snapshot.
                PublishResult::Rejected
            }
        }
        let events = Arc::new(ObservingSink(StdMutex::new(Weak::new())));
        let sinks = Arc::new(Sinks::default());
        let registry = UiRegistry::new(
            RuntimeGeneration::new(1),
            events.clone(),
            sinks.clone(),
            sinks,
            NativeAudit::new(),
        );
        *events.0.lock().unwrap() = Arc::downgrade(&registry);
        let id = BrowserId::new(registry.runtime_generation(), 45);
        registry.begin_create(id, config()).unwrap();
        registry.navigation_event(id, BrowserEvent::TitleChanged("owned title".into()));
        assert_eq!(registry.snapshot(id).state, SessionState::Creating);
        assert_eq!(registry.snapshot(id).navigation.title, "owned title");
    }

    #[test]
    fn navigation_source_urls_reach_the_common_session_gate() {
        let (registry, sinks) = registry();
        let id = BrowserId::new(registry.runtime_generation(), 47);
        alive(&registry, id);
        for url in ["http://127.0.0.1/a", "http://127.0.0.1/b"] {
            registry.navigation_observation(
                id,
                BrowserEvent::NavigationRequested {
                    is_main_frame: true,
                    is_redirect: false,
                },
                Some(url.into()),
            );
        }
        let count = sinks.events.lock().unwrap().len();
        registry.navigation_observation(
            id,
            BrowserEvent::LoadFailed {
                is_main_frame: true,
                code: -324,
            },
            Some("http://127.0.0.1/a".into()),
        );
        assert_eq!(sinks.events.lock().unwrap().len(), count);
        assert_eq!(registry.snapshot(id).navigation.last_main_frame_error, None);
        registry.navigation_observation(
            id,
            BrowserEvent::LoadFinished {
                is_main_frame: true,
                http_status_code: 200,
            },
            Some("http://127.0.0.1/b".into()),
        );
        assert_eq!(registry.snapshot(id).navigation.main_http_status, Some(200));
    }

    #[test]
    fn navigation_ignores_foreign_generation_and_terminal_callbacks() {
        let (registry, sinks) = registry();
        let id = BrowserId::new(registry.runtime_generation(), 46);
        let stale = BrowserId::new(RuntimeGeneration::new(999), id.local());
        alive(&registry, id);
        registry.navigation_event(id, BrowserEvent::TitleChanged("current".into()));
        registry.navigation_event(stale, BrowserEvent::TitleChanged("stale".into()));
        assert_eq!(registry.snapshot(id).navigation.title, "current");
        assert_eq!(sinks.events.lock().unwrap().len(), 1);
        registry.before_close(id);
        let before = registry.snapshot(id);
        let count = sinks.events.lock().unwrap().len();
        registry.navigation_event(id, BrowserEvent::AddressChanged("late".into()));
        assert_eq!(registry.snapshot(id), before);
        assert_eq!(sinks.events.lock().unwrap().len(), count);
    }

    #[derive(Default)]
    struct CommandRecorder(Vec<CommandEnvelope>);
    impl BrowserEngine for CommandRecorder {
        fn execute(&mut self, command: CommandEnvelope) -> Result<(), EngineError> {
            self.0.push(command);
            Ok(())
        }
    }

    #[test]
    fn command_capacity_consumption_and_close_use_the_real_control_path() {
        let (registry, _, posted, executor) = registry_with_posting(true);
        let id = BrowserId::new(registry.runtime_generation(), 40);
        alive(&registry, id);
        assert!(registry.snapshot(id).is_ready());
        assert_eq!(registry.frame_count(), 0, "Ready does not wait for a frame");
        let control = RuntimeBrowserControl::new(id, executor, registry.clone());
        for expected in 1..=256 {
            assert_eq!(
                control.submit(BrowserCommand::Reload).unwrap().sequence,
                expected
            );
        }
        assert_eq!(
            control.submit(BrowserCommand::Reload),
            Err(EngineError::Backpressure)
        );
        let first = match posted.lock().unwrap()[0].clone() {
            TaskPayload::Command(envelope) => envelope,
            _ => panic!("not a command"),
        };
        let mut engine = CommandRecorder::default();
        registry
            .take_command(first)
            .unwrap()
            .0
            .execute(&mut engine)
            .unwrap();
        assert_eq!(
            control
                .submit(BrowserCommand::StopLoading)
                .unwrap()
                .sequence,
            257
        );
        control.request_close(CloseMode::Force);
        assert_eq!(
            control.submit(BrowserCommand::Back),
            Err(EngineError::Closing)
        );
        let tasks = posted.lock().unwrap().clone();
        assert_eq!(tasks.len(), 258);
        assert!(matches!(tasks.last(), Some(TaskPayload::Close { .. })));
        for task in tasks.into_iter().skip(1) {
            if let TaskPayload::Command(envelope) = task {
                // Closing must not silently discard previously accepted work.
                let command = registry.take_command(envelope).unwrap().0;
                assert!(registry.state.try_lock().is_ok());
                command.execute(&mut engine).unwrap();
            }
        }
        assert_eq!(engine.0.len(), 257);
        assert_eq!(registry.lock().browsers[&id].session.pending_commands(), 0);
    }

    #[test]
    fn command_post_rejection_returns_the_reserved_capacity() {
        let (registry, _, _, executor) = registry_with_posting(false);
        let id = BrowserId::new(registry.runtime_generation(), 41);
        alive(&registry, id);
        let control = RuntimeBrowserControl::new(id, executor.clone(), registry.clone());
        assert_eq!(
            control.submit(BrowserCommand::Reload),
            Err(EngineError::PostRejected)
        );
        assert_eq!(registry.lock().browsers[&id].session.pending_commands(), 0);
        executor.set_test_post(Arc::new(|_| true));
        assert_eq!(control.submit(BrowserCommand::Reload).unwrap().sequence, 2);
        for _ in 1..256 {
            control.submit(BrowserCommand::Reload).unwrap();
        }
        assert_eq!(
            control.submit(BrowserCommand::Reload),
            Err(EngineError::Backpressure)
        );
        assert_eq!(
            registry.lock().browsers[&id].session.pending_commands(),
            256
        );
    }

    #[test]
    fn concurrent_command_sequence_matches_native_post_order() {
        let (registry, _, posted, executor) = registry_with_posting(true);
        let id = BrowserId::new(registry.runtime_generation(), 42);
        alive(&registry, id);
        let control = RuntimeBrowserControl::new(id, executor, registry);
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let control = control.clone();
                std::thread::spawn(move || {
                    for _ in 0..64 {
                        control.submit(BrowserCommand::Reload).unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let sequences: Vec<_> = posted
            .lock()
            .unwrap()
            .iter()
            .map(|task| match task {
                TaskPayload::Command(envelope) => envelope.id.sequence,
                _ => panic!("unexpected task"),
            })
            .collect();
        assert_eq!(sequences, (1..=256).collect::<Vec<_>>());
    }

    #[test]
    fn close_cutpoint_cannot_overtake_an_inflight_command_post() {
        let (registry, _, posted, executor) = registry_with_posting(true);
        let id = BrowserId::new(registry.runtime_generation(), 43);
        alive(&registry, id);
        let control = RuntimeBrowserControl::new(id, executor.clone(), registry);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = StdMutex::new(release_rx);
        let observed = posted.clone();
        executor.set_test_post(Arc::new(move |payload| {
            if matches!(payload, TaskPayload::Command(_)) {
                entered_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
            }
            observed.lock().unwrap().push(payload.clone());
            true
        }));
        let sender = control.clone();
        let submit = std::thread::spawn(move || sender.submit(BrowserCommand::Reload));
        entered_rx.recv().unwrap();
        let closer = control.clone();
        let close = std::thread::spawn(move || closer.request_close(CloseMode::Graceful));
        release_tx.send(()).unwrap();
        submit.join().unwrap().unwrap();
        close.join().unwrap();
        let tasks = posted.lock().unwrap();
        assert!(matches!(
            tasks.as_slice(),
            [TaskPayload::Command(_), TaskPayload::Close { .. }]
        ));
        drop(tasks);
        assert_eq!(
            control.submit(BrowserCommand::Forward),
            Err(EngineError::Closing)
        );
    }

    #[test]
    fn stale_generation_with_same_local_cannot_mutate_or_consume_the_instance() {
        let (registry, sinks, _, executor) = registry_with_posting(true);
        let id = BrowserId::new(registry.runtime_generation(), 44);
        let stale = BrowserId::new(RuntimeGeneration::new(999), id.local());
        alive(&registry, id);
        let foreign = RuntimeBrowserControl::new(stale, executor.clone(), registry.clone());
        assert_eq!(
            foreign.submit(BrowserCommand::Reload),
            Err(EngineError::WrongRuntime)
        );
        assert_eq!(
            registry.begin_create(stale, config()),
            Err(CreateError::WrongRuntime)
        );
        registry.note_close_intent(stale, CloseMode::Force);
        registry.before_close(stale);
        registry.reject_create(stale, EngineError::PostRejected);
        assert!(registry.snapshot(id).is_ready());
        assert_eq!(registry.snapshot(stale).browser, stale);
        assert_eq!(registry.snapshot(stale).state, SessionState::CreateFailed);
        assert!(sinks.events.lock().unwrap().is_empty());
        let control = RuntimeBrowserControl::new(id, executor, registry.clone());
        let command = control.submit(BrowserCommand::Reload).unwrap();
        let foreign = CommandEnvelope {
            id: CommandId {
                browser: stale,
                sequence: command.sequence,
            },
            command: BrowserCommand::Back,
        };
        assert!(registry.take_command(foreign).is_none());
        assert_eq!(registry.lock().browsers[&id].session.pending_commands(), 1);
        assert!(
            registry
                .take_command(CommandEnvelope {
                    id: command,
                    command: BrowserCommand::Reload,
                })
                .is_some()
        );
    }

    #[test]
    fn capture_failure_is_not_published_after_abandon_closed_the_same_source() {
        let (registry, sinks) = registry();
        let id = BrowserId::new(RuntimeGeneration::new(1), 91);
        registry.begin_create(id, config()).unwrap();
        registry
            .lock()
            .browsers
            .get_mut(&id)
            .unwrap()
            .session
            .after_created();
        let error = EngineError::Capture {
            kind: rust_cef_core::ports::CaptureFailureKind::Map,
            code: Some(-17),
        };
        registry.capture_failed(id, error.clone());
        assert_eq!(sinks.events.lock().unwrap().len(), 1);
        registry.note_close_intent(id, CloseMode::Force);
        registry.abandon_native_refs();
        registry.capture_failed(id, error);
        assert_eq!(sinks.events.lock().unwrap().len(), 1);
    }

    #[test]
    fn backend_is_only_observed_after_a_real_frame() {
        let (registry, _) = registry();
        assert_eq!(registry.observed_backend(), None);
        registry.note_software_frame();
        assert_eq!(
            registry.observed_backend(),
            Some(RenderPreference::SoftwareRequired)
        );
        assert_eq!(registry.frame_count(), 1);
    }

    #[test]
    fn view_attachment_claims_once_per_browser_and_reuses_the_runtime_source() {
        let (registry, _, _, _executor) = registry_with_posting(true);
        let id = BrowserId::new(registry.runtime_generation(), 61);
        alive(&registry, id);
        let (source, mode) = registry.claim_view_attachment(id).unwrap();
        assert_eq!(mode, BrowserSurfaceMode::Offscreen);
        assert!(Arc::ptr_eq(&source, &registry.frame_source(id).unwrap()));
        assert!(matches!(
            registry.claim_view_attachment(id),
            Err(AttachmentError::AlreadyAttached)
        ));
        // The claim is sticky even though the first token was dropped unused.
        drop(source);
        assert!(matches!(
            registry.claim_view_attachment(id),
            Err(AttachmentError::AlreadyAttached)
        ));

        // An already-accepted but not yet Alive Browser can be attached; this
        // path only reads the existing entry and never creates anything.
        let creating = BrowserId::new(registry.runtime_generation(), 62);
        registry.begin_create(creating, config()).unwrap();
        assert_eq!(registry.snapshot(creating).state, SessionState::Creating);
        let (creating_source, _) = registry.claim_view_attachment(creating).unwrap();
        assert!(Arc::ptr_eq(
            &creating_source,
            &registry.frame_source(creating).unwrap()
        ));
        assert!(matches!(
            registry.claim_view_attachment(creating),
            Err(AttachmentError::AlreadyAttached)
        ));

        // Merely reading the authoritative endpoint (the existing public
        // `frame_source`) is not a claim and clones no second source: the
        // Browser is still attachable exactly once and gets that same Arc.
        let read_only = BrowserId::new(registry.runtime_generation(), 70);
        alive(&registry, read_only);
        let first_read = registry.frame_source(read_only).unwrap();
        let second_read = registry.frame_source(read_only).unwrap();
        assert!(Arc::ptr_eq(&first_read, &second_read));
        let (claimed, _) = registry.claim_view_attachment(read_only).unwrap();
        assert!(Arc::ptr_eq(&claimed, &first_read));
        assert!(matches!(
            registry.claim_view_attachment(read_only),
            Err(AttachmentError::AlreadyAttached)
        ));
    }

    #[test]
    fn view_attachment_rejects_foreign_draining_and_terminal_ids_without_consuming_local() {
        let (registry, _, _, _executor) = registry_with_posting(true);
        let id = BrowserId::new(registry.runtime_generation(), 63);
        alive(&registry, id);
        let foreign = BrowserId::new(RuntimeGeneration::new(999), id.local());
        assert!(matches!(
            registry.claim_view_attachment(foreign),
            Err(AttachmentError::Engine(EngineError::WrongRuntime))
        ));
        // The failed foreign claim must not occupy the local identity.
        assert!(registry.claim_view_attachment(id).is_ok());

        let absent = BrowserId::new(registry.runtime_generation(), 64);
        assert!(matches!(
            registry.claim_view_attachment(absent),
            Err(AttachmentError::Engine(EngineError::Closed))
        ));

        // A terminal create and an OnBeforeClose-closed source are both refused.
        let failed = BrowserId::new(registry.runtime_generation(), 65);
        registry.begin_create(failed, config()).unwrap();
        registry.reject_create(failed, EngineError::PostRejected);
        assert!(matches!(
            registry.claim_view_attachment(failed),
            Err(AttachmentError::Engine(EngineError::Closed))
        ));
        let closed = BrowserId::new(registry.runtime_generation(), 66);
        alive(&registry, closed);
        registry.before_close(closed);
        assert!(matches!(
            registry.claim_view_attachment(closed),
            Err(AttachmentError::Engine(EngineError::Closed))
        ));
        // A sealed frame source is refused even while the session state is not
        // yet terminal: the claim never hands out a permanently closed source.
        let sealed = BrowserId::new(registry.runtime_generation(), 69);
        alive(&registry, sealed);
        // Dropping the returned ticket is fine; the store itself stays the
        // same permanently closed instance.
        let sealed_store = registry.lock().browsers[&sealed].store.clone();
        let _ = sealed_store.close();
        assert!(matches!(
            registry.claim_view_attachment(sealed),
            Err(AttachmentError::Engine(EngineError::Closed))
        ));

        // Drainage is checked in the same state lock as the claim.
        let draining = BrowserId::new(registry.runtime_generation(), 67);
        alive(&registry, draining);
        registry.set_draining();
        assert!(matches!(
            registry.claim_view_attachment(draining),
            Err(AttachmentError::Engine(EngineError::Closing))
        ));
    }

    #[test]
    fn dropping_an_unused_attachment_requests_one_graceful_close_on_the_original_handle() {
        let (registry, _, posted, executor) = registry_with_posting(true);
        let id = BrowserId::new(registry.runtime_generation(), 68);
        alive(&registry, id);
        let control = RuntimeBrowserControl::new(id, executor, registry.clone());
        let handle = BrowserHandle::new(control);
        let (source, mode) = registry.claim_view_attachment(id).unwrap();
        assert_eq!(mode, BrowserSurfaceMode::Offscreen);
        let attachment = BrowserAttachment::new(id, handle.clone(), source).with_surface_mode(mode);
        assert_eq!(attachment.browser(), id);
        assert_eq!(attachment.handle().snapshot().browser, id);
        assert!(Arc::ptr_eq(
            attachment.frame_source(),
            &registry.frame_source(id).unwrap()
        ));
        drop(attachment);
        // Drop is pure data: it submits one ordinary Graceful close and never
        // pumps, shuts down or touches a native object itself.
        let closes: Vec<_> = posted
            .lock()
            .unwrap()
            .iter()
            .filter(|task| matches!(task, TaskPayload::Close { .. }))
            .cloned()
            .collect();
        assert!(matches!(closes.as_slice(), [TaskPayload::Close {
            browser, mode: CloseMode::Graceful, ticket: Some(ticket),
        }] if *browser == id && ticket.browser == id && ticket.sequence > 0 && ticket.result() == CloseResult::Pending));
        assert_eq!(
            registry.lock().browsers[&id].close_mode,
            Some(CloseMode::Graceful)
        );
        assert_eq!(registry.snapshot(id).state, SessionState::Closing);
    }

    #[derive(Default)]
    struct RecordedClicks(StdMutex<Vec<String>>);

    impl MouseClickTarget for RecordedClicks {
        fn focus(&mut self, focused: bool) {
            self.0.lock().unwrap().push(format!("focus {focused}"));
        }

        fn move_to(&mut self, x: i32, y: i32, modifiers: u32) {
            self.0
                .lock()
                .unwrap()
                .push(format!("move {x} {y} {modifiers}"));
        }

        fn button(&mut self, x: i32, y: i32, modifiers: u32, up: bool, click_count: i32) {
            self.0
                .lock()
                .unwrap()
                .push(format!("button {x} {y} {modifiers} {up} {click_count}"));
        }
    }

    #[test]
    fn mouse_click_dispatches_focus_move_down_up_with_core_modifier_flags() {
        use rust_cef_core::ports::MouseModifiers;
        use std::num::NonZeroU8;

        let audit = NativeAudit::new();
        let mut sink = RecordedClicks::default();
        dispatch_mouse_click(
            MouseClick {
                x: 37,
                y: 41,
                modifiers: MouseModifiers {
                    shift: true,
                    control: false,
                    alt: true,
                    platform: true,
                },
                click_count: NonZeroU8::new(2).unwrap(),
            },
            &audit,
            &mut sink,
        );

        let flags = rust_cef_core::keymap::modifier_flags(true, false, true, true);
        // DIPs are passed through unchanged; the same count reaches both halves.
        assert_eq!(
            sink.0.into_inner().unwrap(),
            [
                "focus true".to_owned(),
                format!("move 37 41 {flags}"),
                format!("button 37 41 {flags} false 2"),
                format!("button 37 41 {flags} true 2"),
            ]
        );
        // Exactly one audit entry per native call.
        assert_eq!(audit.snapshot(), (4, 0));
    }

    // ---- IME geometry slot -------------------------------------------------

    fn ime_report(bounds: &[ImeRect]) -> ImeReportedBounds {
        ImeReportedBounds::Reported {
            count: bounds.len(),
            retained: bounds.to_vec(),
        }
    }

    #[test]
    fn ime_snapshot_is_not_ready_until_a_callback_is_observed() {
        let (registry, sinks) = ime_registry();
        let browser = BrowserId::new(RuntimeGeneration::new(1), 81);
        alive(&registry, browser);
        let handle = BrowserHandle::new(RuntimeBrowserControl::new(
            browser,
            Executor::new(registry.runtime_generation(), &registry, NativeAudit::new()),
            registry.clone(),
        ));
        // A live instance that never produced a real callback reports NotReady,
        // never a synthesized zero observation.
        assert_eq!(handle.ime_snapshot(), Err(EngineError::NotReady));
        let wakes = sinks.wakes.load(Ordering::Acquire);
        assert_eq!(
            registry.observe_ime(
                browser,
                Some(Utf16Range { start: 0, end: 1 }),
                ime_report(&[])
            ),
            Ok(Some(1)),
            "an empty report is a real observation"
        );
        let snapshot = handle.ime_snapshot().unwrap();
        assert_eq!(snapshot.browser, browser);
        assert_eq!(snapshot.status, ImeStatus::Cleared);
        assert_eq!(snapshot.observed_input_epoch, 1);
        assert_eq!(sinks.wakes.load(Ordering::Acquire), wakes + 1);
        assert!(sinks.events.lock().unwrap().is_empty());
    }

    #[test]
    fn ime_geometry_is_bounded_and_invalid_reports_clear_previous_bounds() {
        let (registry, sinks) = ime_registry();
        let browser = BrowserId::new(RuntimeGeneration::new(1), 82);
        alive(&registry, browser);
        let wakes = sinks.wakes.load(Ordering::Acquire);
        let range = Some(Utf16Range { start: 0, end: 2 });
        let bounds = [ime_rect(4, 6, 8, 18), ime_rect(12, 6, 8, 18)];
        assert_eq!(
            registry.observe_ime(browser, range, ime_report(&bounds)),
            Ok(Some(1))
        );
        let ready = registry.ime_snapshot(browser).unwrap();
        assert_eq!(ready.status, ImeStatus::Ready);
        assert_eq!(ready.character_bounds, bounds.to_vec());
        assert_eq!(ready.range, range);
        assert_eq!(sinks.wakes.load(Ordering::Acquire), wakes + 1);

        // Over-cap is refused before collection and wakes without a frame.
        let over = ImeReportedBounds::from_reported(MAX_IME_CHARACTER_BOUNDS + 1, || {
            Some(ime_rect(0, 0, 1, 1))
        });
        assert_eq!(registry.observe_ime(browser, range, over), Ok(Some(2)));
        let rejected = registry.ime_snapshot(browser).unwrap();
        assert_eq!(
            rejected.status,
            ImeStatus::Rejected(ImeRejection::OverflowAllocation)
        );
        assert!(rejected.character_bounds.is_empty());
        assert_eq!(sinks.wakes.load(Ordering::Acquire), wakes + 2);

        // Bad geometry rejects and clears the old rectangles.
        assert_eq!(
            registry.observe_ime(browser, range, ime_report(&bounds)),
            Ok(Some(3))
        );
        assert_eq!(
            registry.observe_ime(browser, range, ime_report(&[ime_rect(0, 0, -4, 18)])),
            Ok(Some(4))
        );
        let invalid = registry.ime_snapshot(browser).unwrap();
        assert_eq!(
            invalid.status,
            ImeStatus::Rejected(ImeRejection::InvalidGeometry)
        );
        assert!(invalid.character_bounds.is_empty());

        // A missing array is not an empty one.
        assert_eq!(
            registry.observe_ime(browser, range, ImeReportedBounds::Missing),
            Ok(Some(5))
        );
        assert_eq!(
            registry.ime_snapshot(browser).unwrap().status,
            ImeStatus::Rejected(ImeRejection::MissingGeometry)
        );
    }

    struct CloseWakeFixture {
        registry: Arc<UiRegistry>,
        sinks: Arc<ImeSinks>,
        control: Arc<RuntimeBrowserControl>,
        browser: BrowserId,
        posts: Arc<StdMutex<Vec<CloseMode>>>,
    }

    impl CloseWakeFixture {
        fn new() -> Self {
            let (registry, sinks) = ime_registry();
            let browser = BrowserId::new(registry.runtime_generation(), 85);
            alive(&registry, browser);
            assert_eq!(
                registry.observe_ime(
                    browser,
                    Some(Utf16Range { start: 0, end: 1 }),
                    ime_report(&[ime_rect(1, 2, 3, 4)]),
                ),
                Ok(Some(1))
            );
            let executor =
                Executor::new(registry.runtime_generation(), &registry, NativeAudit::new());
            registry.attach_executor(&executor);
            let posts = Arc::new(StdMutex::new(Vec::new()));
            let recorded = posts.clone();
            executor.set_test_post(Arc::new(move |payload| {
                let TaskPayload::Close {
                    browser: actual,
                    mode,
                    ..
                } = payload
                else {
                    panic!("unexpected post in close wake fixture")
                };
                assert_eq!(*actual, browser);
                recorded.lock().unwrap().push(*mode);
                true
            }));
            let control = RuntimeBrowserControl::new(browser, executor, registry.clone());
            Self {
                registry,
                sinks,
                control,
                browser,
                posts,
            }
        }
    }

    #[test]
    fn close_ticket_is_reused_when_its_ime_wake_reenters_with_force() {
        let f = CloseWakeFixture::new();
        let nested_ticket = Arc::new(StdMutex::new(None));
        let (control, output) = (f.control.clone(), nested_ticket.clone());
        *f.sinks.on_wake.lock().unwrap() = Some(Box::new(move || {
            // Detect the old lock inversion without making a broken run hang.
            assert!(
                control.active_close.try_lock().is_ok(),
                "close wake retained the ticket lock"
            );
            *output.lock().unwrap() = Some(control.request_close(CloseMode::Force));
        }));
        let first = f.control.request_close(CloseMode::Graceful);
        let nested = nested_ticket.lock().unwrap().take().unwrap();
        assert_eq!(
            (first.browser, first.sequence),
            (nested.browser, nested.sequence)
        );
        assert_eq!(first.result(), CloseResult::Pending);
        assert_eq!(nested.result(), CloseResult::Pending);
        assert_eq!(f.posts.lock().unwrap().as_slice(), &[CloseMode::Force]);
        f.registry.before_close(f.browser);
        assert_eq!(first.result(), CloseResult::Closed);
        assert_eq!(nested.result(), CloseResult::Closed);
    }

    #[test]
    fn close_ticket_cancelled_during_its_ime_wake_does_not_post() {
        let f = CloseWakeFixture::new();
        let (registry, browser) = (f.registry.clone(), f.browser);
        *f.sinks.on_wake.lock().unwrap() = Some(Box::new(move || {
            let request = registry
                .dialogs
                .insert_test(
                    browser,
                    close_prompt_cut_in_test(&registry, browser),
                    std::time::Instant::now(),
                    Box::new(|allow| assert!(!allow)),
                )
                .ok()
                .unwrap();
            registry.answer_before_unload(request, false).unwrap();
        }));
        let cancelled = f.control.request_close(CloseMode::Graceful);
        assert_eq!(cancelled.result(), CloseResult::Cancelled);
        assert_eq!(f.registry.snapshot(f.browser).state, SessionState::Alive);
        assert!(f.posts.lock().unwrap().is_empty());
        let second = f.control.request_close(CloseMode::Graceful);
        assert_ne!(second.sequence, cancelled.sequence);
        assert_eq!(second.result(), CloseResult::Pending);
        assert!(
            f.registry
                .schedule_browser_close(f.browser, CloseMode::Force, Some(&cancelled))
                .is_none()
        );
        assert_eq!(f.posts.lock().unwrap().as_slice(), &[CloseMode::Graceful]);
        f.registry.before_close(f.browser);
        assert_eq!(second.result(), CloseResult::Closed);
        assert_eq!(cancelled.result(), CloseResult::Cancelled);
    }

    fn close_prompt_cut_in_test(
        registry: &UiRegistry,
        browser: BrowserId,
    ) -> Option<ClosePromptCut> {
        let (cut, force) = registry.before_unload_status(browser).unwrap();
        assert!(!force && cut.is_some());
        cut
    }

    fn cancel_graceful_in_test(registry: &UiRegistry, browser: BrowserId) {
        let request = registry
            .dialogs
            .insert_test(
                browser,
                close_prompt_cut_in_test(registry, browser),
                std::time::Instant::now(),
                Box::new(|allow| assert!(!allow)),
            )
            .ok()
            .unwrap();
        registry.answer_before_unload(request, false).unwrap();
    }

    fn admit_queued_close_in_test(registry: &UiRegistry, payload: &TaskPayload) {
        let TaskPayload::Close {
            browser,
            mode,
            ticket: Some(ticket),
        } = payload
        else {
            panic!("caller ticket missing from the actual queued Close")
        };
        registry.close_now_inner(
            *browser,
            *mode,
            Some(ticket),
            #[cfg(feature = "test-support")]
            None,
        );
    }

    #[test]
    fn queued_close_of_cancelled_ticket_cannot_reclose_original_browser() {
        let (registry, _, posted, executor) = registry_with_posting(true);
        let browser = BrowserId::new(registry.runtime_generation(), 86);
        alive(&registry, browser);
        let control = RuntimeBrowserControl::new(browser, executor, registry.clone());
        let old = control.request_close(CloseMode::Graceful);
        let queued = posted.lock().unwrap()[0].clone();
        assert!(
            matches!(&queued, TaskPayload::Close { ticket: Some(ticket), .. } if ticket == &old)
        );

        cancel_graceful_in_test(&registry, browser);
        assert_eq!(old.result(), CloseResult::Cancelled);
        admit_queued_close_in_test(&registry, &queued);
        assert_eq!(registry.snapshot(browser).state, SessionState::Alive);
        assert!(registry.lock().browsers[&browser].close_mode.is_none());
        assert!(registry.lock().browsers[&browser].close_posted.is_none());
        assert_eq!(posted.lock().unwrap().len(), 1);

        let new = control.request_close(CloseMode::Force);
        assert_ne!(new.sequence, old.sequence);
        assert!(
            matches!(&posted.lock().unwrap()[1], TaskPayload::Close { mode: CloseMode::Force, ticket: Some(ticket), .. } if ticket == &new)
        );
        registry.before_close(browser);
        assert_eq!(old.result(), CloseResult::Cancelled);
        assert_eq!(new.result(), CloseResult::Closed);
    }

    #[test]
    fn input_deferred_close_carries_cancelled_ticket_across_cleanup_handoff() {
        let (registry, _, posted, executor) = registry_with_posting(true);
        let browser = BrowserId::new(registry.runtime_generation(), 87);
        alive(&registry, browser);
        let control = RuntimeBrowserControl::new(browser, executor, registry.clone());
        control
            .submit(BrowserCommand::ResetInput(InputResetReason::FocusLost))
            .unwrap();
        let old = control.request_close(CloseMode::Graceful);
        let queued = posted.lock().unwrap().last().unwrap().clone();
        admit_queued_close_in_test(&registry, &queued);
        let handed_off = registry
            .lock()
            .browsers
            .get_mut(&browser)
            .unwrap()
            .close_waiting_input
            .take()
            .unwrap();
        assert_eq!(handed_off.ticket.as_ref(), Some(&old));

        // Model a cancel during the unlocked wake after the reset code has
        // already moved this deferred close to a local variable.
        cancel_graceful_in_test(&registry, browser);
        assert_eq!(old.result(), CloseResult::Cancelled);
        registry.close_now_inner(
            browser,
            handed_off.mode,
            handed_off.ticket.as_ref(),
            #[cfg(feature = "test-support")]
            None,
        );
        let state = registry.lock();
        let entry = &state.browsers[&browser];
        assert_eq!(entry.session.snapshot().state, SessionState::Alive);
        assert!(entry.close_waiting_input.is_none());
        assert!(entry.close_mode.is_none());
    }

    #[test]
    fn ime_reset_and_close_refuse_late_geometry_and_wake_outside_the_lock() {
        let (registry, sinks) = ime_registry();
        let browser = BrowserId::new(RuntimeGeneration::new(1), 83);
        alive(&registry, browser);
        let range = Some(Utf16Range { start: 0, end: 1 });
        let bounds = [ime_rect(1, 2, 3, 4)];
        assert_eq!(
            registry.observe_ime(browser, range, ime_report(&bounds)),
            Ok(Some(1))
        );
        let wakes = sinks.wakes.load(Ordering::Acquire);

        // Losing the text target invalidates the retained anchor outside the
        // lock. The flag is a stream observation, not a permanent gate: fixed
        // CEF reports it only through OnVirtualKeyboardRequested.
        registry.set_accepts_text(browser, false);
        let cleared = registry.ime_snapshot(browser).unwrap();
        assert_eq!(cleared.status, ImeStatus::Unavailable);
        assert!(cleared.character_bounds.is_empty());
        assert_eq!(sinks.wakes.load(Ordering::Acquire), wakes + 1);

        // Close intent retires the anchor, and a late callback cannot revive it.
        assert_eq!(
            registry.observe_ime(browser, range, ime_report(&bounds)),
            Ok(Some(3))
        );
        registry.note_close_intent(browser, CloseMode::Graceful);
        assert_eq!(
            registry.ime_snapshot(browser).unwrap().status,
            ImeStatus::Unavailable
        );
        assert_eq!(
            registry.observe_ime(browser, range, ime_report(&bounds)),
            Ok(None)
        );

        registry.before_close(browser);
        assert_eq!(
            registry.ime_snapshot(browser),
            Err(EngineError::Closed),
            "a closed instance never reports a stale slot"
        );
        assert_eq!(
            registry.observe_ime(browser, range, ime_report(&bounds)),
            Ok(None)
        );
    }

    #[test]
    fn ime_dispatch_effects_and_stale_guard_match_commit_semantics() {
        let (registry, sinks) = ime_registry();
        let browser = BrowserId::new(RuntimeGeneration::new(1), 84);
        alive(&registry, browser);
        let range = Some(Utf16Range { start: 0, end: 1 });
        let bounds = [ime_rect(5, 5, 5, 15)];

        // Only a new composition clears before dispatch; commit/finish/cancel
        // clear after a successful native call; everything else leaves it.
        let effect = |kind| ime_dispatch_effect(Some(&InputEvent { epoch: 1, kind }));
        assert_eq!(
            effect(InputKind::ImeSet {
                text: "x".into(),
                selection: Utf16Range { start: 0, end: 1 },
                replacement: None,
            }),
            ImeDispatchEffect::ClearBefore
        );
        for kind in [
            InputKind::Text("x".into()),
            InputKind::ImeFinish {
                keep_selection: false,
            },
            InputKind::ImeCancel,
        ] {
            assert_eq!(effect(kind), ImeDispatchEffect::ClearAfter);
        }
        assert_eq!(
            effect(InputKind::ImeFinish {
                keep_selection: true,
            }),
            ImeDispatchEffect::ClearAfter
        );
        assert_eq!(
            ime_dispatch_effect(None),
            ImeDispatchEffect::None,
            "a non-input command must not retire geometry"
        );

        assert_eq!(
            registry.observe_ime(browser, range, ime_report(&bounds)),
            Ok(Some(1))
        );
        let observed = registry.ime_revision(browser).unwrap();
        // A synchronous callback the native call itself produced is newer than
        // the anchor this commit retires, so the stale guard leaves it alone.
        assert_eq!(
            registry.observe_ime(browser, range, ime_report(&bounds)),
            Ok(Some(2))
        );
        assert!(!registry.invalidate_stale_ime(browser, observed));
        assert_eq!(
            registry.ime_snapshot(browser).unwrap().status,
            ImeStatus::Ready
        );
        // The unchanged revision is still clearable.
        let unchanged = registry.ime_revision(browser).unwrap();
        assert!(registry.invalidate_stale_ime(browser, unchanged));
        assert_eq!(
            registry.ime_snapshot(browser).unwrap().status,
            ImeStatus::Unavailable
        );
        assert!(sinks.wakes.load(Ordering::Acquire) >= 2);
    }

    #[test]
    fn ime_failure_cutpoints_wake_after_unlock_even_without_a_reset_task() {
        for path in ["validation", "post", "execution"] {
            let (registry, sinks) = ime_registry();
            let browser = BrowserId::new(RuntimeGeneration::new(1), 87);
            alive(&registry, browser);
            let executor =
                Executor::new(registry.runtime_generation(), &registry, NativeAudit::new());
            registry.attach_executor(&executor);
            let posted = Arc::new(StdMutex::new(Vec::new()));
            let observed = posted.clone();
            executor.set_test_post(Arc::new(move |payload| {
                if path == "post" {
                    return false;
                }
                observed.lock().unwrap().push(payload.clone());
                true
            }));
            registry
                .observe_ime(
                    browser,
                    Some(Utf16Range { start: 0, end: 1 }),
                    ime_report(&[ime_rect(1, 2, 3, 4)]),
                )
                .unwrap();
            let before = sinks.wakes.load(Ordering::Acquire);
            let kind = if path == "validation" {
                InputKind::ReplaceText {
                    text: "x".into(),
                    replacement: Utf16Range { start: 0, end: 1 },
                }
            } else {
                InputKind::Text("x".into())
            };
            let result = registry.submit_command(
                browser,
                &executor,
                BrowserCommand::Input(InputEvent { epoch: 1, kind }),
            );
            match path {
                "validation" => assert_eq!(result, Err(EngineError::UnsupportedInputReplacement)),
                "post" => assert_eq!(result, Err(EngineError::PostRejected)),
                _ => {
                    assert!(result.is_ok());
                    let payload = posted.lock().unwrap()[0].clone();
                    let TaskPayload::Command(envelope) = payload else {
                        panic!("expected the admitted command");
                    };
                    // Real run_command failure branch, but no native Browser or
                    // CEF input call: this is protocol evidence, not native IME.
                    registry.run_command(envelope);
                }
            }
            assert_eq!(
                registry.ime_snapshot(browser).unwrap().status,
                ImeStatus::Unavailable,
                "{path}"
            );
            assert_eq!(
                sinks.wakes.load(Ordering::Acquire),
                before + 1,
                "{path}: reset may never execute"
            );
        }
    }

    #[test]
    fn ime_snapshot_rejects_a_foreign_runtime_and_an_unknown_browser() {
        let (registry, _) = ime_registry();
        let browser = BrowserId::new(RuntimeGeneration::new(1), 85);
        alive(&registry, browser);
        assert_eq!(
            registry.ime_snapshot(BrowserId::new(RuntimeGeneration::new(2), 85)),
            Err(EngineError::WrongRuntime)
        );
        assert_eq!(
            registry.ime_snapshot(BrowserId::new(RuntimeGeneration::new(1), 86)),
            Err(EngineError::Closed)
        );
        // An unobserved live browser is NotReady, not zeros from another
        // identity; a foreign/absent identity is refused before that check.
        assert_eq!(registry.ime_snapshot(browser), Err(EngineError::NotReady));
    }

    #[test]
    fn writable_cef_string_exposes_non_null_mut_ptr_unlike_default() {
        let mut default_str = CefString::default();
        let default_ptr: *mut cef::sys::_cef_string_utf16_t = (&mut default_str).into();
        assert!(
            default_ptr.is_null(),
            "CefString::default() converts to NULL for mutable out-params"
        );

        let mut writable = WritableCefString::new();
        let mut cef_writable = writable.as_mut_cef();
        let writable_ptr: *mut cef::sys::_cef_string_utf16_t = (&mut cef_writable).into();
        assert!(
            !writable_ptr.is_null(),
            "WritableCefString must provide a non-null writable cef_string_t"
        );
        assert!(cef_writable.try_set("sample pref error"));
        assert_eq!(writable.to_string_lossy(), "sample pref error");
    }

    #[test]
    fn build_browser_settings_maps_frame_rate_encoding_color_and_feature_states() {
        let defaults = build_browser_settings(&BrowserOptions::default(), 0xFFFF_FFFF);
        assert_eq!(defaults.windowless_frame_rate, 60);
        assert_eq!(defaults.background_color, 0xFFFF_FFFF);
        assert_eq!(defaults.javascript, cef::State::DEFAULT);

        let custom = BrowserOptions::default()
            .with_frame_rate(120)
            .with_background_color(0xFF00_AA55)
            .with_default_encoding("UTF-8")
            .with_feature_states(rust_cef_core::contracts::BrowserFeatureStates {
                javascript: CefFeatureState::Disabled,
                local_storage: CefFeatureState::Enabled,
                image_loading: CefFeatureState::Disabled,
                tab_to_links: CefFeatureState::Enabled,
                javascript_access_clipboard: CefFeatureState::Disabled,
            });
        let mapped = build_browser_settings(&custom, 0xFFFF_FFFF);
        assert_eq!(mapped.windowless_frame_rate, 120);
        assert_eq!(mapped.background_color, 0xFF00_AA55);
        assert_eq!(mapped.default_encoding.to_string(), "UTF-8");
        assert_eq!(mapped.javascript, cef::State::DISABLED);
        assert_eq!(mapped.local_storage, cef::State::ENABLED);
        assert_eq!(mapped.image_loading, cef::State::DISABLED);
        assert_eq!(mapped.tab_to_links, cef::State::ENABLED);
        assert_eq!(mapped.javascript_access_clipboard, cef::State::DISABLED);
    }

    #[test]
    fn begin_create_rejects_out_of_range_frame_rate_without_registering() {
        let (r, _, posted, _) = registry_with_posting(true);
        let absent = BrowserId::new(RuntimeGeneration::new(1), 88);
        for fps in [0, 241] {
            let mut invalid = config();
            invalid.options.windowless_frame_rate = Some(fps);
            assert_eq!(
                r.begin_create(absent, invalid),
                Err(CreateError::InvalidOptions)
            );
            assert!(matches!(r.frame_source(absent), Err(EngineError::Closed)));
        }
        assert!(posted.lock().unwrap().is_empty());
    }

    #[test]
    fn ux_events_bounded_cap_64_and_take_ux_events_drains() {
        let (r, _, _, _) = registry_with_posting(true);
        let browser = BrowserId::new(RuntimeGeneration::new(1), 89);
        alive(&r, browser);
        for index in 0..70 {
            r.navigation_event(
                browser,
                BrowserEvent::TooltipChanged(format!("tip-{index}")),
            );
        }
        let drained = r.take_ux_events(browser);
        assert_eq!(drained.len(), MAX_UX_EVENTS);
        assert_eq!(
            drained.first(),
            Some(&BrowserEvent::TooltipChanged("tip-6".into()))
        );
        assert_eq!(
            drained.last(),
            Some(&BrowserEvent::TooltipChanged("tip-69".into()))
        );
        assert!(r.take_ux_events(browser).is_empty());
        assert!(
            r.take_ux_events(BrowserId::new(RuntimeGeneration::new(2), 89))
                .is_empty()
        );
        assert!(
            r.take_ux_events(BrowserId::new(RuntimeGeneration::new(1), 99))
                .is_empty()
        );
    }

    #[test]
    fn evaluate_script_cancels_on_navigation_termination_and_close() {
        let (r, sinks, _, executor) = registry_with_posting(true);
        let b1 = BrowserId::new(RuntimeGeneration::new(1), 91);
        alive(&r, b1);

        let id1 = r
            .submit_eval(b1, &executor, "1 + 1".into(), JsFrameScope::MainFrame, 5000)
            .expect("eval 1 admitted");
        assert_eq!(r.pending_requests(), 1);
        // Main-frame navigation cancels pending evals and bumps document_epoch.
        r.navigation_observation(
            b1,
            BrowserEvent::LoadStarted {
                is_main_frame: true,
            },
            Some("app://local/next.html".into()),
        );
        assert_eq!(r.pending_requests(), 0);
        // Late completion from old epoch is dropped as stale.
        r.complete_eval(id1, Some(1), EvalOutcome::Value("2".into()));
        assert_eq!(r.js_bridge_stats().3, 1);

        let js_evts = r.take_js_events(b1);
        assert!(matches!(
            js_evts.as_slice(),
            [BrowserEvent::EvalCompleted {
                id,
                outcome: EvalOutcome::Err(EvalError::Navigated),
            }] if *id == id1
        ));

        // Renderer termination cancels cleanly.
        let id2 = r
            .submit_eval(b1, &executor, "2 + 2".into(), JsFrameScope::MainFrame, 5000)
            .expect("eval 2 admitted");
        r.renderer_event(
            b1,
            BrowserEvent::RendererTerminated {
                native_status: 1,
                code: Some(9),
            },
        );
        assert_eq!(r.pending_requests(), 0);
        assert!(matches!(
            r.take_js_events(b1).as_slice(),
            [BrowserEvent::EvalCompleted {
                id,
                outcome: EvalOutcome::Err(EvalError::RendererTerminated),
            }] if *id == id2
        ));

        // Bring back b2 to test close + direct unallocated EvaluateScript refusal.
        let b2 = BrowserId::new(RuntimeGeneration::new(1), 92);
        alive(&r, b2);
        let control = RuntimeBrowserControl::new(b2, executor.clone(), r.clone());
        assert_eq!(
            control.submit(BrowserCommand::EvaluateScript {
                id: EvalRequestId {
                    browser: b2,
                    sequence: 9999,
                },
                script: "1".into(),
                frame_scope: JsFrameScope::MainFrame,
                timeout_ms: 1000,
            }),
            Err(EngineError::InvalidRequest)
        );
        let id3 = control
            .evaluate_script("3 + 3".into(), JsFrameScope::MainFrame, 1000)
            .expect("eval 3 admitted");
        assert_eq!(r.pending_requests(), 1);
        r.before_close(b2);
        assert_eq!(r.pending_requests(), 0);
        let events = sinks.events.lock().unwrap();
        assert!(events.iter().any(|e| {
            matches!(
                e,
                BrowserEvent::EvalCompleted {
                    id,
                    outcome: EvalOutcome::Err(EvalError::Closed),
                } if *id == id3
            )
        }));
    }

    #[test]
    fn windowless_disabled_rejects_offscreen_and_admits_native_child_hwnd() {
        let (registry, _, _, _executor) = registry_with_posting(true);
        registry.set_windowless_rendering_enabled(false);
        assert!(!registry.windowless_rendering_enabled());

        let offscreen_id = BrowserId::new(registry.runtime_generation(), 93);
        assert_eq!(
            registry.begin_create(offscreen_id, config()),
            Err(CreateError::InvalidOptions)
        );

        let native_id = BrowserId::new(registry.runtime_generation(), 94);
        let native_cfg = rust_cef_core::contracts::NativeChildHwndConfig {
            parent_hwnd: NonZeroUsize::new(0x1234).unwrap(),
            initial_bounds: PixelRect {
                x: 0,
                y: 0,
                width: 640,
                height: 480,
            },
            activate_on_creation: false,
        };
        let mut cfg = config();
        cfg.options.surface_mode = BrowserSurfaceMode::NativeChildHwnd(native_cfg);
        registry.begin_create(native_id, cfg).unwrap();

        let (_source, mode) = registry.claim_view_attachment(native_id).unwrap();
        assert_eq!(mode, BrowserSurfaceMode::NativeChildHwnd(native_cfg));
        assert_eq!(
            registry.surface_mode(native_id),
            Some(BrowserSurfaceMode::NativeChildHwnd(native_cfg))
        );
    }
}
