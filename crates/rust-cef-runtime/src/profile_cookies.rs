//! Profile-scoped asynchronous cookie management, opt-in `CookieAccessFilter`,
//! HTTP auth cache clearing, and guarded preference writes.
//!
//! Native CEF references and callbacks stay scoped to CEF tasks/threads; public
//! tickets hold plain data only, `RedactedSecret` never emits raw cookie values
//! in `Debug` or error logs, and `Drop` completion guards ensure zero-cookie
//! visits or refused callbacks never wedge the shutdown gate.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use cef::*;
use rust_cef_core::{
    contracts::{BrowserId, ProfileHandle},
    host_capabilities::{
        CookieAccessPolicy, CookieFilterDecision, CookieFilterRequest, CookiePriorityKind,
        CookieRecord, CookieRequestId, CookieSameSiteKind, HostCapabilityError,
        MAX_COOKIE_RECORDS_PER_VISIT, PreferenceValue, RedactedSecret, validate_preference_update,
    },
    ports::UiDispatcher,
};

use crate::{
    executor::{Executor, NativeAudit, TaskPayload},
    profiles::{ProfileCreateError, ProfileManager},
    registry::WritableCefString,
};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CookieVisitStatus {
    Pending,
    Completed {
        request_id: CookieRequestId,
        cookies: Vec<CookieRecord>,
    },
    Failed {
        request_id: CookieRequestId,
        error: HostCapabilityError,
    },
}

pub struct CookieVisitTicket {
    request_id: CookieRequestId,
    state: Arc<Mutex<CookieVisitStatus>>,
}

impl CookieVisitTicket {
    pub fn request_id(&self) -> CookieRequestId {
        self.request_id
    }

    pub fn poll(&self) -> CookieVisitStatus {
        lock(&self.state).clone()
    }
}

impl std::fmt::Debug for CookieVisitTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CookieVisitTicket")
            .field("request_id", &self.request_id)
            .field(
                "pending",
                &matches!(&*lock(&self.state), CookieVisitStatus::Pending),
            )
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CookieMutationStatus {
    Pending,
    Succeeded {
        request_id: CookieRequestId,
        deleted: Option<u32>,
    },
    Failed {
        request_id: CookieRequestId,
        error: HostCapabilityError,
    },
}

pub struct CookieMutationTicket {
    request_id: CookieRequestId,
    state: Arc<Mutex<CookieMutationStatus>>,
}

impl CookieMutationTicket {
    pub fn request_id(&self) -> CookieRequestId {
        self.request_id
    }

    pub fn poll(&self) -> CookieMutationStatus {
        lock(&self.state).clone()
    }
}

impl std::fmt::Debug for CookieMutationTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CookieMutationTicket")
            .field("request_id", &self.request_id)
            .field("status", &self.poll())
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileActionStatus {
    Pending,
    Succeeded {
        request_id: CookieRequestId,
    },
    Failed {
        request_id: CookieRequestId,
        error: HostCapabilityError,
    },
}

pub struct ProfileActionTicket {
    request_id: CookieRequestId,
    state: Arc<Mutex<ProfileActionStatus>>,
}

impl ProfileActionTicket {
    pub fn request_id(&self) -> CookieRequestId {
        self.request_id
    }

    pub fn poll(&self) -> ProfileActionStatus {
        lock(&self.state).clone()
    }
}

impl std::fmt::Debug for ProfileActionTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileActionTicket")
            .field("request_id", &self.request_id)
            .field("status", &self.poll())
            .finish()
    }
}

pub(crate) struct ProfileCookieRegistry {
    profiles: Weak<ProfileManager>,
    executor: Weak<Executor>,
    ui: Arc<dyn UiDispatcher>,
    audit: Arc<NativeAudit>,
    next_seq: AtomicU64,
    pending: Arc<AtomicU64>,
    closed: AtomicBool,
    visits: Mutex<BTreeMap<CookieRequestId, Arc<Mutex<CookieVisitStatus>>>>,
    mutations: Mutex<BTreeMap<CookieRequestId, Arc<Mutex<CookieMutationStatus>>>>,
    actions: Mutex<BTreeMap<CookieRequestId, Arc<Mutex<ProfileActionStatus>>>>,
}

