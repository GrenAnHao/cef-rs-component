//! Profile-scoped and Frame-scoped asynchronous `CefURLRequest` adapter.
//!
//! Structural reference (`D:\System Dir\Downloads\CefSharp`, rev `41919703`):
//! - `CefSharp.Core.Runtime/UrlRequest.h:26-73`
//! - `CefSharp.Core.Runtime/Internals/CefFrameWrapper.cpp:402-416`
//! - `CefSharp.Core.Runtime/Internals/CefUrlRequestClientAdapter.cpp:14-45`
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use cef::*;
use rust_cef_core::contracts::{BrowserId, RuntimeGeneration};
use rust_cef_core::host_capabilities::RedactedSecret;
use rust_cef_core::ports::{PolicyAction, UiDispatcher};
use rust_cef_core::url_request::{
    MAX_URL_REQUEST_HEADER_BYTES, UrlRequestError, UrlRequestFlags, UrlRequestId, UrlRequestLedger,
    UrlRequestProgress, UrlRequestResponse, UrlRequestSpec, UrlRequestStatus, UrlRequestTarget,
};

use crate::executor::{Executor, NativeAudit, TaskPayload};
use crate::policy_native::NativePolicy;
use crate::profiles::ProfileManager;
use crate::registry::UiRegistry;

const MAX_RESPONSE_HEADERS: usize = 128;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UrlRequestState {
    Pending(UrlRequestProgress),
    Completed(UrlRequestResponse),
    Failed(UrlRequestError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UrlRequestAuthChallenge {
    pub is_proxy: bool,
    pub host: String,
    pub port: u16,
    pub realm: String,
    pub scheme: String,
}

pub type UrlRequestAuthResolver =
    Arc<dyn Fn(&UrlRequestAuthChallenge) -> Option<(String, RedactedSecret)> + Send + Sync>;

#[derive(Clone)]
pub struct UrlRequestTicket {
    id: UrlRequestId,
    state: Arc<Mutex<UrlRequestState>>,
    manager: Weak<UrlRequestManager>,
    executor: Weak<Executor>,
}

impl UrlRequestTicket {
    pub fn id(&self) -> UrlRequestId {
        self.id
    }

    pub fn poll(&self) -> UrlRequestState {
        lock(&self.state).clone()
    }

    pub fn cancel(&self) -> bool {
        let (Some(manager), Some(executor)) = (self.manager.upgrade(), self.executor.upgrade())
        else {
            return false;
        };
        manager.request_cancel(self.id, &executor)
    }
}

impl fmt::Debug for UrlRequestTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UrlRequestTicket")
            .field("id", &self.id)
            .field("state", &self.poll())
            .finish()
    }
}

pub(crate) fn build_cef_urlrequest_flags(flags: UrlRequestFlags) -> i32 {
    let mut bits = 0i32;
    if flags.skip_cache {
        bits |= cef::sys::cef_urlrequest_flags_t::UR_FLAG_SKIP_CACHE.0;
    }
    if flags.allow_stored_credentials {
        bits |= cef::sys::cef_urlrequest_flags_t::UR_FLAG_ALLOW_STORED_CREDENTIALS.0;
    }
    if flags.report_upload_progress {
        bits |= cef::sys::cef_urlrequest_flags_t::UR_FLAG_REPORT_UPLOAD_PROGRESS.0;
    }
    if flags.no_download_data {
        bits |= cef::sys::cef_urlrequest_flags_t::UR_FLAG_NO_DOWNLOAD_DATA.0;
    }
    if flags.no_retry_on_5xx {
        bits |= cef::sys::cef_urlrequest_flags_t::UR_FLAG_NO_RETRY_ON_5XX.0;
    }
    if flags.stop_on_redirect {
        bits |= cef::sys::cef_urlrequest_flags_t::UR_FLAG_STOP_ON_REDIRECT.0;
    }
    bits
}

fn cef_status_to_core(status: UrlrequestStatus) -> UrlRequestStatus {
    if status == UrlrequestStatus::SUCCESS {
        UrlRequestStatus::Success
    } else if status == UrlrequestStatus::IO_PENDING {
        UrlRequestStatus::IoPending
    } else if status == UrlrequestStatus::CANCELED {
        UrlRequestStatus::Canceled
    } else if status == UrlrequestStatus::FAILED {
        UrlRequestStatus::Failed
    } else {
        UrlRequestStatus::Unknown
    }
}

