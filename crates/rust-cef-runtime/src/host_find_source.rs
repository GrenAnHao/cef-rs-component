//! Host-triggered Page Find and View Source operations routed through Runtime
//! without requiring a native context menu popup or frame focus.
//!
//! Upstream CefSharp anchors (CEF 156.0.31 / CefSharp rev `41919703`):
//! - `D:\System Dir\Downloads\CefSharp\CefSharp.Core.Runtime\Internals\CefBrowserHostWrapper.cpp:300-312`
//!   (`Find` forwarding `searchText, forward, matchCase, findNext` and `StopFinding(clearSelection)`)
//! - `D:\System Dir\Downloads\CefSharp\CefSharp\WebBrowserExtensions.cs:291-313`
//!   (`Find` / `StopFinding` convenience APIs)
//! - `D:\System Dir\Downloads\CefSharp\CefSharp\WebBrowserExtensions.cs:1264-1315`
//!   (`ViewSource` acquiring the main-frame HTML source asynchronously)

use crate::{
    executor::TaskPayload,
    find::{FindScope, find_receipt},
    menu_bridge::{Scope, TargetMenuContext},
    registry::UiRegistry,
    source_view::SourceScope,
};
use cef::*;
use rust_cef_core::{
    contracts::{BrowserId, SessionState},
    find::{FindFailure, FindOperationId, FindOutcome, FindRequest, FindResultSink},
    menu::ContextMenuRequestId,
    menu_context::MenuContext,
    menu_lease::MenuLeaseError,
    ports::{EngineError, WindowBinding},
    source_view::{SourceFailure, SourceOperationId, SourceViewSink},
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Default)]
struct BrowserTargets {
    find: Option<Arc<dyn FindResultSink>>,
    source: Option<Arc<dyn SourceViewSink>>,
}

/// Direct per-browser sink registry for host-triggered Find and View Source
/// when menu-bridge find/source services are not bound (e.g. NativeHwnd or
/// `--context-menu false`).
pub(crate) struct HostFindSourceBook {
    next_menu_sequence: AtomicU64,
    browsers: Mutex<BTreeMap<BrowserId, BrowserTargets>>,
}

impl HostFindSourceBook {
    pub(crate) fn new() -> Self {
        Self {
            next_menu_sequence: AtomicU64::new(1),
            browsers: Mutex::new(BTreeMap::new()),
        }
    }

    fn alloc_menu_id(&self, browser: BrowserId) -> Result<ContextMenuRequestId, EngineError> {
        let sequence = self
            .next_menu_sequence
            .try_update(Ordering::AcqRel, Ordering::Acquire, |prev| {
                prev.checked_add(1)
            })
            .map_err(|_| EngineError::InvalidRequest)?;
        if sequence == 0 {
            return Err(EngineError::InvalidRequest);
        }
        Ok(ContextMenuRequestId { browser, sequence })
    }

    pub(crate) fn set_find_sink(&self, browser: BrowserId, sink: Option<Arc<dyn FindResultSink>>) {
        let old = {
            let mut map = self.browsers.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(sink) = sink {
                map.entry(browser).or_default().find.replace(sink)
            } else if let Some(entry) = map.get_mut(&browser) {
                let prev = entry.find.take();
                if entry.source.is_none() {
                    map.remove(&browser);
                }
                prev
            } else {
                None
            }
        };
        drop(old);
    }

    pub(crate) fn find_sink(&self, browser: BrowserId) -> Option<Arc<dyn FindResultSink>> {
        self.browsers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&browser)
            .and_then(|entry| entry.find.clone())
    }

    pub(crate) fn set_source_viewer(
        &self,
        browser: BrowserId,
        viewer: Option<Arc<dyn SourceViewSink>>,
    ) {
        let old = {
            let mut map = self.browsers.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(viewer) = viewer {
                map.entry(browser).or_default().source.replace(viewer)
            } else if let Some(entry) = map.get_mut(&browser) {
                let prev = entry.source.take();
                if entry.find.is_none() {
                    map.remove(&browser);
                }
                prev
            } else {
                None
            }
        };
        drop(old);
    }

    pub(crate) fn source_viewer(&self, browser: BrowserId) -> Option<Arc<dyn SourceViewSink>> {
        self.browsers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&browser)
            .and_then(|entry| entry.source.clone())
    }

    pub(crate) fn browser_closed(&self, browser: BrowserId) {
        let old = self
            .browsers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&browser);
        drop(old);
    }
}

fn map_lease_err(err: MenuLeaseError) -> EngineError {
    match err {
        MenuLeaseError::Retired => EngineError::Closed,
        _ => EngineError::InvalidRequest,
    }
}

fn host_context(
    binding: WindowBinding,
    binding_revision: u64,
    selection: &str,
) -> Arc<MenuContext> {
    Arc::new(MenuContext {
        binding,
        binding_revision,
        x: 0,
        y: 0,
        frame_id: String::new(),
        selection: selection.to_owned(),
        misspelled_word: String::new(),
        dictionary_suggestions: Vec::new(),
    })
}