fn map_profile_error(err: ProfileCreateError) -> HostCapabilityError {
    match err {
        ProfileCreateError::Closed => HostCapabilityError::BrowserClosed,
        ProfileCreateError::NotReady
        | ProfileCreateError::Profile(rust_cef_core::profiles::ProfileError::NotReady) => {
            HostCapabilityError::NotReady
        }
        ProfileCreateError::Profile(rust_cef_core::profiles::ProfileError::WrongRuntime) => {
            HostCapabilityError::WrongRuntime
        }
        ProfileCreateError::PostFailed(_) => HostCapabilityError::PostFailed,
        _ => HostCapabilityError::OperationFailed,
    }
}

struct PendingCounterGuard {
    pending: Arc<AtomicU64>,
    finished: bool,
}

impl PendingCounterGuard {
    fn new(pending: Arc<AtomicU64>) -> Self {
        Self {
            pending,
            finished: false,
        }
    }

    fn release(&mut self) -> bool {
        if self.finished {
            return false;
        }
        self.finished = true;
        self.pending.fetch_sub(1, Ordering::AcqRel);
        true
    }
}

impl Drop for PendingCounterGuard {
    fn drop(&mut self) {
        self.release();
    }
}

impl ProfileCookieRegistry {
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
            next_seq: AtomicU64::new(1),
            pending: Arc::new(AtomicU64::new(0)),
            closed: AtomicBool::new(false),
            visits: Mutex::new(BTreeMap::new()),
            mutations: Mutex::new(BTreeMap::new()),
            actions: Mutex::new(BTreeMap::new()),
        })
    }

    pub(crate) fn pending(&self) -> u64 {
        self.pending.load(Ordering::Acquire)
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        for (id, cell) in std::mem::take(&mut *lock(&self.visits)) {
            *lock(&cell) = CookieVisitStatus::Failed {
                request_id: id,
                error: HostCapabilityError::BrowserClosed,
            };
        }
        for (id, cell) in std::mem::take(&mut *lock(&self.mutations)) {
            *lock(&cell) = CookieMutationStatus::Failed {
                request_id: id,
                error: HostCapabilityError::BrowserClosed,
            };
        }
        for (id, cell) in std::mem::take(&mut *lock(&self.actions)) {
            *lock(&cell) = ProfileActionStatus::Failed {
                request_id: id,
                error: HostCapabilityError::BrowserClosed,
            };
        }
    }

    fn begin_request(
        &self,
        profile: ProfileHandle,
    ) -> Result<CookieRequestId, HostCapabilityError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(HostCapabilityError::BrowserClosed);
        }
        let profiles = self
            .profiles
            .upgrade()
            .ok_or(HostCapabilityError::BrowserClosed)?;
        if profiles.is_sealed() {
            return Err(HostCapabilityError::BrowserClosed);
        }
        profiles.select(Some(profile)).map_err(map_profile_error)?;
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        if seq == 0 {
            return Err(HostCapabilityError::QueueFull);
        }
        Ok(CookieRequestId(seq))
    }

    fn post_with_state<S>(
        &self,
        request_id: CookieRequestId,
        table: &Mutex<BTreeMap<CookieRequestId, Arc<Mutex<S>>>>,
        initial: S,
        payload: TaskPayload,
    ) -> Result<Arc<Mutex<S>>, HostCapabilityError> {
        let executor = self
            .executor
            .upgrade()
            .ok_or(HostCapabilityError::BrowserClosed)?;
        let cell = Arc::new(Mutex::new(initial));
        lock(table).insert(request_id, cell.clone());
        self.pending.fetch_add(1, Ordering::AcqRel);
        if executor.post(payload).is_err() {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            lock(table).remove(&request_id);
            return Err(HostCapabilityError::PostFailed);
        }
        Ok(cell)
    }

    pub(crate) fn visit_cookies(
        &self,
        profile: ProfileHandle,
        url: Option<String>,
        include_http_only: bool,
    ) -> Result<CookieVisitTicket, HostCapabilityError> {
        let request_id = self.begin_request(profile)?;
        let state = self.post_with_state(
            request_id,
            &self.visits,
            CookieVisitStatus::Pending,
            TaskPayload::VisitProfileCookies {
                request_id,
                handle: profile,
                url,
                include_http_only,
            },
        )?;
        Ok(CookieVisitTicket { request_id, state })
    }

    pub(crate) fn set_cookie(
        &self,
        profile: ProfileHandle,
        url: String,
        cookie: CookieRecord,
    ) -> Result<CookieMutationTicket, HostCapabilityError> {
        if url.is_empty() || url.contains('\0') || cookie.name.is_empty() {
            return Err(HostCapabilityError::InvalidPreferenceValue);
        }
        let request_id = self.begin_request(profile)?;
        let state = self.post_with_state(
            request_id,
            &self.mutations,
            CookieMutationStatus::Pending,
            TaskPayload::SetProfileCookie {
                request_id,
                handle: profile,
                url,
                cookie,
            },
        )?;
        Ok(CookieMutationTicket { request_id, state })
    }

    pub(crate) fn delete_cookies(
        &self,
        profile: ProfileHandle,
        url: Option<String>,
        cookie_name: Option<String>,
    ) -> Result<CookieMutationTicket, HostCapabilityError> {
        let request_id = self.begin_request(profile)?;
        let state = self.post_with_state(
            request_id,
            &self.mutations,
            CookieMutationStatus::Pending,
            TaskPayload::DeleteProfileCookies {
                request_id,
                handle: profile,
                url,
                cookie_name,
            },
        )?;
        Ok(CookieMutationTicket { request_id, state })
    }

    pub(crate) fn clear_http_auth_credentials(
        &self,
        profile: ProfileHandle,
    ) -> Result<ProfileActionTicket, HostCapabilityError> {
        let request_id = self.begin_request(profile)?;
        let state = self.post_with_state(
            request_id,
            &self.actions,
            ProfileActionStatus::Pending,
            TaskPayload::ClearProfileHttpAuth {
                request_id,
                handle: profile,
            },
        )?;
        Ok(ProfileActionTicket { request_id, state })
    }

    pub(crate) fn set_preference(
        &self,
        profile: ProfileHandle,
        name: String,
        value: PreferenceValue,
    ) -> Result<ProfileActionTicket, HostCapabilityError> {
        validate_preference_update(&name, &value)?;
        let request_id = self.begin_request(profile)?;
        let state = self.post_with_state(
            request_id,
            &self.actions,
            ProfileActionStatus::Pending,
            TaskPayload::SetProfilePreference {
                request_id,
                handle: profile,
                name,
                value,
            },
        )?;
        Ok(ProfileActionTicket { request_id, state })
    }

    // ---- CEF UI sequence execution ----

    fn ready_context(&self, handle: ProfileHandle) -> Result<RequestContext, HostCapabilityError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(HostCapabilityError::BrowserClosed);
        }
        let profiles = self
            .profiles
            .upgrade()
            .ok_or(HostCapabilityError::BrowserClosed)?;
        if profiles.is_sealed() {
            return Err(HostCapabilityError::BrowserClosed);
        }
        profiles
            .context_for(handle)
            .ok_or(HostCapabilityError::NotReady)
    }

    fn ready_cookie_manager(
        &self,
        handle: ProfileHandle,
    ) -> Result<(RequestContext, CookieManager), HostCapabilityError> {
        let context = self.ready_context(handle)?;
        self.audit.record();
        match context.cookie_manager(None) {
            Some(manager) => Ok((context, manager)),
            None => {
                drop(context);
                Err(HostCapabilityError::OperationFailed)
            }
        }
    }

    pub(crate) fn run_visit_cookies(
        &self,
        on_ui: bool,
        request_id: CookieRequestId,
        handle: ProfileHandle,
        url: Option<String>,
        include_http_only: bool,
    ) {
        let mut guard = PendingCounterGuard::new(self.pending.clone());
        let state = lock(&self.visits)
            .remove(&request_id)
            .unwrap_or_else(|| Arc::new(Mutex::new(CookieVisitStatus::Pending)));
        let fail = |guard: &mut PendingCounterGuard, err: HostCapabilityError| {
            *lock(&state) = CookieVisitStatus::Failed {
                request_id,
                error: err,
            };
            guard.release();
            let _ = self.ui.wake();
        };
        if !on_ui {
            fail(&mut guard, HostCapabilityError::Cancelled);
            return;
        }
        let (context, manager) = match self.ready_cookie_manager(handle) {
            Ok(pair) => pair,
            Err(err) => {
                fail(&mut guard, err);
                return;
            }
        };

        let collector = Arc::new(Mutex::new(CookieVisitCollector {
            request_id,
            cookies: Vec::new(),
            out: state,
            guard: Some(guard),
            ui: self.ui.clone(),
        }));
        let mut visitor = ProfileCookieVisitor::new(collector.clone());
        self.audit.record();
        let accepted = match url.as_deref() {
            Some(url_str) => {
                let cef_url = CefString::from(url_str);
                manager.visit_url_cookies(
                    Some(&cef_url),
                    include_http_only as i32,
                    Some(&mut visitor),
                ) != 0
            }
            None => manager.visit_all_cookies(Some(&mut visitor)) != 0,
        };
        drop(manager);
        drop(context);
        if !accepted {
            lock(&collector).finish_error(HostCapabilityError::OperationFailed);
        }
        drop(visitor);
    }

    pub(crate) fn run_set_cookie(
        &self,
        on_ui: bool,
        request_id: CookieRequestId,
        handle: ProfileHandle,
        url: String,
        record: CookieRecord,
    ) {
        let mut guard = PendingCounterGuard::new(self.pending.clone());
        let state = lock(&self.mutations)
            .remove(&request_id)
            .unwrap_or_else(|| Arc::new(Mutex::new(CookieMutationStatus::Pending)));
        let fail = |guard: &mut PendingCounterGuard, err: HostCapabilityError| {
            *lock(&state) = CookieMutationStatus::Failed {
                request_id,
                error: err,
            };
            guard.release();
            let _ = self.ui.wake();
        };
        if !on_ui {
            fail(&mut guard, HostCapabilityError::Cancelled);
            return;
        }
        let (context, manager) = match self.ready_cookie_manager(handle) {
            Ok(pair) => pair,
            Err(err) => {
                fail(&mut guard, err);
                return;
            }
        };

        let collector = Arc::new(Mutex::new(CookieMutationCollector {
            request_id,
            out: state,
            guard: Some(guard),
            ui: self.ui.clone(),
        }));
        let mut cb = ProfileSetCookieCallback::new(collector.clone());
        let cef_url = CefString::from(url.as_str());
        let cookie = Cookie {
            size: std::mem::size_of::<cef::sys::_cef_cookie_t>(),
            name: CefString::from(record.name.as_str()),
            value: CefString::from(record.value.expose()),
            domain: CefString::from(record.domain.as_str()),
            path: CefString::from(record.path.as_str()),
            secure: record.secure as i32,
            httponly: record.http_only as i32,
            same_site: map_to_cef_same_site(record.same_site),
            priority: map_to_cef_priority(record.priority),
            ..Default::default()
        };
        self.audit.record();
        let accepted = manager.set_cookie(Some(&cef_url), Some(&cookie), Some(&mut cb)) != 0;
        drop(manager);
        drop(context);
        if !accepted {
            lock(&collector).finish_error(HostCapabilityError::OperationFailed);
        }
        drop(cb);
    }

    pub(crate) fn run_delete_cookies(
        &self,
        on_ui: bool,
        request_id: CookieRequestId,
        handle: ProfileHandle,
        url: Option<String>,
        cookie_name: Option<String>,
    ) {
        let mut guard = PendingCounterGuard::new(self.pending.clone());
        let state = lock(&self.mutations)
            .remove(&request_id)
            .unwrap_or_else(|| Arc::new(Mutex::new(CookieMutationStatus::Pending)));
        let fail = |guard: &mut PendingCounterGuard, err: HostCapabilityError| {
            *lock(&state) = CookieMutationStatus::Failed {
                request_id,
                error: err,
            };
            guard.release();
            let _ = self.ui.wake();
        };
        if !on_ui {
            fail(&mut guard, HostCapabilityError::Cancelled);
            return;
        }
        let (context, manager) = match self.ready_cookie_manager(handle) {
            Ok(pair) => pair,
            Err(err) => {
                fail(&mut guard, err);
                return;
            }
        };

        let collector = Arc::new(Mutex::new(CookieMutationCollector {
            request_id,
            out: state,
            guard: Some(guard),
            ui: self.ui.clone(),
        }));
        let mut cb = ProfileDeleteCookiesCallback::new(collector.clone());
        let cef_url = url.as_deref().map(CefString::from);
        let cef_name = cookie_name.as_deref().map(CefString::from);
        self.audit.record();
        let accepted =
            manager.delete_cookies(cef_url.as_ref(), cef_name.as_ref(), Some(&mut cb)) != 0;
        drop(manager);
        drop(context);
        if !accepted {
            lock(&collector).finish_error(HostCapabilityError::OperationFailed);
        }
        drop(cb);
    }

    pub(crate) fn run_clear_http_auth(
        &self,
        on_ui: bool,
        request_id: CookieRequestId,
        handle: ProfileHandle,
    ) {
        let mut guard = PendingCounterGuard::new(self.pending.clone());
        let state = lock(&self.actions)
            .remove(&request_id)
            .unwrap_or_else(|| Arc::new(Mutex::new(ProfileActionStatus::Pending)));
        let fail = |guard: &mut PendingCounterGuard, err: HostCapabilityError| {
            *lock(&state) = ProfileActionStatus::Failed {
                request_id,
                error: err,
            };
            guard.release();
            let _ = self.ui.wake();
        };
        if !on_ui {
            fail(&mut guard, HostCapabilityError::Cancelled);
            return;
        }
        let context = match self.ready_context(handle) {
            Ok(ctx) => ctx,
            Err(err) => {
                fail(&mut guard, err);
                return;
            }
        };
        let collector = Arc::new(Mutex::new(ProfileActionCollector {
            request_id,
            out: state,
            guard: Some(guard),
            ui: self.ui.clone(),
        }));
        let mut cb = ProfileCompletionCallback::new(collector);
        self.audit.record();
        context.clear_http_auth_credentials(Some(&mut cb));
        drop(cb);
        drop(context);
    }

    pub(crate) fn run_set_preference(
        &self,
        on_ui: bool,
        request_id: CookieRequestId,
        handle: ProfileHandle,
        name: String,
        value: PreferenceValue,
    ) {
        let mut guard = PendingCounterGuard::new(self.pending.clone());
        let state = lock(&self.actions)
            .remove(&request_id)
            .unwrap_or_else(|| Arc::new(Mutex::new(ProfileActionStatus::Pending)));
        let mut finish = |status: ProfileActionStatus| {
            *lock(&state) = status;
            guard.release();
            let _ = self.ui.wake();
        };
        if !on_ui {
            finish(ProfileActionStatus::Failed {
                request_id,
                error: HostCapabilityError::Cancelled,
            });
            return;
        }
        if let Err(err) = validate_preference_update(&name, &value) {
            finish(ProfileActionStatus::Failed {
                request_id,
                error: err,
            });
            return;
        }
        let context = match self.ready_context(handle) {
            Ok(ctx) => ctx,
            Err(err) => {
                finish(ProfileActionStatus::Failed {
                    request_id,
                    error: err,
                });
                return;
            }
        };
        let cef_name = CefString::from(name.as_str());
        self.audit.record();
        if context.can_set_preference(Some(&cef_name)) == 0 {
            drop(context);
            finish(ProfileActionStatus::Failed {
                request_id,
                error: HostCapabilityError::PreferenceBlocked,
            });
            return;
        }
        let Some(mut cef_value) = build_cef_preference_value(&value) else {
            drop(context);
            finish(ProfileActionStatus::Failed {
                request_id,
                error: HostCapabilityError::InvalidPreferenceValue,
            });
            return;
        };
        let mut err_buf = WritableCefString::new();
        let mut err_cef = err_buf.as_mut_cef();
        self.audit.record();
        let ok =
            context.set_preference(Some(&cef_name), Some(&mut cef_value), Some(&mut err_cef)) != 0;
        drop(cef_value);
        drop(context);
        drop(err_buf);
        if ok {
            finish(ProfileActionStatus::Succeeded { request_id });
        } else {
            finish(ProfileActionStatus::Failed {
                request_id,
                error: HostCapabilityError::OperationFailed,
            });
        }
    }
}