struct ActiveUrlRequestEntry {
    target_browser: Option<BrowserId>,
    max_response_bytes: usize,
    stop_on_redirect: bool,
    initial_url: String,
    state: Arc<Mutex<UrlRequestState>>,
    body: Vec<u8>,
    native: Option<Urlrequest>,
    cancel_requested: bool,
    timed_out: bool,
    policy_denied: bool,
    too_large: bool,
    auth_attempted: bool,
}

pub(crate) struct UrlRequestManager {
    runtime_generation: RuntimeGeneration,
    ledger: Mutex<UrlRequestLedger>,
    pending: Arc<AtomicU64>,
    entries: Mutex<BTreeMap<UrlRequestId, ActiveUrlRequestEntry>>,
    policy: Arc<NativePolicy>,
    profiles: Weak<ProfileManager>,
    ui: Arc<dyn UiDispatcher>,
    audit: Arc<NativeAudit>,
    auth_resolver: Mutex<Option<UrlRequestAuthResolver>>,
}

impl UrlRequestManager {
    pub(crate) fn new(
        runtime_generation: RuntimeGeneration,
        policy: Arc<NativePolicy>,
        profiles: &Arc<ProfileManager>,
        ui: Arc<dyn UiDispatcher>,
        audit: Arc<NativeAudit>,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime_generation,
            ledger: Mutex::new(UrlRequestLedger::new()),
            pending: Arc::new(AtomicU64::new(0)),
            entries: Mutex::new(BTreeMap::new()),
            policy,
            profiles: Arc::downgrade(profiles),
            ui,
            audit,
            auth_resolver: Mutex::new(None),
        })
    }

    pub(crate) fn set_auth_resolver(&self, resolver: Option<UrlRequestAuthResolver>) {
        *lock(&self.auth_resolver) = resolver;
    }

    pub(crate) fn clear_auth_resolver(&self) {
        *lock(&self.auth_resolver) = None;
    }

    pub(crate) fn submit(
        self: &Arc<Self>,
        spec: UrlRequestSpec,
        executor: &Arc<Executor>,
        draining: bool,
    ) -> Result<UrlRequestTicket, UrlRequestError> {
        spec.validate()
            .map_err(|_| UrlRequestError::InvalidRequest)?;
        let target_browser = match spec.target {
            UrlRequestTarget::DefaultProfile => None,
            UrlRequestTarget::Profile(handle) => {
                if handle.runtime() != self.runtime_generation {
                    return Err(UrlRequestError::InvalidRequest);
                }
                None
            }
            UrlRequestTarget::BrowserMainFrame(browser) => {
                if browser.runtime() != self.runtime_generation {
                    return Err(UrlRequestError::InvalidRequest);
                }
                Some(browser)
            }
        };
        if draining {
            return Err(UrlRequestError::Closing);
        }
        if !self.policy.rules.allows_network(&spec.url) {
            self.policy.deny(target_browser, PolicyAction::Resource);
            return Err(UrlRequestError::PolicyDenied);
        }

        let id = {
            let mut ledger = lock(&self.ledger);
            ledger.admit()?
        };

        let state = Arc::new(Mutex::new(UrlRequestState::Pending(UrlRequestProgress {
            id,
            uploaded_bytes: 0,
            upload_total: -1,
            downloaded_bytes: 0,
            download_total: -1,
        })));

        {
            let mut entries = lock(&self.entries);
            entries.insert(
                id,
                ActiveUrlRequestEntry {
                    target_browser,
                    max_response_bytes: spec.max_response_bytes,
                    stop_on_redirect: spec.flags.stop_on_redirect,
                    initial_url: spec.url.clone(),
                    state: state.clone(),
                    body: Vec::new(),
                    native: None,
                    cancel_requested: false,
                    timed_out: false,
                    policy_denied: false,
                    too_large: false,
                    auth_attempted: false,
                },
            );
        }
        self.pending.fetch_add(1, Ordering::AcqRel);

        let timeout_ms = spec.timeout_ms;
        if executor
            .post(TaskPayload::StartUrlRequest { id, spec })
            .is_err()
        {
            self.rollback_submission(id, UrlRequestError::Closing);
            return Err(UrlRequestError::Closing);
        }

        let delay_ms = i64::try_from(timeout_ms).unwrap_or(i64::MAX);
        let _ = executor.post_delayed(TaskPayload::TimeoutUrlRequest { id }, delay_ms);

        Ok(UrlRequestTicket {
            id,
            state,
            manager: Arc::downgrade(self),
            executor: Arc::downgrade(executor),
        })
    }

    fn rollback_submission(&self, id: UrlRequestId, error: UrlRequestError) {
        let entry = lock(&self.entries).remove(&id);
        let _ = lock(&self.ledger).complete(id);
        if let Some(entry) = entry {
            *lock(&entry.state) = UrlRequestState::Failed(error);
            self.pending.fetch_sub(1, Ordering::AcqRel);
        }
    }

    pub(crate) fn request_cancel(&self, id: UrlRequestId, executor: &Arc<Executor>) -> bool {
        let marked = {
            let mut ledger = lock(&self.ledger);
            if !ledger.mark_cancelling(id) {
                return false;
            }
            true
        };
        if let Some(entry) = lock(&self.entries).get_mut(&id) {
            entry.cancel_requested = true;
        }
        let _ = executor.post_while_draining(TaskPayload::CancelUrlRequest { id });
        marked
    }

    pub(crate) fn run_start(
        self: &Arc<Self>,
        on_ui: bool,
        id: UrlRequestId,
        spec: UrlRequestSpec,
        registry: &UiRegistry,
    ) {
        if !on_ui {
            self.finish_before_native(id, UrlRequestError::Closing);
            return;
        }
        let already_cancelled_or_draining = {
            let entries = lock(&self.entries);
            let ledger = lock(&self.ledger);
            ledger.is_draining() || entries.get(&id).is_some_and(|entry| entry.cancel_requested)
        };
        if already_cancelled_or_draining {
            self.finish_before_native(id, UrlRequestError::Cancelled);
            return;
        }

        if !self.policy.rules.allows_network(&spec.url) {
            self.policy.deny(
                match spec.target {
                    UrlRequestTarget::BrowserMainFrame(b) => Some(b),
                    _ => None,
                },
                PolicyAction::Resource,
            );
            self.finish_before_native(id, UrlRequestError::PolicyDenied);
            return;
        }

        let Some(mut request) = build_cef_request(&spec) else {
            self.finish_before_native(
                id,
                UrlRequestError::Failed {
                    error_code: Errorcode::FAILED.get_raw(),
                },
            );
            return;
        };

        let mut client = RuntimeUrlRequestClient::new_client(id, Arc::downgrade(self));
        let created = match spec.target {
            UrlRequestTarget::DefaultProfile => {
                let Some(profiles) = self.profiles.upgrade() else {
                    self.finish_before_native(id, UrlRequestError::NotReady);
                    return;
                };
                if !profiles.default_ready() {
                    self.finish_before_native(id, UrlRequestError::NotReady);
                    return;
                }
                let Some(mut ctx) = profiles.context_for(profiles.default_handle()) else {
                    self.finish_before_native(id, UrlRequestError::NotReady);
                    return;
                };
                self.audit.record();
                urlrequest_create(Some(&mut request), Some(&mut client), Some(&mut ctx))
            }
            UrlRequestTarget::Profile(handle) => {
                let Some(profiles) = self.profiles.upgrade() else {
                    self.finish_before_native(id, UrlRequestError::NotReady);
                    return;
                };
                if profiles.select(Some(handle)).is_err() {
                    self.finish_before_native(id, UrlRequestError::NotReady);
                    return;
                }
                let Some(mut ctx) = profiles.context_for(handle) else {
                    self.finish_before_native(id, UrlRequestError::NotReady);
                    return;
                };
                self.audit.record();
                urlrequest_create(Some(&mut request), Some(&mut client), Some(&mut ctx))
            }
            UrlRequestTarget::BrowserMainFrame(browser) => {
                let Some(native_browser) = registry.ready_browser_native(browser) else {
                    self.finish_before_native(id, UrlRequestError::NotReady);
                    return;
                };
                let Some(frame) = native_browser.main_frame() else {
                    self.finish_before_native(id, UrlRequestError::NotReady);
                    return;
                };
                self.audit.record();
                frame.create_urlrequest(Some(&mut request), Some(&mut client))
            }
        };

        let Some(native) = created else {
            self.finish_before_native(
                id,
                UrlRequestError::Failed {
                    error_code: Errorcode::FAILED.get_raw(),
                },
            );
            return;
        };

        let mut cancel_now: Option<Urlrequest> = None;
        {
            let mut entries = lock(&self.entries);
            let draining = lock(&self.ledger).is_draining();
            if let Some(entry) = entries.get_mut(&id) {
                if entry.cancel_requested || draining {
                    cancel_now = Some(native.clone());
                }
                entry.native = Some(native);
            } else {
                // `on_request_complete` already fired synchronously inside creation.
                drop(entries);
                drop(native);
                return;
            }
        }
        if let Some(to_cancel) = cancel_now {
            self.audit.record();
            to_cancel.cancel();
        }
    }

    pub(crate) fn run_cancel(&self, on_ui: bool, id: UrlRequestId, timed_out: bool) {
        if !on_ui {
            return;
        }
        let native = {
            let mut entries = lock(&self.entries);
            let Some(entry) = entries.get_mut(&id) else {
                return;
            };
            entry.cancel_requested = true;
            if timed_out {
                entry.timed_out = true;
            }
            entry.native.clone()
        };
        if let Some(native) = native {
            let _ = lock(&self.ledger).mark_cancelling(id);
            self.audit.record();
            native.cancel();
        }
    }

    pub(crate) fn drain_on_ui(&self, on_ui: bool) {
        let ids = lock(&self.ledger).begin_drain();
        if ids.is_empty() && lock(&self.entries).is_empty() {
            return;
        }
        let mut to_cancel = Vec::new();
        {
            let mut entries = lock(&self.entries);
            for entry in entries.values_mut() {
                entry.cancel_requested = true;
                if let Some(native) = &entry.native {
                    to_cancel.push(native.clone());
                }
            }
        }
        if on_ui {
            for native in to_cancel {
                self.audit.record();
                native.cancel();
            }
        }
    }

    /// Abandons all remaining native handles on process-exit-required teardown drop.
    pub(crate) fn abandon_all(&self) {
        self.clear_auth_resolver();
        let entries = std::mem::take(&mut *lock(&self.entries));
        for (_, mut entry) in entries {
            if let Some(native) = entry.native.take() {
                std::mem::forget(native);
            }
        }
    }

    pub(crate) fn pending_count(&self) -> u64 {
        self.pending.load(Ordering::Acquire)
    }

    pub(crate) fn is_clear(&self) -> bool {
        self.pending_count() == 0 && lock(&self.entries).is_empty() && lock(&self.ledger).is_empty()
    }

    fn finish_before_native(&self, id: UrlRequestId, error: UrlRequestError) {
        let entry = lock(&self.entries).remove(&id);
        let _ = lock(&self.ledger).complete(id);
        if let Some(entry) = entry {
            *lock(&entry.state) = UrlRequestState::Failed(error);
            self.pending.fetch_sub(1, Ordering::AcqRel);
            let _ = self.ui.wake();
        }
    }

    fn on_upload_progress(&self, id: UrlRequestId, current: i64, total: i64) {
        let state_cell = {
            let entries = lock(&self.entries);
            entries.get(&id).map(|e| e.state.clone())
        };
        if let Some(cell) = state_cell {
            let mut state = lock(&cell);
            if let UrlRequestState::Pending(progress) = &mut *state {
                progress.uploaded_bytes = current;
                progress.upload_total = total;
            }
        }
    }

    fn on_download_progress(
        &self,
        id: UrlRequestId,
        current: i64,
        total: i64,
        observed_url: Option<&str>,
    ) -> Option<Urlrequest> {
        let mut cancel_target = None;
        let state_cell = {
            let mut entries = lock(&self.entries);
            let Some(entry) = entries.get_mut(&id) else {
                return None;
            };
            if let Some(url) = observed_url
                && !url.is_empty()
                && ((entry.stop_on_redirect && url != entry.initial_url)
                    || !self.policy.rules.allows_network(url))
            {
                if !self.policy.rules.allows_network(url) {
                    entry.policy_denied = true;
                    self.policy
                        .deny(entry.target_browser, PolicyAction::Redirect);
                }
                entry.cancel_requested = true;
                entry.body.clear();
                cancel_target = entry.native.clone();
            } else if total > 0 && (total as usize) > entry.max_response_bytes {
                entry.too_large = true;
                entry.cancel_requested = true;
                entry.body.clear();
                cancel_target = entry.native.clone();
            }
            entry.state.clone()
        };
        {
            let mut state = lock(&state_cell);
            if let UrlRequestState::Pending(progress) = &mut *state {
                progress.downloaded_bytes = current;
                progress.download_total = total;
            }
        }
        cancel_target
    }

    fn on_download_data(
        &self,
        id: UrlRequestId,
        bytes: &[u8],
        observed_url: Option<&str>,
    ) -> Option<Urlrequest> {
        let mut cancel_target = None;
        let state_cell = {
            let mut entries = lock(&self.entries);
            let Some(entry) = entries.get_mut(&id) else {
                return None;
            };
            if entry.policy_denied || entry.too_large || entry.cancel_requested {
                return None;
            }
            if let Some(url) = observed_url
                && !url.is_empty()
                && ((entry.stop_on_redirect && url != entry.initial_url)
                    || !self.policy.rules.allows_network(url))
            {
                if !self.policy.rules.allows_network(url) {
                    entry.policy_denied = true;
                    self.policy
                        .deny(entry.target_browser, PolicyAction::Redirect);
                }
                entry.cancel_requested = true;
                entry.body.clear();
                cancel_target = entry.native.clone();
                return cancel_target;
            }
            let Some(next_len) = entry.body.len().checked_add(bytes.len()) else {
                entry.too_large = true;
                entry.cancel_requested = true;
                entry.body.clear();
                return entry.native.clone();
            };
            if next_len > entry.max_response_bytes {
                entry.too_large = true;
                entry.cancel_requested = true;
                entry.body.clear();
                cancel_target = entry.native.clone();
            } else {
                entry.body.extend_from_slice(bytes);
            }
            entry.state.clone()
        };
        let downloaded = lock(&self.entries)
            .get(&id)
            .map_or(0, |e| e.body.len() as i64);
        let mut state = lock(&state_cell);
        if let UrlRequestState::Pending(progress) = &mut *state {
            progress.downloaded_bytes = progress.downloaded_bytes.max(downloaded);
        }
        cancel_target
    }

    fn resolve_auth_once(
        &self,
        id: UrlRequestId,
        challenge: &UrlRequestAuthChallenge,
    ) -> Option<(String, RedactedSecret)> {
        let should_try = {
            let mut entries = lock(&self.entries);
            let Some(entry) = entries.get_mut(&id) else {
                return None;
            };
            if entry.auth_attempted || entry.cancel_requested {
                return None;
            }
            entry.auth_attempted = true;
            true
        };
        if !should_try {
            return None;
        }
        let resolver = lock(&self.auth_resolver).clone()?;
        resolver(challenge)
    }

    fn on_request_complete(
        &self,
        id: UrlRequestId,
        status: UrlRequestStatus,
        error_code: i32,
        http_status: Option<i32>,
        final_url: String,
        headers: Vec<(String, String)>,
        response_was_cached: bool,
    ) {
        let entry = lock(&self.entries).remove(&id);
        let _ = lock(&self.ledger).complete(id);
        let Some(mut entry) = entry else {
            return;
        };
        let native = entry.native.take();
        let mut body = std::mem::take(&mut entry.body);

        let check_url = if final_url.is_empty() {
            entry.initial_url.as_str()
        } else {
            final_url.as_str()
        };
        let policy_blocked = entry.policy_denied || !self.policy.rules.allows_network(check_url);
        if policy_blocked {
            self.policy
                .deny(entry.target_browser, PolicyAction::Resource);
            body.clear();
        }

        let terminal = if policy_blocked {
            UrlRequestState::Failed(UrlRequestError::PolicyDenied)
        } else if entry.too_large {
            UrlRequestState::Failed(UrlRequestError::ResponseTooLarge)
        } else if entry.timed_out {
            UrlRequestState::Failed(UrlRequestError::Timeout)
        } else if entry.cancel_requested || status == UrlRequestStatus::Canceled {
            UrlRequestState::Failed(UrlRequestError::Cancelled)
        } else if status == UrlRequestStatus::Success {
            UrlRequestState::Completed(UrlRequestResponse {
                status,
                error_code,
                http_status,
                final_url,
                headers,
                body,
                response_was_cached,
            })
        } else {
            UrlRequestState::Failed(UrlRequestError::Failed { error_code })
        };

        *lock(&entry.state) = terminal;
        self.pending.fetch_sub(1, Ordering::AcqRel);
        // Drop native CefURLRequest outside all registry/entry locks.
        drop(native);
        let _ = self.ui.wake();
    }
}