fn menu_scope_for_host(
    registry: &Arc<UiRegistry>,
    binding: &Arc<crate::menu_bridge::Binding>,
    selection: &str,
) -> Result<Arc<Scope>, EngineError> {
    let stamp = binding.target.lease.capture().map_err(map_lease_err)?;
    let context = host_context(stamp.binding(), stamp.revision(), selection);
    Ok(Arc::new(Scope {
        browser: binding.browser,
        native_id: 0,
        frame_id: String::new(),
        context,
        lease: binding.target.lease.clone(),
        stamp,
        registry: Arc::downgrade(registry),
        registration: Arc::downgrade(binding),
        serial: binding.serial,
        epoch: binding.epoch.load(Ordering::Acquire),
        spelling_opening: 0,
        delivery: Arc::downgrade(&binding.target.delivery),
        target: TargetMenuContext::default(),
        synthesized_target: false,
    }))
}

impl UiRegistry {
    pub(crate) fn host_browser_alive(&self, browser: BrowserId) -> bool {
        self.lock_source_admission(browser).is_ok()
    }

    /// Checks liveness of a host-initiated operation routed through an existing
    /// menu-bridge registration without failing on unrelated context-menu `epoch`
    /// advances or `s.ready == false` focus changes.
    pub(crate) fn host_menu_scope_alive(&self, scope: &Scope) -> bool {
        if self.is_draining() || !self.host_browser_alive(scope.browser) {
            return false;
        }
        let Some(binding) = scope.registration.upgrade() else {
            return false;
        };
        if binding.retired.load(Ordering::Acquire) || binding.serial != scope.serial {
            return false;
        }
        scope
            .lease
            .snapshot()
            .is_ok_and(|snap| !snap.retired && !snap.faulted)
    }

    pub(crate) fn host_direct_find_alive(&self, browser: BrowserId) -> bool {
        !self.is_draining()
            && self.host_browser_alive(browser)
            && self.host_find_source.find_sink(browser).is_some()
    }

    pub(crate) fn host_direct_source_alive(&self, browser: BrowserId) -> bool {
        !self.is_draining()
            && self.host_browser_alive(browser)
            && self.host_find_source.source_viewer(browser).is_some()
    }

    /// Returns the valid main frame for an open browser on the CEF UI thread
    /// without requiring `frame.is_focused() != 0` (Find bars and host menus
    /// steal GPUI focus from the browser).
    pub(crate) fn host_browser_main_frame(&self, browser: BrowserId) -> Option<(Browser, Frame)> {
        if !self.host_browser_alive(browser) {
            return None;
        }
        let native = self.find_browser(browser)?;
        self.menu_audit().record();
        if native.is_valid() == 0 {
            return None;
        }
        self.menu_audit().record();
        let frame = native.main_frame()?;
        if frame.is_valid() == 0 {
            return None;
        }
        Some((native, frame))
    }

    pub(crate) fn set_host_find_sink(
        &self,
        browser: BrowserId,
        sink: Option<Arc<dyn FindResultSink>>,
    ) -> Result<(), EngineError> {
        if browser.runtime() != self.runtime_generation() {
            return Err(EngineError::WrongRuntime);
        }
        if sink.is_some() {
            if self.is_draining() {
                return Err(EngineError::Closing);
            }
            if !matches!(
                self.snapshot(browser).state,
                SessionState::Creating | SessionState::Alive
            ) {
                return Err(EngineError::Closed);
            }
        }
        self.host_find_source.set_find_sink(browser, sink);
        Ok(())
    }

    pub(crate) fn set_host_source_viewer(
        &self,
        browser: BrowserId,
        viewer: Option<Arc<dyn SourceViewSink>>,
    ) -> Result<(), EngineError> {
        if browser.runtime() != self.runtime_generation() {
            return Err(EngineError::WrongRuntime);
        }
        if viewer.is_some() {
            if self.is_draining() {
                return Err(EngineError::Closing);
            }
            if !matches!(
                self.snapshot(browser).state,
                SessionState::Creating | SessionState::Alive
            ) {
                return Err(EngineError::Closed);
            }
        }
        self.host_find_source.set_source_viewer(browser, viewer);
        Ok(())
    }

    fn resolve_host_find_target(
        self: &Arc<Self>,
        browser: BrowserId,
        search_text: &str,
    ) -> Result<(FindScope, Arc<dyn FindResultSink>), EngineError> {
        if let Some(binding) = self.menu_bridge.get(browser)
            && !binding.retired.load(Ordering::Acquire)
            && let Some(sink) = binding.find.get()
        {
            let scope = menu_scope_for_host(self, &binding, search_text)?;
            return Ok((FindScope::HostMenu(scope), sink.clone()));
        }
        if let Some(sink) = self.host_find_source.find_sink(browser) {
            let context = host_context(WindowBinding { browser, window: 0 }, 1, search_text);
            return Ok((
                FindScope::HostDirect {
                    browser,
                    context,
                    registry: Arc::downgrade(self),
                },
                sink,
            ));
        }
        Err(EngineError::NotReady)
    }

    fn resolve_host_source_target(
        self: &Arc<Self>,
        browser: BrowserId,
    ) -> Result<(SourceScope, Arc<dyn SourceViewSink>), EngineError> {
        if let Some(binding) = self.menu_bridge.get(browser)
            && !binding.retired.load(Ordering::Acquire)
            && let Some(sink) = binding.source.get()
        {
            let scope = menu_scope_for_host(self, &binding, "")?;
            return Ok((SourceScope::HostMenu(scope), sink.clone()));
        }
        if let Some(sink) = self.host_find_source.source_viewer(browser) {
            let context = host_context(WindowBinding { browser, window: 0 }, 1, "");
            return Ok((
                SourceScope::HostDirect {
                    browser,
                    context,
                    registry: Arc::downgrade(self),
                },
                sink,
            ));
        }
        Err(EngineError::NotReady)
    }