fn build_cef_preference_value(value: &PreferenceValue) -> Option<Value> {
    let v = cef::value_create()?;
    let ok = match value {
        PreferenceValue::Null => v.set_null() != 0,
        PreferenceValue::Bool(b) => v.set_bool(*b as i32) != 0,
        PreferenceValue::Int(i) => v.set_int(*i) != 0,
        PreferenceValue::Double(d) => v.set_double(*d) != 0,
        PreferenceValue::String(s) => v.set_string(Some(&CefString::from(s.as_str()))) != 0,
        PreferenceValue::StringList(items) => {
            let mut list = cef::list_value_create()?;
            list.set_size(items.len());
            for (idx, item) in items.iter().enumerate() {
                list.set_string(idx, Some(&CefString::from(item.as_str())));
            }
            v.set_list(Some(&mut list)) != 0
        }
    };
    ok.then_some(v)
}

struct CookieVisitCollector {
    request_id: CookieRequestId,
    cookies: Vec<CookieRecord>,
    out: Arc<Mutex<CookieVisitStatus>>,
    guard: Option<PendingCounterGuard>,
    ui: Arc<dyn UiDispatcher>,
}

impl CookieVisitCollector {
    fn finish_ok(&mut self) {
        let Some(mut guard) = self.guard.take() else {
            return;
        };
        let cookies = std::mem::take(&mut self.cookies);
        *lock(&self.out) = CookieVisitStatus::Completed {
            request_id: self.request_id,
            cookies,
        };
        guard.release();
        let _ = self.ui.wake();
    }