fn build_cef_request(spec: &UrlRequestSpec) -> Option<Request> {
    let req = request_create()?;
    req.set_url(Some(&CefString::from(spec.url.as_str())));
    req.set_method(Some(&CefString::from(spec.method.as_str())));
    req.set_flags(build_cef_urlrequest_flags(spec.flags));

    let mut has_content_type = false;
    for (name, value) in &spec.headers {
        if name.eq_ignore_ascii_case("content-type") {
            has_content_type = true;
        }
        req.set_header_by_name(
            Some(&CefString::from(name.as_str())),
            Some(&CefString::from(value.as_str())),
            1,
        );
    }
    if let Some(ct) = &spec.upload_content_type
        && !has_content_type
    {
        req.set_header_by_name(
            Some(&CefString::from("Content-Type")),
            Some(&CefString::from(ct.as_str())),
            1,
        );
    }
    if let Some(body) = &spec.upload_body {
        let mut post_data = post_data_create()?;
        if !body.is_empty() {
            let mut element = post_data_element_create()?;
            element.set_to_bytes(body.len(), body.as_ptr());
            if post_data.add_element(Some(&mut element)) == 0 {
                return None;
            }
        }
        req.set_post_data(Some(&mut post_data));
    }
    Some(req)
}

fn extract_response_metadata(url_req: &Urlrequest) -> (Option<i32>, String, Vec<(String, String)>) {
    let req_url = url_req
        .request()
        .map(|r| CefString::from(&r.url()).to_string())
        .unwrap_or_default();
    let Some(resp) = url_req.response() else {
        return (None, req_url, Vec::new());
    };
    let status = resp.status();
    let http_status = (100..=599).contains(&status).then_some(status);
    let resp_url = CefString::from(&resp.url()).to_string();
    let final_url = if resp_url.is_empty() {
        req_url
    } else {
        resp_url
    };

    let mut multimap = CefStringMultimap::new();
    resp.header_map(Some(&mut multimap));
    let mut headers = Vec::new();
    for (name, values) in multimap {
        for value in values {
            if headers.len() >= MAX_RESPONSE_HEADERS {
                return (http_status, final_url, headers);
            }
            if name.len().saturating_add(value.len()) <= MAX_URL_REQUEST_HEADER_BYTES {
                headers.push((name.clone(), value));
            }
        }
    }
    (http_status, final_url, headers)
}