    pub(crate) fn host_find(
        self: &Arc<Self>,
        browser: BrowserId,
        search_text: &str,
        forward: bool,
        match_case: bool,
        find_next: bool,
    ) -> Result<FindOperationId, EngineError> {
        if browser.runtime() != self.runtime_generation() {
            return Err(EngineError::WrongRuntime);
        }
        if search_text.is_empty() {
            return Err(EngineError::InvalidRequest);
        }
        let executor = self.executor().ok_or(EngineError::Closed)?;
        drop(self.lock_source_admission(browser)?);
        if !self.finds.configured() {
            return Err(EngineError::NotReady);
        }
        if self.host_find_unmanaged(browser) {
            return Err(EngineError::InvalidRequest);
        }
        let (scope, sink) = self.resolve_host_find_target(browser, search_text)?;
        let menu = self.host_find_source.alloc_menu_id(browser)?;
        if self.host_find_unmanaged(browser) {
            return Err(EngineError::InvalidRequest);
        }
        let data = {
            let _gate = self.lock_source_admission(browser)?;
            self.finds.begin_host(scope, menu, &sink, find_next)?
        };
        let receipt = find_receipt(Arc::downgrade(self), &data);
        let request = FindRequest {
            operation: data.id,
            menu,
            context: data.scope.context().clone(),
        };
        if let Err(e) = data.sink.started(request, receipt) {
            self.finds.rollback_unstarted(data.id);
            return Err(e);
        }
        if let Err(e) = executor.post(TaskPayload::HostFind {
            id: data.id,
            forward,
            match_case,
            find_next,
        }) {
            self.finds.failure(&data, FindFailure::Post(e.clone()));
            self.finds.finish(&data, FindOutcome::Failed);
            return Err(e);
        }
        Ok(data.id)
    }

    pub(crate) fn execute_host_find(
        &self,
        id: FindOperationId,
        forward: bool,
        match_case: bool,
        find_next: bool,
    ) {
        let _work = self.finds.work();
        let Some(data) = self.finds.data(id) else {
            return;
        };
        if self.is_draining()
            || !data.scope.operation_alive()
            || self.host_find_unmanaged(id.browser)
        {
            self.finds.finish(&data, FindOutcome::Cancelled);
            let _ = self.menu_wake();
            return;
        }
        let Some((native, _frame)) = self.host_browser_main_frame(id.browser) else {
            self.finds.finish(&data, FindOutcome::Cancelled);
            let _ = self.menu_wake();
            return;
        };
        if !self.finds.issue(&data) {
            self.finds.finish(&data, FindOutcome::Cancelled);
            let _ = self.menu_wake();
            return;
        }
        self.menu_audit().record();
        let Some(host) = native.host() else {
            self.finds.failure(&data, FindFailure::NativeUnavailable);
            self.finds.finish(&data, FindOutcome::Failed);
            let _ = self.menu_wake();
            return;
        };
        // CEF 156.0.31 forwards `findNext` to Chromium `FindTabHelper::StartFinding`'s
        // `find_match` flag (`0` counts without selecting an active match, leaving
        // `active_ordinal == 0`). Reset the session via `stop_finding(0)` when starting
        // a new search (`!continue_session`) and pass `1` so Match #1 is activated
        // immediately while follow-up Next/Prev steps advance within the active session.
        let continue_session = data.allow_same_session_id && find_next;
        if !continue_session {
            self.menu_audit().record();
            host.stop_finding(0);
        }
        let text = CefString::from(data.scope.context().selection.as_str());
        self.menu_audit().record();
        host.find(Some(&text), i32::from(forward), i32::from(match_case), 1);
        if let Err(e) = self.menu_wake() {
            self.finds.failure(&data, FindFailure::Wake(e));
        }
    }

    pub(crate) fn host_stop_finding(
        &self,
        browser: BrowserId,
        clear_selection: bool,
    ) -> Result<(), EngineError> {
        if browser.runtime() != self.runtime_generation() {
            return Err(EngineError::WrongRuntime);
        }
        let executor = self.executor().ok_or(EngineError::Closed)?;
        let cutoff_sequence = {
            let _gate = self.lock_source_admission(browser)?;
            self.finds.sequence_watermark_and_clear_session(browser)
        };
        executor
            .post(TaskPayload::HostStopFinding {
                browser,
                clear_selection,
                cutoff_sequence,
            })
            .map(|_| ())
    }

    pub(crate) fn execute_host_stop_finding(
        &self,
        browser: BrowserId,
        clear_selection: bool,
        cutoff_sequence: u64,
    ) {
        let _work = self.finds.work();
        if let Some(data) = self.finds.active(browser)
            && data.id.sequence <= cutoff_sequence
        {
            self.cancel_find_now_with_clear(data.id, clear_selection);
            return;
        }
        if self.finds.active(browser).is_none() {
            self.finds.clear_session_active(browser);
        }
        if self.finds.enrolled(browser)
            && let Some((native, _)) = self.host_browser_main_frame(browser)
        {
            self.menu_audit().record();
            if let Some(host) = native.host() {
                self.menu_audit().record();
                host.stop_finding(i32::from(clear_selection));
            }
        }
    }