    fn finish_error(&mut self, error: HostCapabilityError) {
        let Some(mut guard) = self.guard.take() else {
            return;
        };
        self.cookies.clear();
        *lock(&self.out) = CookieVisitStatus::Failed {
            request_id: self.request_id,
            error,
        };
        guard.release();
        let _ = self.ui.wake();
    }
}

impl Drop for CookieVisitCollector {
    fn drop(&mut self) {
        // When 0 cookies match a visit query, CEF drops CookieVisitor without
        // ever calling `visit()`. Complete cleanly with the collected vec.
        self.finish_ok();
    }
}

wrap_cookie_visitor! {
    struct ProfileCookieVisitor {
        collector: Arc<Mutex<CookieVisitCollector>>,
    }

    impl CookieVisitor {
        fn visit(
            &self,
            cookie: Option<&Cookie>,
            count: ::std::os::raw::c_int,
            total: ::std::os::raw::c_int,
            _delete_cookie: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            let mut collector = lock(&self.collector);
            if let Some(c) = cookie
                && collector.cookies.len() < MAX_COOKIE_RECORDS_PER_VISIT
            {
                collector.cookies.push(CookieRecord {
                    name: c.name.to_string(),
                    value: RedactedSecret::new(c.value.to_string()),
                    domain: c.domain.to_string(),
                    path: c.path.to_string(),
                    secure: c.secure != 0,
                    http_only: c.httponly != 0,
                    same_site: map_from_cef_same_site(c.same_site),
                    priority: map_from_cef_priority(c.priority),
                });
            }
            if count.saturating_add(1) >= total
                || collector.cookies.len() >= MAX_COOKIE_RECORDS_PER_VISIT
            {
                collector.finish_ok();
                0
            } else {
                1
            }
        }
    }
}