fn observed_response_url(url_req: &Urlrequest) -> Option<String> {
    if let Some(resp) = url_req.response() {
        let url = CefString::from(&resp.url()).to_string();
        if !url.is_empty() {
            return Some(url);
        }
    }
    url_req
        .request()
        .map(|r| CefString::from(&r.url()).to_string())
        .filter(|s| !s.is_empty())
}

wrap_urlrequest_client! {
    struct RuntimeUrlRequestClient {
        id: UrlRequestId,
        manager: Weak<UrlRequestManager>,
        completed: Arc<AtomicBool>,
    }

    impl UrlrequestClient {
        fn on_upload_progress(&self, _request: Option<&mut Urlrequest>, current: i64, total: i64) {
            if let Some(manager) = self.manager.upgrade() {
                manager.on_upload_progress(self.id, current, total);
            }
        }

        fn on_download_progress(&self, request: Option<&mut Urlrequest>, current: i64, total: i64) {
            let Some(manager) = self.manager.upgrade() else {
                return;
            };
            let observed = request.as_deref().and_then(observed_response_url);
            if let Some(cancel) =
                manager.on_download_progress(self.id, current, total, observed.as_deref())
            {
                cancel.cancel();
            }
        }

        fn on_download_data(
            &self,
            request: Option<&mut Urlrequest>,
            data: *const u8,
            data_length: usize,
        ) {
            if data.is_null() || data_length == 0 {
                return;
            }
            let Some(manager) = self.manager.upgrade() else {
                return;
            };
            // SAFETY: CEF guarantees `data` points to `data_length` readable bytes for callback scope.
            let slice = unsafe { std::slice::from_raw_parts(data, data_length) };
            let observed = request.as_deref().and_then(observed_response_url);
            if let Some(cancel) = manager.on_download_data(self.id, slice, observed.as_deref()) {
                cancel.cancel();
            }
        }

        fn auth_credentials(
            &self,
            is_proxy: ::std::os::raw::c_int,
            host: Option<&CefString>,
            port: ::std::os::raw::c_int,
            realm: Option<&CefString>,
            scheme: Option<&CefString>,
            callback: Option<&mut AuthCallback>,
        ) -> ::std::os::raw::c_int {
            let (Some(manager), Some(callback)) = (self.manager.upgrade(), callback) else {
                return 0;
            };
            let challenge = UrlRequestAuthChallenge {
                is_proxy: is_proxy != 0,
                host: host.map(ToString::to_string).unwrap_or_default(),
                port: u16::try_from(port).unwrap_or(0),
                realm: realm.map(ToString::to_string).unwrap_or_default(),
                scheme: scheme.map(ToString::to_string).unwrap_or_default(),
            };
            if let Some((username, password)) = manager.resolve_auth_once(self.id, &challenge) {
                let user = CefString::from(username.as_str());
                let pass = CefString::from(password.expose());
                callback.cont(Some(&user), Some(&pass));
                1
            } else {
                0
            }
        }

        fn on_request_complete(&self, request: Option<&mut Urlrequest>) {
            if self.completed.swap(true, Ordering::AcqRel) {
                return;
            }
            let Some(manager) = self.manager.upgrade() else {
                return;
            };
            let (status, error_code, http_status, final_url, headers, cached) = match request {
                Some(req) => {
                    let status = cef_status_to_core(req.request_status());
                    let error_code = req.request_error().get_raw();
                    let (http_status, final_url, headers) = extract_response_metadata(req);
                    let cached = req.response_was_cached() != 0;
                    (status, error_code, http_status, final_url, headers, cached)
                }
                None => (
                    UrlRequestStatus::Failed,
                    Errorcode::FAILED.get_raw(),
                    None,
                    String::new(),
                    Vec::new(),
                    false,
                ),
            };
            manager.on_request_complete(
                self.id,
                status,
                error_code,
                http_status,
                final_url,
                headers,
                cached,
            );
        }
    }
}