    pub(crate) fn host_view_source(
        self: &Arc<Self>,
        browser: BrowserId,
    ) -> Result<SourceOperationId, EngineError> {
        if browser.runtime() != self.runtime_generation() {
            return Err(EngineError::WrongRuntime);
        }
        let executor = self.executor().ok_or(EngineError::Closed)?;
        drop(self.lock_source_admission(browser)?);
        if !self.sources.configured() {
            return Err(EngineError::NotReady);
        }
        let (scope, sink) = self.resolve_host_source_target(browser)?;
        let menu = self.host_find_source.alloc_menu_id(browser)?;
        let (id, _) = {
            let _gate = self.lock_source_admission(browser)?;
            self.sources.begin_host(menu, scope, &sink)?
        };
        let data = self.sources.data(id).ok_or(EngineError::Closed)?;
        let control = self.sources.control(id, &data);
        if let Err(e) = sink.started(id, menu, control) {
            self.sources.abort_unstarted(id);
            return Err(e);
        }
        if let Err(e) = executor.post(TaskPayload::HostViewSource { id }) {
            self.sources.fail(id, SourceFailure::Post(e.clone()));
            self.sources.unissued(id);
            return Err(e);
        }
        Ok(id)
    }

    pub(crate) fn execute_host_view_source(self: &Arc<Self>, id: SourceOperationId) {
        let Some(data) = self.sources.data(id) else {
            return;
        };
        if self.is_draining() || !data.scope.operation_alive() || self.sources.is_cancelled(id) {
            let _ = self
                .sources
                .cancel(Some(id.browser), data.scope.serial(), Some(id));
            self.sources.unissued(id);
            let _ = self.menu_wake();
            return;
        }
        let Some(limit) = self.sources.max_source_bytes() else {
            self.sources.fail(id, SourceFailure::Invariant);
            self.sources.unissued(id);
            let _ = self.menu_wake();
            return;
        };
        let Some((_native, frame)) = self.host_browser_main_frame(id.browser) else {
            let _ = self
                .sources
                .cancel(Some(id.browser), data.scope.serial(), Some(id));
            self.sources.unissued(id);
            let _ = self.menu_wake();
            return;
        };
        self.issue_host_visitor(id, limit, Arc::downgrade(self), &frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        executor::{Executor, NativeAudit},
        menu_bridge::ContextMenuConfig,
    };
    use rust_cef_core::{
        contracts::{BrowserConfig, RenderPreference, RuntimeGeneration, Viewport},
        find::{FindLimits, FindReceipt, FindResult, FindStatus},
        menu::MenuLimits,
        menu_context::MenuContextLimits,
        menu_lease::MenuLeaseOwner,
        ports::{
            BrowserEventEnvelope, BrowserEventSink, ContextMenuDeliverySink,
            ContextMenuResponseSink, ContextMenuTarget, FrameSink, MenuDeliveryError, OwnedFrame,
            PublishResult, UiDispatcher,
        },
        source_view::{SourceViewControl, SourceViewLimits, SourceViewRequest},
    };
    use std::num::NonZeroUsize;

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[derive(Default)]
    struct TestSinks {
        wakes: std::sync::atomic::AtomicUsize,
    }
    impl BrowserEventSink for TestSinks {
        fn publish(&self, _: BrowserEventEnvelope) -> PublishResult {
            PublishResult::Published
        }
    }
    impl FrameSink for TestSinks {
        fn publish(&self, _: OwnedFrame) -> PublishResult {
            PublishResult::Published
        }
    }
    impl UiDispatcher for TestSinks {
        fn wake(&self) -> Result<(), EngineError> {
            self.wakes.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    struct DummyDelivery;
    impl ContextMenuDeliverySink for DummyDelivery {
        fn capture(&self) -> Result<rust_cef_core::menu_lease::MenuLeaseStamp, MenuDeliveryError> {
            // Host-triggered operations capture via MenuLease directly even when UI menu presenter isn't ready.
            Err(MenuDeliveryError::Closed)
        }
        fn publish(
            &self,
            _: rust_cef_core::menu_context::MenuPresentation,
            _: Arc<dyn ContextMenuResponseSink>,
            _: rust_cef_core::menu_lease::MenuLeaseStamp,
        ) -> Result<(), MenuDeliveryError> {
            Ok(())
        }
        fn dismiss(&self, _: ContextMenuRequestId) -> Result<(), MenuDeliveryError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingFindSink {
        started: Mutex<Vec<FindRequest>>,
        finished: Mutex<Vec<FindStatus>>,
        reject_start: Mutex<Option<EngineError>>,
    }
    impl FindResultSink for RecordingFindSink {
        fn started(&self, req: FindRequest, _: Arc<dyn FindReceipt>) -> Result<(), EngineError> {
            if let Some(err) = self.reject_start.lock().unwrap().take() {
                return Err(err);
            }
            self.started.lock().unwrap().push(req);
            Ok(())
        }
        fn update(&self, _: FindResult) -> Result<(), EngineError> {
            Ok(())
        }
        fn finished(&self, status: FindStatus) -> Result<(), EngineError> {
            self.finished.lock().unwrap().push(status);
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingSourceSink {
        started: Mutex<Vec<(SourceOperationId, Arc<dyn SourceViewControl>)>>,
        published: Mutex<Vec<SourceViewRequest>>,
        closed: Mutex<Vec<SourceOperationId>>,
        reject_start: Mutex<Option<EngineError>>,
    }
    impl SourceViewSink for RecordingSourceSink {
        fn started(
            &self,
            id: SourceOperationId,
            _: ContextMenuRequestId,
            control: Arc<dyn SourceViewControl>,
        ) -> Result<(), EngineError> {
            if let Some(err) = self.reject_start.lock().unwrap().take() {
                return Err(err);
            }
            self.started.lock().unwrap().push((id, control));
            Ok(())
        }
        fn publish(
            &self,
            request: SourceViewRequest,
            _: Arc<dyn SourceViewControl>,
        ) -> Result<(), EngineError> {
            self.published.lock().unwrap().push(request);
            Ok(())
        }
        fn close(
            &self,
            id: SourceOperationId,
            _: Arc<dyn SourceViewControl>,
        ) -> Result<(), EngineError> {
            self.closed.lock().unwrap().push(id);
            Ok(())
        }
    }

    fn setup_registry(
        accept_posts: bool,
    ) -> (Arc<UiRegistry>, Arc<Mutex<Vec<TaskPayload>>>, Arc<Executor>) {
        let sinks = Arc::new(TestSinks::default());
        let registry = UiRegistry::new(
            RuntimeGeneration::new(1),
            sinks.clone(),
            sinks.clone(),
            sinks,
            NativeAudit::new(),
        );
        let executor = Executor::new(registry.runtime_generation(), &registry, NativeAudit::new());
        registry.attach_executor(&executor);
        let posted = Arc::new(Mutex::new(Vec::new()));
        let observed = posted.clone();
        executor.set_test_post(Arc::new(move |payload| {
            if accept_posts {
                observed.lock().unwrap().push(payload.clone());
            }
            accept_posts
        }));
        (registry, posted, executor)
    }

    fn add_alive_browser(registry: &UiRegistry, id: BrowserId) {
        registry
            .begin_create(
                id,
                BrowserConfig {
                    initial_url: "https://example.com".into(),
                    profile: None,
                    render_preference: RenderPreference::SoftwareRequired,
                    viewport: Viewport {
                        width: 640,
                        height: 480,
                        scale: 1.0,
                    },
                    options: Default::default(),
                },
            )
            .unwrap();
        registry.mark_alive_for_test(id);
    }

    #[test]
    fn find_rejects_empty_unconfigured_and_rolls_back_on_post_or_sink_error() {
        let (registry, posted, _exec) = setup_registry(true);
        let browser = BrowserId::new(RuntimeGeneration::new(1), 1);
        add_alive_browser(&registry, browser);

        assert_eq!(
            registry.host_find(browser, "", true, false, false),
            Err(EngineError::InvalidRequest)
        );
        assert_eq!(
            registry.host_find(browser, "needle", true, false, false),
            Err(EngineError::NotReady)
        );

        registry
            .configure_finds(FindLimits {
                capacity: nz(4),
                max_query_bytes: nz(16),
            })
            .unwrap();
        // No sink or menu binding registered yet.
        assert_eq!(
            registry.host_find(browser, "needle", true, false, false),
            Err(EngineError::NotReady)
        );

        let sink = Arc::new(RecordingFindSink::default());
        registry
            .set_host_find_sink(browser, Some(sink.clone()))
            .unwrap();
        assert_eq!(
            registry.host_find(browser, &"a".repeat(17), true, false, false),
            Err(EngineError::Backpressure)
        );

        // If sink rejects start, Finds rolls back cleanly so a subsequent find succeeds.
        *sink.reject_start.lock().unwrap() = Some(EngineError::Backpressure);
        assert_eq!(
            registry.host_find(browser, "needle", true, false, false),
            Err(EngineError::Backpressure)
        );
        assert_eq!(registry.finds.pending(), 0);
        assert!(registry.finds.snapshot().first_failure.is_none());

        let op = registry
            .host_find(browser, "needle", false, true, true)
            .unwrap();
        assert_eq!(op.browser, browser);
        assert_eq!(sink.started.lock().unwrap().len(), 1);
        assert_eq!(posted.lock().unwrap().len(), 1);
        assert!(matches!(
            posted.lock().unwrap()[0],
            TaskPayload::HostFind {
                id,
                forward: false,
                match_case: true,
                find_next: true
            } if id == op
        ));
        // Strictly serial: second overlapping find returns Backpressure.
        assert_eq!(
            registry.host_find(browser, "other", true, false, false),
            Err(EngineError::Backpressure)
        );

        registry.finds.browser_closed(browser);
        registry.host_find_source.browser_closed(browser);
        assert_eq!(registry.finds.pending(), 0);
    }

    #[test]
    fn find_executor_post_failure_finishes_started_sink_without_wedging_slot() {
        let (registry, _posted, _exec) = setup_registry(false);
        let browser = BrowserId::new(RuntimeGeneration::new(1), 2);
        add_alive_browser(&registry, browser);
        registry
            .configure_finds(FindLimits {
                capacity: nz(4),
                max_query_bytes: nz(32),
            })
            .unwrap();
        let sink = Arc::new(RecordingFindSink::default());
        registry
            .set_host_find_sink(browser, Some(sink.clone()))
            .unwrap();
        assert!(
            registry
                .host_find(browser, "abc", true, false, false)
                .is_err()
        );
        assert_eq!(sink.started.lock().unwrap().len(), 1);
        assert_eq!(sink.finished.lock().unwrap().len(), 1);
        assert_eq!(
            sink.finished.lock().unwrap()[0].outcome,
            Some(FindOutcome::Failed)
        );
        assert_eq!(registry.finds.pending(), 0);
    }

    #[test]
    fn host_menu_find_and_view_source_survive_menu_epoch_advances_without_delivery_capture() {
        let (registry, posted, _exec) = setup_registry(true);
        let browser = BrowserId::new(RuntimeGeneration::new(1), 3);
        add_alive_browser(&registry, browser);

        registry
            .configure_finds(FindLimits {
                capacity: nz(4),
                max_query_bytes: nz(64),
            })
            .unwrap();
        registry
            .configure_source_views(SourceViewLimits {
                capacity: nz(4),
                max_source_bytes: nz(4096),
            })
            .unwrap();
        registry
            .menu_bridge
            .configure(
                ContextMenuConfig {
                    capacity: nz(4),
                    model_limits: MenuLimits {
                        max_entries: nz(32),
                        max_label_bytes: nz(128),
                        max_total_label_bytes: nz(2048),
                    },
                    context_limits: MenuContextLimits {
                        max_frame_id_bytes: nz(128),
                        max_text_bytes: nz(128),
                        max_suggestions: nz(8),
                        max_total_bytes: nz(512),
                    },
                },
                || Ok(()),
            )
            .unwrap();

        let owner = MenuLeaseOwner::new(
            WindowBinding {
                browser,
                window: 99,
            },
            nz(4),
        )
        .unwrap();
        let target = ContextMenuTarget {
            lease: owner.lease(),
            delivery: Arc::new(DummyDelivery),
        };
        let (binding, old) = registry.menu_bridge.bind(browser, &target, true).unwrap();
        assert!(old.is_none());

        let find_sink = Arc::new(RecordingFindSink::default());
        let source_sink = Arc::new(RecordingSourceSink::default());
        registry
            .bind_find_results(&binding, find_sink.clone())
            .unwrap();
        registry
            .bind_source_viewer(&binding, source_sink.clone())
            .unwrap();

        let find_op = registry
            .host_find(browser, "hello", true, false, false)
            .unwrap();
        let src_op = registry.host_view_source(browser).unwrap();

        // Advance native menu epoch; host scopes remain alive until retired or browser closed.
        binding.advance();
        let find_data = registry.finds.data(find_op).unwrap();
        assert!(find_data.scope.operation_alive());
        let src_data = registry.sources.data(src_op).unwrap();
        assert!(src_data.scope.operation_alive());

        // Once retired, host scope reports dead.
        binding.retired.store(true, Ordering::Release);
        assert!(!find_data.scope.operation_alive());
        assert!(!src_data.scope.operation_alive());
        assert_eq!(posted.lock().unwrap().len(), 2);
    }

    mod host_find_tests {
        use super::*;

        #[derive(Default)]
        struct DirectFindSink {
            started: Mutex<Vec<FindRequest>>,
            updates: Mutex<Vec<FindResult>>,
            finished: Mutex<Vec<FindStatus>>,
        }
        impl FindResultSink for DirectFindSink {
            fn started(
                &self,
                req: FindRequest,
                _: Arc<dyn FindReceipt>,
            ) -> Result<(), EngineError> {
                self.started.lock().unwrap().push(req);
                Ok(())
            }
            fn update(&self, res: FindResult) -> Result<(), EngineError> {
                self.updates.lock().unwrap().push(res);
                Ok(())
            }
            fn finished(&self, status: FindStatus) -> Result<(), EngineError> {
                self.finished.lock().unwrap().push(status);
                Ok(())
            }
        }

        #[test]
        fn direct_find_quarantines_uncorrelated_filters_stale_and_posts_stop_finding() {
            let (registry, posted, _exec) = setup_registry(true);
            let browser = BrowserId::new(RuntimeGeneration::new(1), 10);
            add_alive_browser(&registry, browser);
            registry
                .configure_finds(FindLimits {
                    capacity: nz(4),
                    max_query_bytes: nz(64),
                })
                .unwrap();

            let sink = Arc::new(DirectFindSink::default());
            registry
                .set_host_find_sink(browser, Some(sink.clone()))
                .unwrap();

            // Search #1: direct sink delivery + highwater establishment (id = 10).
            let op1 = registry
                .host_find(browser, "alpha", true, false, false)
                .unwrap();
            let d1 = registry.finds.data(op1).unwrap();
            assert!(registry.finds.issue(&d1));
            let (_, r1, done1) = registry
                .finds
                .result(browser, 10, 3, 1, None, true)
                .unwrap();
            assert!(done1);
            sink.update(r1).unwrap();
            registry.finds.finish(&d1, FindOutcome::Result);
            assert_eq!(sink.updates.lock().unwrap().len(), 1);
            assert_eq!(
                sink.finished.lock().unwrap()[0].outcome,
                Some(FindOutcome::Result)
            );

            // Stop finding posts TaskPayload::HostStopFinding with clear_selection.
            registry.host_stop_finding(browser, true).unwrap();
            assert!(matches!(
                posted.lock().unwrap().last(),
                Some(TaskPayload::HostStopFinding {
                    browser: b,
                    clear_selection: true,
                    cutoff_sequence: 1,
                }) if *b == browser
            ));

            // Search #2: stale <= highwater (id = 10) is ignored; positive uncorrelated id quarantines.
            let op2 = registry
                .host_find(browser, "beta", true, true, false)
                .unwrap();
            let d2 = registry.finds.data(op2).unwrap();
            assert!(registry.finds.issue(&d2));
            assert!(
                registry
                    .finds
                    .result(browser, 10, 1, 1, None, false)
                    .is_none()
            );
            assert!(registry.finds.snapshot().late_results >= 1);

            let (_, r2, _) = registry
                .finds
                .result(browser, 11, 2, 1, None, false)
                .unwrap();
            sink.update(r2).unwrap();
            // Uncorrelated newer identifier (12 > 11) quarantines browser.
            assert!(
                registry
                    .finds
                    .result(browser, 12, 1, 1, None, true)
                    .is_none()
            );
            registry.finds.finish(&d2, FindOutcome::Failed);

            // Quarantined cursor rejects further host_find until OnBeforeClose.
            assert_eq!(
                registry.host_find(browser, "gamma", true, false, false),
                Err(EngineError::Backpressure)
            );
            registry.finds.browser_closed(browser);
            registry.host_find_source.browser_closed(browser);
        }

        #[test]
        fn host_find_next_reuses_same_session_id_and_clears_after_stop_or_zero_matches() {
            let (registry, _posted, _exec) = setup_registry(true);
            let browser = BrowserId::new(RuntimeGeneration::new(1), 11);
            add_alive_browser(&registry, browser);
            registry
                .configure_finds(FindLimits {
                    capacity: nz(4),
                    max_query_bytes: nz(64),
                })
                .unwrap();

            let sink = Arc::new(DirectFindSink::default());
            registry
                .set_host_find_sink(browser, Some(sink.clone()))
                .unwrap();

            // Search #1 completes with 3 matches at native identifier 10.
            let op1 = registry
                .host_find(browser, "Pro", true, false, false)
                .unwrap();
            let d1 = registry.finds.data(op1).unwrap();
            assert!(!d1.allow_same_session_id);
            assert!(registry.finds.issue(&d1));
            let (_, r1, done1) = registry
                .finds
                .result(browser, 10, 3, 1, None, true)
                .unwrap();
            assert!(done1);
            sink.update(r1).unwrap();
            registry.finds.finish(&d1, FindOutcome::Result);

            // Search #2 (find_next = true) allows same session identifier 10 and still rejects 9.
            let op2 = registry
                .host_find(browser, "Pro", true, false, true)
                .unwrap();
            let d2 = registry.finds.data(op2).unwrap();
            assert!(d2.allow_same_session_id);
            assert!(registry.finds.issue(&d2));
            assert!(
                registry
                    .finds
                    .result(browser, 9, 1, 1, None, false)
                    .is_none()
            );
            let (_, r2, done2) = registry
                .finds
                .result(browser, 10, 3, 2, None, true)
                .unwrap();
            assert!(done2);
            assert_eq!(r2.active_ordinal, 2);
            sink.update(r2).unwrap();
            registry.finds.finish(&d2, FindOutcome::Result);

            // execute_host_stop_finding clears active session so next find_next cannot reuse id 10.
            registry.execute_host_stop_finding(browser, true, u64::MAX);
            let op3 = registry
                .host_find(browser, "Pro", true, false, true)
                .unwrap();
            let d3 = registry.finds.data(op3).unwrap();
            assert!(!d3.allow_same_session_id);
            assert!(registry.finds.issue(&d3));
            assert!(
                registry
                    .finds
                    .result(browser, 10, 3, 2, None, true)
                    .is_none()
            );
            let (_, r3, done3) = registry
                .finds
                .result(browser, 11, 0, 0, None, true)
                .unwrap();
            assert!(done3);
            sink.update(r3).unwrap();
            registry.finds.finish(&d3, FindOutcome::Result);

            // Zero-match completion does not keep an active session for find_next.
            let op4 = registry
                .host_find(browser, "Pro", true, false, true)
                .unwrap();
            let d4 = registry.finds.data(op4).unwrap();
            assert!(!d4.allow_same_session_id);
            assert!(registry.finds.issue(&d4));
            assert!(
                registry
                    .finds
                    .result(browser, 11, 1, 1, None, true)
                    .is_none()
            );
            let (_, r4, _) = registry
                .finds
                .result(browser, 12, 1, 1, None, true)
                .unwrap();
            sink.update(r4).unwrap();
            registry.finds.finish(&d4, FindOutcome::Result);

            registry.finds.browser_closed(browser);
            registry.host_find_source.browser_closed(browser);
        }

        #[test]
        fn host_stop_finding_does_not_cancel_subsequent_host_find() {
            let (registry, posted, _exec) = setup_registry(true);
            let browser = BrowserId::new(RuntimeGeneration::new(1), 12);
            add_alive_browser(&registry, browser);
            registry
                .configure_finds(FindLimits {
                    capacity: nz(4),
                    max_query_bytes: nz(64),
                })
                .unwrap();

            let sink = Arc::new(DirectFindSink::default());
            registry
                .set_host_find_sink(browser, Some(sink.clone()))
                .unwrap();

            // Search #1 finishes with an active session on native id 10.
            let op1 = registry
                .host_find(browser, "SEARCH", true, false, false)
                .unwrap();
            let d1 = registry.finds.data(op1).unwrap();
            assert!(registry.finds.issue(&d1));
            let (_, r1, _) = registry
                .finds
                .result(browser, 10, 3, 1, None, true)
                .unwrap();
            sink.update(r1).unwrap();
            registry.finds.finish(&d1, FindOutcome::Result);

            // Host posts stop_finding and immediately admits Search #2 before UI executes stop_finding.
            registry.host_stop_finding(browser, true).unwrap();
            let cutoff_sequence = match posted.lock().unwrap().last().cloned() {
                Some(TaskPayload::HostStopFinding {
                    cutoff_sequence, ..
                }) => cutoff_sequence,
                other => panic!("unexpected payload: {other:?}"),
            };
            assert_eq!(cutoff_sequence, op1.sequence);

            let op2 = registry
                .host_find(browser, "SEARCH_TOKEN_ALPHA", true, false, true)
                .unwrap();
            assert!(op2.sequence > cutoff_sequence);
            let d2 = registry.finds.data(op2).unwrap();
            // Synchronous clear_session_active on host_stop_finding prevents stale session reuse.
            assert!(!d2.allow_same_session_id);

            // When UI thread pops HostStopFinding ahead of HostFind(op2), op2 stays intact.
            registry.execute_host_stop_finding(browser, true, cutoff_sequence);
            assert!(registry.finds.data(op2).is_some());
            assert!(registry.finds.issue(&d2));
            let (_, r2, done2) = registry
                .finds
                .result(browser, 11, 2, 1, None, true)
                .unwrap();
            assert!(done2);
            assert_eq!(r2.matches, 2);
            assert_eq!(r2.active_ordinal, 1);
            sink.update(r2).unwrap();
            registry.finds.finish(&d2, FindOutcome::Result);

            registry.finds.browser_closed(browser);
            registry.host_find_source.browser_closed(browser);
        }
    }

    mod host_source_view_tests {
        use super::*;
        use rust_cef_core::source_view::SourceText;

        #[test]
        fn host_view_source_rejects_closed_unbound_and_records_budget_failure() {
            let (registry, posted, _exec) = setup_registry(true);
            let unknown = BrowserId::new(RuntimeGeneration::new(1), 404);
            let browser = BrowserId::new(RuntimeGeneration::new(1), 20);
            add_alive_browser(&registry, browser);

            // Wrong runtime / closed / unconfigured / unbound checks.
            let foreign = BrowserId::new(RuntimeGeneration::new(99), 1);
            assert_eq!(
                registry.host_view_source(foreign),
                Err(EngineError::WrongRuntime)
            );
            assert_eq!(registry.host_view_source(unknown), Err(EngineError::Closed));
            assert_eq!(
                registry.host_view_source(browser),
                Err(EngineError::NotReady)
            );

            registry
                .configure_source_views(SourceViewLimits {
                    capacity: nz(4),
                    max_source_bytes: nz(16),
                })
                .unwrap();
            // Configured but no viewer registered yet -> NotReady.
            assert_eq!(
                registry.host_view_source(browser),
                Err(EngineError::NotReady)
            );

            let viewer = Arc::new(RecordingSourceSink::default());
            registry
                .set_host_source_viewer(browser, Some(viewer.clone()))
                .unwrap();

            let op = registry.host_view_source(browser).unwrap();
            assert_eq!(viewer.started.lock().unwrap().len(), 1);
            assert!(matches!(
                posted.lock().unwrap().last(),
                Some(TaskPayload::HostViewSource { id }) if *id == op
            ));

            // SourceText rejects payloads exceeding SourceViewLimits::max_source_bytes via SourceFailure::Budget.
            assert_eq!(
                SourceText::new("<html>too long payload</html>".into(), nz(16)),
                Err(SourceFailure::Budget)
            );
            assert!(registry.sources.received(op));
            registry.sources.fail(op, SourceFailure::Budget);
            registry.sources.released(op);
            let diag = registry.sources.snapshot();
            assert_eq!(diag.first_failure, Some((op, SourceFailure::Budget)));
            assert_eq!(diag.pending, 0);

            // Valid SourceText publishes and closes cleanly via HostDirect control.
            let op_ok = registry.host_view_source(browser).unwrap();
            assert!(registry.sources.received(op_ok));
            let data = registry.sources.begin_publish(op_ok, 13).unwrap();
            let text = SourceText::new("<html></html>".into(), nz(16)).unwrap();
            let control = registry.sources.control(op_ok, &data);
            viewer.published.lock().unwrap().push(SourceViewRequest {
                operation: op_ok,
                menu: ContextMenuRequestId {
                    browser,
                    sequence: 1,
                },
                context: data.scope.context().clone(),
                source: text,
            });
            control.presented().unwrap();
            control.closed().unwrap();
            registry.sources.released(op_ok);
            assert_eq!(registry.sources.snapshot().pending, 0);
        }
    }
}