struct CookieMutationCollector {
    request_id: CookieRequestId,
    out: Arc<Mutex<CookieMutationStatus>>,
    guard: Option<PendingCounterGuard>,
    ui: Arc<dyn UiDispatcher>,
}

impl CookieMutationCollector {
    fn finish_ok(&mut self, deleted: Option<u32>) {
        let Some(mut guard) = self.guard.take() else {
            return;
        };
        *lock(&self.out) = CookieMutationStatus::Succeeded {
            request_id: self.request_id,
            deleted,
        };
        guard.release();
        let _ = self.ui.wake();
    }

    fn finish_error(&mut self, error: HostCapabilityError) {
        let Some(mut guard) = self.guard.take() else {
            return;
        };
        *lock(&self.out) = CookieMutationStatus::Failed {
            request_id: self.request_id,
            error,
        };
        guard.release();
        let _ = self.ui.wake();
    }
}

impl Drop for CookieMutationCollector {
    fn drop(&mut self) {
        self.finish_error(HostCapabilityError::Cancelled);
    }
}

wrap_set_cookie_callback! {
    struct ProfileSetCookieCallback {
        collector: Arc<Mutex<CookieMutationCollector>>,
    }

    impl SetCookieCallback {
        fn on_complete(&self, success: ::std::os::raw::c_int) {
            let mut collector = lock(&self.collector);
            if success != 0 {
                collector.finish_ok(None);
            } else {
                collector.finish_error(HostCapabilityError::OperationFailed);
            }
        }
    }
}