impl RuntimeUrlRequestClient {
    #[allow(clippy::new_ret_no_self)]
    fn new_client(id: UrlRequestId, manager: Weak<UrlRequestManager>) -> UrlrequestClient {
        Self::new(id, manager, Arc::new(AtomicBool::new(false)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::CompiledPolicy;
    use rust_cef_core::contracts::{ProfileHandle, TrustedContentPolicy};
    use rust_cef_core::url_request::MAX_IN_FLIGHT_URL_REQUESTS;

    struct DummyDispatcher;
    impl UiDispatcher for DummyDispatcher {
        fn wake(&self) -> Result<(), rust_cef_core::ports::EngineError> {
            Ok(())
        }
    }

    struct DummySink;
    impl rust_cef_core::ports::BrowserEventSink for DummySink {
        fn publish(
            &self,
            _: rust_cef_core::ports::BrowserEventEnvelope,
        ) -> rust_cef_core::ports::PublishResult {
            rust_cef_core::ports::PublishResult::Published
        }
    }
    impl rust_cef_core::ports::FrameSink for DummySink {
        fn publish(
            &self,
            _: rust_cef_core::ports::OwnedFrame,
        ) -> rust_cef_core::ports::PublishResult {
            rust_cef_core::ports::PublishResult::Published
        }
    }

    fn test_policy() -> Arc<NativePolicy> {
        let compiled = Arc::new(
            CompiledPolicy::compile(&TrustedContentPolicy {
                allowed_origins: vec!["https://allowed.example".into()],
                allow_embedded_data: false,
            })
            .unwrap(),
        );
        NativePolicy::new(compiled, Weak::new())
    }

    #[test]
    fn url_request_flags_map_to_expected_cef_bits() {
        assert_eq!(build_cef_urlrequest_flags(UrlRequestFlags::default()), 0);
        let all = UrlRequestFlags {
            skip_cache: true,
            allow_stored_credentials: true,
            report_upload_progress: true,
            no_download_data: true,
            no_retry_on_5xx: true,
            stop_on_redirect: true,
        };
        let bits = build_cef_urlrequest_flags(all);
        assert_eq!(bits, 1 | 8 | 16 | 32 | 64 | 128);
    }

    #[test]
    fn redirect_and_size_checks_and_completion_drain_barrier() {
        let policy = test_policy();
        let audit = NativeAudit::new();
        let ui: Arc<dyn UiDispatcher> = Arc::new(DummyDispatcher);
        let profiles = ProfileManager::new(
            RuntimeGeneration::new(1),
            rust_cef_core::contracts::ProfileStorage::Memory,
            ui.clone(),
            audit.clone(),
            None,
        );
        let mgr = UrlRequestManager::new(
            RuntimeGeneration::new(1),
            policy.clone(),
            &profiles,
            ui.clone(),
            audit.clone(),
        );

        let id = lock(&mgr.ledger).admit().unwrap();
        let state = Arc::new(Mutex::new(UrlRequestState::Pending(UrlRequestProgress {
            id,
            uploaded_bytes: 0,
            upload_total: -1,
            downloaded_bytes: 0,
            download_total: -1,
        })));
        lock(&mgr.entries).insert(
            id,
            ActiveUrlRequestEntry {
                target_browser: None,
                max_response_bytes: 64,
                stop_on_redirect: false,
                initial_url: "https://allowed.example/ok".into(),
                state: state.clone(),
                body: Vec::new(),
                native: None,
                cancel_requested: false,
                timed_out: false,
                policy_denied: false,
                too_large: false,
                auth_attempted: false,
            },
        );
        mgr.pending.fetch_add(1, Ordering::AcqRel);
        assert_eq!(mgr.pending_count(), 1);
        assert!(!mgr.is_clear());

        // Redirect to untrusted origin marks policy_denied.
        let _ = mgr.on_download_progress(id, 0, 16, Some("https://evil.example/bounce"));
        mgr.on_request_complete(
            id,
            UrlRequestStatus::Canceled,
            Errorcode::ABORTED.get_raw(),
            None,
            "https://evil.example/bounce".into(),
            Vec::new(),
            false,
        );
        assert_eq!(
            *lock(&state),
            UrlRequestState::Failed(UrlRequestError::PolicyDenied)
        );
        assert_eq!(mgr.pending_count(), 0);
        assert!(mgr.is_clear());
        let (counts, _) = policy.snapshot();
        assert!(counts[PolicyAction::Redirect as usize] >= 1);

        // Ledger cap enforces 32 max concurrent.
        let mut ids = Vec::new();
        for _ in 0..MAX_IN_FLIGHT_URL_REQUESTS {
            ids.push(lock(&mgr.ledger).admit().unwrap());
        }
        assert_eq!(
            lock(&mgr.ledger).admit(),
            Err(UrlRequestError::Backpressure)
        );
        mgr.drain_on_ui(true);
        for item in ids {
            assert!(lock(&mgr.ledger).complete(item));
        }
        assert!(mgr.is_clear());

        // Pre-submission rejects untrusted origins and foreign runtime handles before CEF posting.
        let sink = Arc::new(DummySink);
        let reg = crate::registry::UiRegistry::new(
            RuntimeGeneration::new(1),
            sink.clone(),
            sink,
            ui,
            audit.clone(),
        );
        let executor = Executor::new(RuntimeGeneration::new(1), &reg, audit);
        reg.attach_executor(&executor);
        let untrusted = UrlRequestSpec::get(
            UrlRequestTarget::DefaultProfile,
            "https://evil.example/blocked",
        );
        assert_eq!(
            mgr.submit(untrusted, &executor, false).unwrap_err(),
            UrlRequestError::PolicyDenied
        );
        let foreign_profile = ProfileHandle::new(RuntimeGeneration::new(99), 1);
        let foreign = UrlRequestSpec::get(
            UrlRequestTarget::Profile(foreign_profile),
            "https://allowed.example/ok",
        );
        assert_eq!(
            mgr.submit(foreign, &executor, false).unwrap_err(),
            UrlRequestError::InvalidRequest
        );
        let draining_req = UrlRequestSpec::get(
            UrlRequestTarget::DefaultProfile,
            "https://allowed.example/ok",
        );
        assert_eq!(
            mgr.submit(draining_req, &executor, true).unwrap_err(),
            UrlRequestError::Closing
        );
        assert!(mgr.is_clear());
    }
}