wrap_delete_cookies_callback! {
    struct ProfileDeleteCookiesCallback {
        collector: Arc<Mutex<CookieMutationCollector>>,
    }

    impl DeleteCookiesCallback {
        fn on_complete(&self, num_deleted: ::std::os::raw::c_int) {
            let mut collector = lock(&self.collector);
            if num_deleted >= 0 {
                collector.finish_ok(Some(num_deleted as u32));
            } else {
                collector.finish_error(HostCapabilityError::OperationFailed);
            }
        }
    }
}

struct ProfileActionCollector {
    request_id: CookieRequestId,
    out: Arc<Mutex<ProfileActionStatus>>,
    guard: Option<PendingCounterGuard>,
    ui: Arc<dyn UiDispatcher>,
}

impl ProfileActionCollector {
    fn finish_ok(&mut self) {
        let Some(mut guard) = self.guard.take() else {
            return;
        };
        *lock(&self.out) = ProfileActionStatus::Succeeded {
            request_id: self.request_id,
        };
        guard.release();
        let _ = self.ui.wake();
    }

    fn finish_error(&mut self, error: HostCapabilityError) {
        let Some(mut guard) = self.guard.take() else {
            return;
        };
        *lock(&self.out) = ProfileActionStatus::Failed {
            request_id: self.request_id,
            error,
        };
        guard.release();
        let _ = self.ui.wake();
    }
}

impl Drop for ProfileActionCollector {
    fn drop(&mut self) {
        self.finish_error(HostCapabilityError::Cancelled);
    }
}

wrap_completion_callback! {
    struct ProfileCompletionCallback {
        collector: Arc<Mutex<ProfileActionCollector>>,
    }

    impl CompletionCallback {
        fn on_complete(&self) {
            lock(&self.collector).finish_ok();
        }
    }
}

pub(crate) fn cookie_access_filter(
    browser: Option<BrowserId>,
    policy: Arc<dyn CookieAccessPolicy>,
) -> CookieAccessFilter {
    RuntimeCookieAccessFilter::new(browser, policy)
}

wrap_cookie_access_filter! {
    struct RuntimeCookieAccessFilter {
        browser: Option<BrowserId>,
        policy: Arc<dyn CookieAccessPolicy>,
    }

    impl CookieAccessFilter {
        fn can_send_cookie(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            cookie: Option<&Cookie>,
        ) -> ::std::os::raw::c_int {
            let url = request.map(|r| CefString::from(&r.url()).to_string()).unwrap_or_default();
            let req = CookieFilterRequest {
                browser: self.browser,
                url,
                cookie_name: cookie.map(|c| c.name.to_string()).unwrap_or_default(),
                cookie_domain: cookie.map(|c| c.domain.to_string()).unwrap_or_default(),
                cookie_path: cookie.map(|c| c.path.to_string()).unwrap_or_default(),
                is_save: false,
            };
            i32::from(self.policy.can_send_cookie(&req) == CookieFilterDecision::Allow)
        }

        fn can_save_cookie(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _response: Option<&mut Response>,
            cookie: Option<&Cookie>,
        ) -> ::std::os::raw::c_int {
            let url = request.map(|r| CefString::from(&r.url()).to_string()).unwrap_or_default();
            let req = CookieFilterRequest {
                browser: self.browser,
                url,
                cookie_name: cookie.map(|c| c.name.to_string()).unwrap_or_default(),
                cookie_domain: cookie.map(|c| c.domain.to_string()).unwrap_or_default(),
                cookie_path: cookie.map(|c| c.path.to_string()).unwrap_or_default(),
                is_save: true,
            };
            i32::from(self.policy.can_save_cookie(&req) == CookieFilterDecision::Allow)
        }
    }
}

fn map_to_cef_same_site(same_site: CookieSameSiteKind) -> CookieSameSite {
    match same_site {
        CookieSameSiteKind::Unspecified => CookieSameSite::UNSPECIFIED,
        CookieSameSiteKind::NoRestriction => CookieSameSite::NO_RESTRICTION,
        CookieSameSiteKind::Lax => CookieSameSite::LAX_MODE,
        CookieSameSiteKind::Strict => CookieSameSite::STRICT_MODE,
    }
}

fn map_from_cef_same_site(same_site: CookieSameSite) -> CookieSameSiteKind {
    if same_site == CookieSameSite::NO_RESTRICTION {
        CookieSameSiteKind::NoRestriction
    } else if same_site == CookieSameSite::LAX_MODE {
        CookieSameSiteKind::Lax
    } else if same_site == CookieSameSite::STRICT_MODE {
        CookieSameSiteKind::Strict
    } else {
        CookieSameSiteKind::Unspecified
    }
}

fn map_to_cef_priority(priority: CookiePriorityKind) -> CookiePriority {
    match priority {
        CookiePriorityKind::Low => CookiePriority::LOW,
        CookiePriorityKind::Medium => CookiePriority::MEDIUM,
        CookiePriorityKind::High => CookiePriority::HIGH,
    }
}

fn map_from_cef_priority(priority: CookiePriority) -> CookiePriorityKind {
    if priority == CookiePriority::LOW {
        CookiePriorityKind::Low
    } else if priority == CookiePriority::HIGH {
        CookiePriorityKind::High
    } else {
        CookiePriorityKind::Medium
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::contracts::{ProfileStorage, RuntimeGeneration};
    use rust_cef_core::ports::EngineError;

    struct DummyUi;
    impl UiDispatcher for DummyUi {
        fn wake(&self) -> Result<(), EngineError> {
            Ok(())
        }
    }

    #[test]
    fn zero_cookie_visitor_drop_completes_empty_and_releases_pending() {
        let pending = Arc::new(AtomicU64::new(1));
        let out = Arc::new(Mutex::new(CookieVisitStatus::Pending));
        let guard = PendingCounterGuard::new(pending.clone());
        let collector = Arc::new(Mutex::new(CookieVisitCollector {
            request_id: CookieRequestId(7),
            cookies: Vec::new(),
            out: out.clone(),
            guard: Some(guard),
            ui: Arc::new(DummyUi),
        }));
        let visitor = ProfileCookieVisitor::new(collector);
        // Simulate CEF finding 0 matching cookies and dropping the visitor without calling visit().
        drop(visitor);

        assert_eq!(pending.load(Ordering::Acquire), 0);
        assert_eq!(
            *lock(&out),
            CookieVisitStatus::Completed {
                request_id: CookieRequestId(7),
                cookies: Vec::new(),
            }
        );
    }

    #[test]
    fn sealed_cookie_registry_and_blocked_preference_fail_safely() {
        let generation = RuntimeGeneration::new(1);
        let ui = Arc::new(DummyUi);
        let audit = NativeAudit::new();
        let profiles = ProfileManager::new(
            generation,
            ProfileStorage::Memory,
            ui.clone(),
            audit.clone(),
            None,
        );
        let reg = crate::registry::UiRegistry::new(
            generation,
            Arc::new(DummySink),
            Arc::new(DummySink),
            ui.clone(),
            audit.clone(),
        );
        let executor = Executor::new(generation, &reg, audit.clone());
        let cookies = ProfileCookieRegistry::new(&profiles, &executor, ui, audit);
        let def = profiles.default_handle();

        assert_eq!(
            cookies
                .set_preference(
                    def,
                    "browser.cache_path".into(),
                    PreferenceValue::String("x".into())
                )
                .unwrap_err(),
            HostCapabilityError::PreferenceBlocked
        );

        cookies.close();
        assert_eq!(
            cookies.visit_cookies(def, None, true).unwrap_err(),
            HostCapabilityError::BrowserClosed
        );
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
}
