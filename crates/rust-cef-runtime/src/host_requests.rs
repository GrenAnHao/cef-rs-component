//! Lock-external single-use callback tables for opt-in host capabilities.
//!
//! Modeled after [`crate::before_unload`]: CEF callbacks and X509 certificate
//! references are never invoked or dropped while holding any mutex lock. All
//! callbacks are audited on the calling runtime path and released before
//! `cef::shutdown`.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use cef::*;
use rust_cef_core::{
    contracts::BrowserId,
    host_capabilities::{
        AuthDecision, CertificateDecision, DownloadId, DownloadProgress, DownloadState,
        FileDialogDecision, HostCapabilities, HostCapabilityError, HostRequestId, JsDialogDecision,
        MAX_FILE_DIALOG_PATHS, MAX_HOST_REQUEST_LEDGER_ENTRIES, PermissionDecision,
        validate_download_destination,
    },
};

use crate::{executor::NativeAudit, policy_native::NativePolicy};

const MAX_CONSUMED_HISTORY: usize = 1024;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn mark_bounded<K: Ord + Copy>(set: &mut BTreeSet<K>, key: K, cap: usize) {
    if set.len() >= cap.max(1) {
        if let Some(oldest) = set.first().copied() {
            set.remove(&oldest);
        }
    }
    set.insert(key);
}

fn mark_consumed<K: Ord + Copy>(consumed: &mut BTreeSet<K>, key: K) {
    mark_bounded(consumed, key, MAX_CONSUMED_HISTORY);
}

enum CallbackSlot<V> {
    Active { browser: BrowserId, value: V },
    Claimed { browser: BrowserId, value: V },
}

impl<V> CallbackSlot<V> {
    fn browser(&self) -> BrowserId {
        match self {
            Self::Active { browser, .. } | Self::Claimed { browser, .. } => *browser,
        }
    }

    fn into_value(self) -> V {
        match self {
            Self::Active { value, .. } | Self::Claimed { value, .. } => value,
        }
    }
}

struct CallbackTable<V> {
    capacity: usize,
    active: BTreeMap<HostRequestId, CallbackSlot<V>>,
    consumed: BTreeSet<HostRequestId>,
}

impl<V> CallbackTable<V> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.clamp(1, MAX_HOST_REQUEST_LEDGER_ENTRIES),
            active: BTreeMap::new(),
            consumed: BTreeSet::new(),
        }
    }

    fn insert(
        &mut self,
        id: HostRequestId,
        browser: BrowserId,
        value: V,
    ) -> Result<(), (V, HostCapabilityError)> {
        if self.active.contains_key(&id) || self.consumed.contains(&id) {
            return Err((value, HostCapabilityError::AlreadyConsumed));
        }
        if self.active.len() >= self.capacity {
            return Err((value, HostCapabilityError::QueueFull));
        }
        self.active
            .insert(id, CallbackSlot::Active { browser, value });
        Ok(())
    }

    fn claim_if<F>(&mut self, id: HostRequestId, validate: F) -> Result<(), HostCapabilityError>
    where
        F: FnOnce(&V) -> Result<(), HostCapabilityError>,
    {
        if let Some(slot) = self.active.get(&id) {
            match slot {
                CallbackSlot::Active { value, .. } => validate(value)?,
                CallbackSlot::Claimed { .. } => return Err(HostCapabilityError::AlreadyConsumed),
            }
            let Some(CallbackSlot::Active { browser, value }) = self.active.remove(&id) else {
                return Err(HostCapabilityError::UnknownOrStaleToken);
            };
            self.active
                .insert(id, CallbackSlot::Claimed { browser, value });
            return Ok(());
        }
        if self.consumed.contains(&id) {
            Err(HostCapabilityError::AlreadyConsumed)
        } else {
            Err(HostCapabilityError::UnknownOrStaleToken)
        }
    }

    fn take(&mut self, id: HostRequestId) -> Option<V> {
        let slot = self.active.remove(&id)?;
        mark_consumed(&mut self.consumed, id);
        Some(slot.into_value())
    }

    fn drain_browser(&mut self, browser: BrowserId) -> Vec<(HostRequestId, V)> {
        let keys: Vec<HostRequestId> = self
            .active
            .iter()
            .filter_map(|(k, v)| (v.browser() == browser).then_some(*k))
            .collect();
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(slot) = self.active.remove(&key) {
                mark_consumed(&mut self.consumed, key);
                out.push((key, slot.into_value()));
            }
        }
        out
    }

    fn drain_all(&mut self) -> Vec<(HostRequestId, V)> {
        let entries = std::mem::take(&mut self.active);
        let mut out = Vec::with_capacity(entries.len());
        for (key, slot) in entries {
            mark_consumed(&mut self.consumed, key);
            out.push((key, slot.into_value()));
        }
        out
    }

    fn len(&self) -> usize {
        self.active.len()
    }
}

pub(crate) enum FileDialogReply {
    Native(FileDialogCallback),
}

pub(crate) enum JsDialogReply {
    Native(JsdialogCallback),
    #[cfg(test)]
    Test(Box<dyn FnOnce(bool, Option<String>) + Send + 'static>),
}

pub(crate) enum PermissionReply {
    Media(MediaAccessCallback),
    Prompt(PermissionPromptCallback),
}

pub(crate) enum AuthReply {
    Native(AuthCallback),
}

pub(crate) enum CertificateReply {
    Server(Callback),
    Client {
        callback: SelectClientCertificateCallback,
        certs: Vec<X509Certificate>,
    },
    #[cfg(test)]
    Test(Box<dyn FnOnce(CertificateDecision) + Send + 'static>),
}

struct DownloadEntry {
    browser: BrowserId,
    cef_id: u32,
    before_callback: Option<BeforeDownloadCallback>,
    item_callback: Option<DownloadItemCallback>,
    confirm_claimed: bool,
    confirmed: bool,
    cancel_requested: bool,
    #[cfg(test)]
    test_cancel: Option<Box<dyn FnOnce() + Send + 'static>>,
}

struct DownloadBook {
    capacity: usize,
    entries: BTreeMap<DownloadId, DownloadEntry>,
    by_cef_id: BTreeMap<(BrowserId, u32), DownloadId>,
    consumed_confirm: BTreeSet<DownloadId>,
    retired: BTreeSet<DownloadId>,
    cancelled_cef_ids: BTreeSet<(BrowserId, u32)>,
}

impl DownloadBook {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.clamp(1, MAX_HOST_REQUEST_LEDGER_ENTRIES),
            entries: BTreeMap::new(),
            by_cef_id: BTreeMap::new(),
            consumed_confirm: BTreeSet::new(),
            retired: BTreeSet::new(),
            cancelled_cef_ids: BTreeSet::new(),
        }
    }

    fn has_retained_callbacks(&self) -> bool {
        self.entries
            .values()
            .any(|e| e.before_callback.is_some() || e.item_callback.is_some())
    }

    fn mark_cancelled_cef(&mut self, key: (BrowserId, u32)) {
        mark_bounded(
            &mut self.cancelled_cef_ids,
            key,
            MAX_HOST_REQUEST_LEDGER_ENTRIES,
        );
    }

    fn should_cancel_untracked_update(
        &mut self,
        key: (BrowserId, u32),
        state: DownloadState,
        download_enabled: bool,
    ) -> bool {
        if state != DownloadState::InProgress {
            self.cancelled_cef_ids.remove(&key);
            return false;
        }
        !download_enabled || self.cancelled_cef_ids.contains(&key)
    }
}

pub(crate) struct InFlightCallbackGuard<'a> {
    counter: &'a AtomicUsize,
}

impl Drop for InFlightCallbackGuard<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) struct CancelledDownloads {
    before_callbacks: Vec<BeforeDownloadCallback>,
    item_callbacks: Vec<DownloadItemCallback>,
    cancelled_confirmed_ids: Vec<DownloadId>,
    #[cfg(test)]
    test_cancels: Vec<Box<dyn FnOnce() + Send + 'static>>,
}

impl CancelledDownloads {
    pub(crate) fn finish(self, audit: &NativeAudit, caps: &HostCapabilities) {
        drop(self.before_callbacks);
        for cb in self.item_callbacks {
            audit.record();
            cb.cancel();
        }
        #[cfg(test)]
        for cancel in self.test_cancels {
            cancel();
        }
        if !self.cancelled_confirmed_ids.is_empty()
            && let Some(svc) = caps.download.as_ref()
        {
            for id in self.cancelled_confirmed_ids {
                svc.on_download_updated(DownloadProgress {
                    id,
                    state: DownloadState::Canceled,
                    received_bytes: 0,
                    total_bytes: None,
                    current_speed: 0,
                    percent_complete: -1,
                    full_path: None,
                });
            }
        }
    }
}

pub(crate) struct HostRequestManager {
    next_seq: AtomicU64,
    in_flight_callbacks: AtomicUsize,
    file_dialogs: Mutex<CallbackTable<FileDialogReply>>,
    js_dialogs: Mutex<CallbackTable<JsDialogReply>>,
    permissions: Mutex<CallbackTable<PermissionReply>>,
    auths: Mutex<CallbackTable<AuthReply>>,
    certificates: Mutex<CallbackTable<CertificateReply>>,
    downloads: Mutex<DownloadBook>,
}

impl HostRequestManager {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            next_seq: AtomicU64::new(1),
            in_flight_callbacks: AtomicUsize::new(0),
            file_dialogs: Mutex::new(CallbackTable::new(MAX_HOST_REQUEST_LEDGER_ENTRIES)),
            js_dialogs: Mutex::new(CallbackTable::new(MAX_HOST_REQUEST_LEDGER_ENTRIES)),
            permissions: Mutex::new(CallbackTable::new(MAX_HOST_REQUEST_LEDGER_ENTRIES)),
            auths: Mutex::new(CallbackTable::new(MAX_HOST_REQUEST_LEDGER_ENTRIES)),
            certificates: Mutex::new(CallbackTable::new(MAX_HOST_REQUEST_LEDGER_ENTRIES)),
            downloads: Mutex::new(DownloadBook::new(MAX_HOST_REQUEST_LEDGER_ENTRIES)),
        })
    }

    pub(crate) fn enter_callback(&self) -> InFlightCallbackGuard<'_> {
        self.in_flight_callbacks.fetch_add(1, Ordering::AcqRel);
        InFlightCallbackGuard {
            counter: &self.in_flight_callbacks,
        }
    }

    fn alloc_id(&self, browser: BrowserId) -> Result<HostRequestId, HostCapabilityError> {
        let sequence = self.next_seq.fetch_add(1, Ordering::Relaxed);
        if sequence == 0 {
            return Err(HostCapabilityError::QueueFull);
        }
        Ok(HostRequestId { browser, sequence })
    }

    pub(crate) fn pending_count(&self) -> u64 {
        (lock(&self.file_dialogs).len()
            + lock(&self.js_dialogs).len()
            + lock(&self.permissions).len()
            + lock(&self.auths).len()
            + lock(&self.certificates).len()
            + lock(&self.downloads).entries.len()) as u64
    }

    pub(crate) fn is_clear(&self) -> bool {
        self.in_flight_callbacks.load(Ordering::Acquire) == 0
            && lock(&self.file_dialogs).len() == 0
            && lock(&self.js_dialogs).len() == 0
            && lock(&self.permissions).len() == 0
            && lock(&self.auths).len() == 0
            && lock(&self.certificates).len() == 0
            && {
                let book = lock(&self.downloads);
                book.entries.is_empty() && !book.has_retained_callbacks()
            }
    }

    // ---- Registration helpers (called from CEF callbacks) ----

    pub(crate) fn register_file_dialog(
        &self,
        browser: BrowserId,
        callback: FileDialogCallback,
    ) -> Result<HostRequestId, (FileDialogCallback, HostCapabilityError)> {
        let id = match self.alloc_id(browser) {
            Ok(id) => id,
            Err(err) => return Err((callback, err)),
        };
        lock(&self.file_dialogs)
            .insert(id, browser, FileDialogReply::Native(callback))
            .map(|()| id)
            .map_err(|(reply, err)| match reply {
                FileDialogReply::Native(cb) => (cb, err),
            })
    }

    pub(crate) fn register_js_dialog(
        &self,
        browser: BrowserId,
        callback: JsdialogCallback,
    ) -> Result<HostRequestId, (JsdialogCallback, HostCapabilityError)> {
        let id = match self.alloc_id(browser) {
            Ok(id) => id,
            Err(err) => return Err((callback, err)),
        };
        lock(&self.js_dialogs)
            .insert(id, browser, JsDialogReply::Native(callback))
            .map(|()| id)
            .map_err(|(reply, err)| match reply {
                JsDialogReply::Native(cb) => (cb, err),
                #[cfg(test)]
                _ => unreachable!(),
            })
    }

    pub(crate) fn register_permission(
        &self,
        browser: BrowserId,
        reply: PermissionReply,
    ) -> Result<HostRequestId, (PermissionReply, HostCapabilityError)> {
        let id = match self.alloc_id(browser) {
            Ok(id) => id,
            Err(err) => return Err((reply, err)),
        };
        lock(&self.permissions)
            .insert(id, browser, reply)
            .map(|()| id)
    }

    pub(crate) fn register_auth(
        &self,
        browser: BrowserId,
        callback: AuthCallback,
    ) -> Result<HostRequestId, (AuthCallback, HostCapabilityError)> {
        let id = match self.alloc_id(browser) {
            Ok(id) => id,
            Err(err) => return Err((callback, err)),
        };
        lock(&self.auths)
            .insert(id, browser, AuthReply::Native(callback))
            .map(|()| id)
            .map_err(|(reply, err)| match reply {
                AuthReply::Native(cb) => (cb, err),
            })
    }

    pub(crate) fn register_certificate(
        &self,
        browser: BrowserId,
        reply: CertificateReply,
    ) -> Result<HostRequestId, (CertificateReply, HostCapabilityError)> {
        let id = match self.alloc_id(browser) {
            Ok(id) => id,
            Err(err) => return Err((reply, err)),
        };
        lock(&self.certificates)
            .insert(id, browser, reply)
            .map(|()| id)
    }

    pub(crate) fn register_download(
        &self,
        browser: BrowserId,
        cef_id: u32,
        before_callback: BeforeDownloadCallback,
    ) -> Result<DownloadId, (BeforeDownloadCallback, HostCapabilityError)> {
        let sequence = self.next_seq.fetch_add(1, Ordering::Relaxed);
        if sequence == 0 {
            return Err((before_callback, HostCapabilityError::QueueFull));
        }
        let id = DownloadId {
            browser,
            cef_id,
            sequence,
        };
        let mut book = lock(&self.downloads);
        if book.entries.len() >= book.capacity || book.by_cef_id.contains_key(&(browser, cef_id)) {
            if !book.by_cef_id.contains_key(&(browser, cef_id)) {
                book.mark_cancelled_cef((browser, cef_id));
            }
            return Err((before_callback, HostCapabilityError::QueueFull));
        }
        book.cancelled_cef_ids.remove(&(browser, cef_id));
        book.by_cef_id.insert((browser, cef_id), id);
        book.entries.insert(
            id,
            DownloadEntry {
                browser,
                cef_id,
                before_callback: Some(before_callback),
                item_callback: None,
                confirm_claimed: false,
                confirmed: false,
                cancel_requested: false,
                #[cfg(test)]
                test_cancel: None,
            },
        );
        Ok(id)
    }

    // ---- Download update routing ----

    pub(crate) fn on_download_updated(
        &self,
        browser: BrowserId,
        item_info: DownloadUpdateSnapshot,
        callback: Option<DownloadItemCallback>,
        audit: &NativeAudit,
        caps: &HostCapabilities,
    ) {
        let state = item_info.state;
        let (tracked_id, prev_cb, cancel_cb, unconfirmed_cancel) = {
            let mut book = lock(&self.downloads);
            let dl_id = book.by_cef_id.get(&(browser, item_info.cef_id)).copied();
            if let Some(dl_id) = dl_id {
                let (prev_cb, cancel_now, remove_now) = if let Some(entry) =
                    book.entries.get_mut(&dl_id)
                {
                    let prev = entry.item_callback.take();
                    let terminal = state != DownloadState::InProgress;
                    let cancel_now = entry.cancel_requested && state == DownloadState::InProgress;
                    if !terminal && !cancel_now {
                        entry.item_callback = callback.clone();
                    }
                    (prev, cancel_now, terminal)
                } else {
                    (None, false, true)
                };
                if remove_now {
                    book.entries.remove(&dl_id);
                    book.by_cef_id.remove(&(browser, item_info.cef_id));
                    mark_consumed(&mut book.retired, dl_id);
                }
                let cancel_cb = if cancel_now { callback } else { None };
                (Some(dl_id), prev_cb, cancel_cb, None)
            } else {
                let download_enabled = caps.download.is_some();
                let cancel_cb = if book.should_cancel_untracked_update(
                    (browser, item_info.cef_id),
                    state,
                    download_enabled,
                ) {
                    callback
                } else {
                    None
                };
                (None, None, None, cancel_cb)
            }
        };

        drop(prev_cb);
        if let Some(cb) = unconfirmed_cancel {
            audit.record();
            cb.cancel();
            return;
        }
        if let Some(cb) = cancel_cb {
            audit.record();
            cb.cancel();
        }
        if let Some(id) = tracked_id
            && let Some(ref svc) = caps.download
        {
            svc.on_download_updated(DownloadProgress {
                id,
                state,
                received_bytes: item_info.received_bytes,
                total_bytes: item_info.total_bytes,
                current_speed: item_info.current_speed,
                percent_complete: item_info.percent_complete,
                full_path: item_info.full_path,
            });
        }
    }

    // ---- Synchronous caller claim / validation ----

    pub(crate) fn claim_confirm_download(&self, id: DownloadId) -> Result<(), HostCapabilityError> {
        let mut book = lock(&self.downloads);
        if book.consumed_confirm.contains(&id) {
            return Err(HostCapabilityError::AlreadyConsumed);
        }
        let Some(entry) = book.entries.get_mut(&id) else {
            return if book.retired.contains(&id) {
                Err(HostCapabilityError::AlreadyConsumed)
            } else {
                Err(HostCapabilityError::UnknownOrStaleToken)
            };
        };
        if entry.cancel_requested {
            return Err(HostCapabilityError::Cancelled);
        }
        if entry.confirm_claimed || entry.confirmed || entry.before_callback.is_none() {
            return Err(HostCapabilityError::AlreadyConsumed);
        }
        entry.confirm_claimed = true;
        Ok(())
    }

    pub(crate) fn claim_cancel_download(&self, id: DownloadId) -> Result<(), HostCapabilityError> {
        let mut book = lock(&self.downloads);
        let Some(entry) = book.entries.get_mut(&id) else {
            return if book.retired.contains(&id) || book.consumed_confirm.contains(&id) {
                Err(HostCapabilityError::AlreadyConsumed)
            } else {
                Err(HostCapabilityError::UnknownOrStaleToken)
            };
        };
        if entry.cancel_requested {
            return Err(HostCapabilityError::AlreadyConsumed);
        }
        entry.cancel_requested = true;
        mark_consumed(&mut book.consumed_confirm, id);
        Ok(())
    }

    pub(crate) fn claim_file_dialog(
        &self,
        id: HostRequestId,
        decision: &FileDialogDecision,
    ) -> Result<(), HostCapabilityError> {
        validate_file_dialog_decision(decision)?;
        lock(&self.file_dialogs).claim_if(id, |_| Ok(()))
    }

    pub(crate) fn claim_js_dialog(&self, id: HostRequestId) -> Result<(), HostCapabilityError> {
        lock(&self.js_dialogs).claim_if(id, |_| Ok(()))
    }

    pub(crate) fn claim_permission(&self, id: HostRequestId) -> Result<(), HostCapabilityError> {
        lock(&self.permissions).claim_if(id, |_| Ok(()))
    }

    pub(crate) fn claim_auth(&self, id: HostRequestId) -> Result<(), HostCapabilityError> {
        lock(&self.auths).claim_if(id, |_| Ok(()))
    }

    pub(crate) fn claim_certificate(
        &self,
        id: HostRequestId,
        decision: CertificateDecision,
    ) -> Result<(), HostCapabilityError> {
        lock(&self.certificates).claim_if(id, |reply| {
            if let (
                CertificateReply::Client { certs, .. },
                CertificateDecision::Accept {
                    certificate_index: Some(idx),
                },
            ) = (reply, decision)
            {
                if idx >= certs.len() {
                    return Err(HostCapabilityError::InvalidPreferenceValue);
                }
            }
            Ok(())
        })
    }

    // ---- CEF UI resolution (lock-external callback execution) ----

    pub(crate) fn resolve_confirm_download(
        &self,
        id: DownloadId,
        destination: PathBuf,
        policy: &NativePolicy,
        audit: &NativeAudit,
    ) {
        let roots = policy.forbidden_roots();
        let validated = validate_download_destination(&destination, &roots);
        let (cb_to_run, cb_to_drop) = {
            let mut book = lock(&self.downloads);
            let Some(entry) = book.entries.get_mut(&id) else {
                return;
            };
            let cb = entry.before_callback.take();
            if entry.cancel_requested || validated.is_err() {
                let cef_id = entry.cef_id;
                let browser = entry.browser;
                entry.cancel_requested = true;
                mark_consumed(&mut book.consumed_confirm, id);
                mark_consumed(&mut book.retired, id);
                book.entries.remove(&id);
                book.by_cef_id.remove(&(browser, cef_id));
                book.mark_cancelled_cef((browser, cef_id));
                (None, cb)
            } else {
                entry.confirmed = true;
                mark_consumed(&mut book.consumed_confirm, id);
                (cb, None)
            }
        };
        drop(cb_to_drop);
        if let (Some(cb), Ok(clean)) = (cb_to_run, validated) {
            let path_str = crate::runtime::dos_path(&clean);
            let path_cef = CefString::from(path_str.as_str());
            audit.record();
            cb.cont(Some(&path_cef), 0);
        }
    }

    pub(crate) fn resolve_cancel_download(&self, id: DownloadId, audit: &NativeAudit) {
        let (before_cb, item_cb, test_cb) = {
            let mut book = lock(&self.downloads);
            let Some((before_cb, item_cb, test_cb, confirmed, cef_id, browser)) =
                book.entries.get_mut(&id).map(|entry| {
                    entry.cancel_requested = true;
                    #[cfg(test)]
                    let test_cb = entry.test_cancel.take();
                    #[cfg(not(test))]
                    let test_cb: Option<()> = None;
                    (
                        entry.before_callback.take(),
                        entry.item_callback.take(),
                        test_cb,
                        entry.confirmed,
                        entry.cef_id,
                        entry.browser,
                    )
                })
            else {
                return;
            };
            mark_consumed(&mut book.consumed_confirm, id);
            if !confirmed || item_cb.is_none() {
                book.entries.remove(&id);
                book.by_cef_id.remove(&(browser, cef_id));
                mark_consumed(&mut book.retired, id);
                book.mark_cancelled_cef((browser, cef_id));
            }
            (before_cb, item_cb, test_cb)
        };
        drop(before_cb);
        if let Some(cb) = item_cb {
            audit.record();
            cb.cancel();
        }
        #[cfg(test)]
        if let Some(test_fn) = test_cb {
            test_fn();
        }
        #[cfg(not(test))]
        let _ = test_cb;
    }

    pub(crate) fn resolve_file_dialog(
        &self,
        id: HostRequestId,
        decision: FileDialogDecision,
        audit: &NativeAudit,
    ) {
        let reply = lock(&self.file_dialogs).take(id);
        match reply {
            Some(FileDialogReply::Native(cb)) => match decision {
                FileDialogDecision::Accept(paths) => {
                    let mut list = CefStringList::new();
                    for path in paths.into_iter().take(MAX_FILE_DIALOG_PATHS) {
                        let s = crate::runtime::dos_path(&path);
                        if !s.contains('\0') {
                            let _ = list.append(s.as_str());
                        }
                    }
                    audit.record();
                    cb.cont(Some(&mut list));
                }
                FileDialogDecision::Cancel => {
                    audit.record();
                    cb.cancel();
                }
            },
            None => {}
        }
    }

    pub(crate) fn resolve_js_dialog(
        &self,
        id: HostRequestId,
        decision: JsDialogDecision,
        audit: &NativeAudit,
    ) {
        let reply = lock(&self.js_dialogs).take(id);
        match reply {
            Some(JsDialogReply::Native(cb)) => match decision {
                JsDialogDecision::Accept { prompt_input } => {
                    let cef_input = prompt_input.as_deref().map(CefString::from);
                    audit.record();
                    cb.cont(1, cef_input.as_ref());
                }
                JsDialogDecision::Cancel => {
                    audit.record();
                    cb.cont(0, None);
                }
            },
            #[cfg(test)]
            Some(JsDialogReply::Test(f)) => match decision {
                JsDialogDecision::Accept { prompt_input } => f(true, prompt_input),
                JsDialogDecision::Cancel => f(false, None),
            },
            None => {}
        }
    }

    pub(crate) fn resolve_permission(
        &self,
        id: HostRequestId,
        decision: PermissionDecision,
        audit: &NativeAudit,
    ) {
        let reply = lock(&self.permissions).take(id);
        match reply {
            Some(PermissionReply::Media(cb)) => {
                audit.record();
                match decision {
                    PermissionDecision::Allow { media_mask } => cb.cont(media_mask),
                    PermissionDecision::Deny
                    | PermissionDecision::Dismiss
                    | PermissionDecision::Ignore => cb.cancel(),
                }
            }
            Some(PermissionReply::Prompt(cb)) => {
                let mapped = match decision {
                    PermissionDecision::Allow { .. } => PermissionRequestResult::ACCEPT,
                    PermissionDecision::Deny => PermissionRequestResult::DENY,
                    PermissionDecision::Dismiss => PermissionRequestResult::DISMISS,
                    PermissionDecision::Ignore => PermissionRequestResult::IGNORE,
                };
                audit.record();
                cb.cont(mapped);
            }
            None => {}
        }
    }

    pub(crate) fn resolve_auth(
        &self,
        id: HostRequestId,
        decision: AuthDecision,
        audit: &NativeAudit,
    ) {
        let reply = lock(&self.auths).take(id);
        match reply {
            Some(AuthReply::Native(cb)) => match decision {
                AuthDecision::Continue(creds) => {
                    let user = CefString::from(creds.username.expose());
                    let pass = CefString::from(creds.password.expose());
                    audit.record();
                    cb.cont(Some(&user), Some(&pass));
                }
                AuthDecision::Cancel => {
                    audit.record();
                    cb.cancel();
                }
            },
            None => {}
        }
    }

    pub(crate) fn resolve_certificate(
        &self,
        id: HostRequestId,
        decision: CertificateDecision,
        audit: &NativeAudit,
    ) {
        let reply = lock(&self.certificates).take(id);
        match reply {
            Some(CertificateReply::Server(cb)) => {
                audit.record();
                match decision {
                    CertificateDecision::Accept { .. } => cb.cont(),
                    CertificateDecision::Cancel => cb.cancel(),
                }
            }
            Some(CertificateReply::Client {
                callback,
                mut certs,
            }) => {
                audit.record();
                match decision {
                    CertificateDecision::Accept {
                        certificate_index: Some(idx),
                    } => callback.select(certs.get_mut(idx)),
                    CertificateDecision::Accept {
                        certificate_index: None,
                    }
                    | CertificateDecision::Cancel => callback.select(None),
                }
                drop(certs);
            }
            #[cfg(test)]
            Some(CertificateReply::Test(f)) => f(decision),
            None => {}
        }
    }

    // ---- Cancellation & teardown drains (lock-external) ----

    pub(crate) fn cancel_js_dialogs_for_browser(
        &self,
        browser: BrowserId,
        audit: &NativeAudit,
        caps: &HostCapabilities,
    ) {
        let drained = lock(&self.js_dialogs).drain_browser(browser);
        cancel_js_dialog_list(drained, audit, caps);
    }

    pub(crate) fn cancel_navigation_prompts_for_browser(
        &self,
        browser: BrowserId,
        audit: &NativeAudit,
        caps: &HostCapabilities,
    ) {
        let files = lock(&self.file_dialogs).drain_browser(browser);
        let perms = lock(&self.permissions).drain_browser(browser);
        cancel_file_dialog_list(files, audit, caps);
        cancel_permission_list(perms, audit, caps);
    }

    pub(crate) fn cancel_non_download_for_browser(
        &self,
        browser: BrowserId,
        audit: &NativeAudit,
        caps: &HostCapabilities,
    ) {
        let files = lock(&self.file_dialogs).drain_browser(browser);
        let js = lock(&self.js_dialogs).drain_browser(browser);
        let perms = lock(&self.permissions).drain_browser(browser);
        let auths = lock(&self.auths).drain_browser(browser);
        let certs = lock(&self.certificates).drain_browser(browser);
        cancel_file_dialog_list(files, audit, caps);
        cancel_js_dialog_list(js, audit, caps);
        cancel_permission_list(perms, audit, caps);
        cancel_auth_list(auths, audit, caps);
        cancel_certificate_list(certs, audit, caps);
    }

    pub(crate) fn cancel_browser_all(
        &self,
        browser: BrowserId,
        drop_confirmed_without_item: bool,
        audit: &NativeAudit,
        caps: &HostCapabilities,
    ) {
        self.cancel_non_download_for_browser(browser, audit, caps);
        let downloads = self.extract_browser_downloads(browser, drop_confirmed_without_item);
        downloads.finish(audit, caps);
    }

    pub(crate) fn cancel_all(&self, audit: &NativeAudit, caps: &HostCapabilities) {
        let files = lock(&self.file_dialogs).drain_all();
        let js = lock(&self.js_dialogs).drain_all();
        let perms = lock(&self.permissions).drain_all();
        let auths = lock(&self.auths).drain_all();
        let certs = lock(&self.certificates).drain_all();
        let downloads = self.extract_all_downloads();
        cancel_file_dialog_list(files, audit, caps);
        cancel_js_dialog_list(js, audit, caps);
        cancel_permission_list(perms, audit, caps);
        cancel_auth_list(auths, audit, caps);
        cancel_certificate_list(certs, audit, caps);
        downloads.finish(audit, caps);
    }

    fn extract_browser_downloads(
        &self,
        browser: BrowserId,
        drop_confirmed_without_item: bool,
    ) -> CancelledDownloads {
        let mut book = lock(&self.downloads);
        let keys: Vec<DownloadId> = book
            .entries
            .iter()
            .filter_map(|(k, v)| (v.browser == browser).then_some(*k))
            .collect();
        let mut before_callbacks = Vec::new();
        let mut item_callbacks = Vec::new();
        let mut cancelled_confirmed_ids = Vec::new();
        #[cfg(test)]
        let mut test_cancels = Vec::new();
        for key in keys {
            let extracted = book.entries.get_mut(&key).map(|entry| {
                entry.cancel_requested = true;
                #[cfg(test)]
                let tc = entry.test_cancel.take();
                #[cfg(not(test))]
                let tc: Option<()> = None;
                (
                    entry.before_callback.take(),
                    entry.item_callback.take(),
                    tc,
                    entry.confirmed,
                    entry.cef_id,
                )
            });
            if let Some((before_cb, item_cb, tc, confirmed, cef_id)) = extracted {
                mark_consumed(&mut book.consumed_confirm, key);
                if let Some(cb) = before_cb {
                    before_callbacks.push(cb);
                }
                if let Some(cb) = item_cb {
                    item_callbacks.push(cb);
                }
                #[cfg(test)]
                if let Some(tc) = tc {
                    test_cancels.push(tc);
                }
                #[cfg(not(test))]
                let _ = tc;
                if !confirmed || drop_confirmed_without_item {
                    book.entries.remove(&key);
                    book.by_cef_id.remove(&(browser, cef_id));
                    mark_consumed(&mut book.retired, key);
                    book.mark_cancelled_cef((browser, cef_id));
                    if confirmed && drop_confirmed_without_item {
                        cancelled_confirmed_ids.push(key);
                    }
                }
            }
        }
        CancelledDownloads {
            before_callbacks,
            item_callbacks,
            cancelled_confirmed_ids,
            #[cfg(test)]
            test_cancels,
        }
    }

    fn extract_all_downloads(&self) -> CancelledDownloads {
        let mut book = lock(&self.downloads);
        let entries = std::mem::take(&mut book.entries);
        book.by_cef_id.clear();
        let mut before_callbacks = Vec::new();
        let mut item_callbacks = Vec::new();
        let mut cancelled_confirmed_ids = Vec::new();
        #[cfg(test)]
        let mut test_cancels = Vec::new();
        for (key, mut entry) in entries {
            mark_consumed(&mut book.consumed_confirm, key);
            mark_consumed(&mut book.retired, key);
            book.mark_cancelled_cef((entry.browser, entry.cef_id));
            if entry.confirmed {
                cancelled_confirmed_ids.push(key);
            }
            if let Some(cb) = entry.before_callback.take() {
                before_callbacks.push(cb);
            }
            if let Some(cb) = entry.item_callback.take() {
                item_callbacks.push(cb);
            }
            #[cfg(test)]
            if let Some(tc) = entry.test_cancel.take() {
                test_cancels.push(tc);
            }
        }
        CancelledDownloads {
            before_callbacks,
            item_callbacks,
            cancelled_confirmed_ids,
            #[cfg(test)]
            test_cancels,
        }
    }
}

pub(crate) struct DownloadUpdateSnapshot {
    pub(crate) cef_id: u32,
    pub(crate) state: DownloadState,
    pub(crate) received_bytes: u64,
    pub(crate) total_bytes: Option<u64>,
    pub(crate) current_speed: i64,
    pub(crate) percent_complete: i32,
    pub(crate) full_path: Option<PathBuf>,
}

pub(crate) fn validate_file_dialog_decision(
    decision: &FileDialogDecision,
) -> Result<(), HostCapabilityError> {
    if let FileDialogDecision::Accept(paths) = decision {
        if paths.len() > MAX_FILE_DIALOG_PATHS {
            return Err(HostCapabilityError::InvalidDestination);
        }
        for path in paths {
            let s = path.to_string_lossy();
            if s.is_empty() || s.contains('\0') {
                return Err(HostCapabilityError::InvalidDestination);
            }
        }
    }
    Ok(())
}

fn cancel_file_dialog_list(
    list: Vec<(HostRequestId, FileDialogReply)>,
    audit: &NativeAudit,
    caps: &HostCapabilities,
) {
    for (id, reply) in list {
        match reply {
            FileDialogReply::Native(cb) => {
                audit.record();
                cb.cancel();
            }
        }
        if let Some(svc) = &caps.file_dialog {
            svc.on_file_dialog_cancelled(id);
        }
    }
}

fn cancel_js_dialog_list(
    list: Vec<(HostRequestId, JsDialogReply)>,
    audit: &NativeAudit,
    caps: &HostCapabilities,
) {
    for (id, reply) in list {
        match reply {
            JsDialogReply::Native(cb) => {
                audit.record();
                cb.cont(0, None);
            }
            #[cfg(test)]
            JsDialogReply::Test(f) => f(false, None),
        }
        if let Some(svc) = &caps.js_dialog {
            svc.on_js_dialog_cancelled(id);
        }
    }
}

fn cancel_permission_list(
    list: Vec<(HostRequestId, PermissionReply)>,
    audit: &NativeAudit,
    caps: &HostCapabilities,
) {
    for (id, reply) in list {
        match reply {
            PermissionReply::Media(cb) => {
                audit.record();
                cb.cancel();
            }
            PermissionReply::Prompt(cb) => {
                audit.record();
                cb.cont(PermissionRequestResult::IGNORE);
            }
        }
        if let Some(svc) = &caps.permission {
            svc.on_permission_cancelled(id);
        }
    }
}

fn cancel_auth_list(
    list: Vec<(HostRequestId, AuthReply)>,
    audit: &NativeAudit,
    caps: &HostCapabilities,
) {
    for (id, reply) in list {
        match reply {
            AuthReply::Native(cb) => {
                audit.record();
                cb.cancel();
            }
        }
        if let Some(svc) = &caps.auth {
            svc.on_auth_cancelled(id);
        }
    }
}

fn cancel_certificate_list(
    list: Vec<(HostRequestId, CertificateReply)>,
    audit: &NativeAudit,
    caps: &HostCapabilities,
) {
    for (id, reply) in list {
        match reply {
            CertificateReply::Server(cb) => {
                audit.record();
                cb.cancel();
            }
            CertificateReply::Client { callback, certs } => {
                audit.record();
                callback.select(None);
                drop(certs);
            }
            #[cfg(test)]
            CertificateReply::Test(f) => f(CertificateDecision::Cancel),
        }
        if let Some(svc) = &caps.certificate {
            svc.on_certificate_cancelled(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::{
        contracts::{RuntimeGeneration, TrustedContentPolicy},
        host_capabilities::CertificateService,
    };
    use std::sync::atomic::AtomicBool;

    fn bid(local: u64) -> BrowserId {
        BrowserId::new(RuntimeGeneration::new(1), local)
    }

    #[test]
    fn single_use_tokens_resolve_once_and_reject_stale_and_duplicates() {
        let mgr = HostRequestManager::new();
        let audit = NativeAudit::new();
        let browser = bid(1);
        let called = Arc::new(AtomicUsize::new(0));
        let last_prompt = Arc::new(Mutex::new(None));
        let called_c = called.clone();
        let prompt_c = last_prompt.clone();

        let id = mgr.alloc_id(browser).unwrap();
        lock(&mgr.js_dialogs)
            .insert(
                id,
                browser,
                JsDialogReply::Test(Box::new(move |ok, text| {
                    if ok {
                        called_c.fetch_add(1, Ordering::AcqRel);
                        *lock(&prompt_c) = text;
                    }
                })),
            )
            .ok()
            .unwrap();

        mgr.claim_js_dialog(id).unwrap();
        assert_eq!(
            mgr.claim_js_dialog(id),
            Err(HostCapabilityError::AlreadyConsumed)
        );
        mgr.resolve_js_dialog(
            id,
            JsDialogDecision::Accept {
                prompt_input: Some("ok-value".into()),
            },
            &audit,
        );
        assert_eq!(called.load(Ordering::Acquire), 1);
        assert_eq!(lock(&last_prompt).as_deref(), Some("ok-value"));
        assert_eq!(
            mgr.claim_js_dialog(id),
            Err(HostCapabilityError::AlreadyConsumed)
        );
        assert_eq!(
            mgr.claim_js_dialog(HostRequestId {
                browser,
                sequence: 9999
            }),
            Err(HostCapabilityError::UnknownOrStaleToken)
        );
        assert!(mgr.is_clear());
    }

    #[test]
    fn cancel_on_reset_or_close_notifies_service_and_blocks_late_reply() {
        struct CertObserver(Mutex<Vec<HostRequestId>>);
        impl CertificateService for CertObserver {
            fn on_certificate_cancelled(&self, id: HostRequestId) {
                lock(&self.0).push(id);
            }
        }
        let obs = Arc::new(CertObserver(Mutex::new(Vec::new())));
        let caps = HostCapabilities {
            certificate: Some(obs.clone()),
            ..Default::default()
        };
        let mgr = HostRequestManager::new();
        let audit = NativeAudit::new();
        let browser = bid(2);
        let id = mgr.alloc_id(browser).unwrap();
        let resolved_cancel = Arc::new(AtomicBool::new(false));
        let flag = resolved_cancel.clone();
        lock(&mgr.certificates)
            .insert(
                id,
                browser,
                CertificateReply::Test(Box::new(move |d| {
                    if matches!(d, CertificateDecision::Cancel) {
                        flag.store(true, Ordering::Release);
                    }
                })),
            )
            .ok()
            .unwrap();

        mgr.cancel_browser_all(browser, true, &audit, &caps);
        assert!(resolved_cancel.load(Ordering::Acquire));
        assert_eq!(*lock(&obs.0), vec![id]);
        assert_eq!(
            mgr.claim_certificate(id, CertificateDecision::Cancel),
            Err(HostCapabilityError::AlreadyConsumed)
        );
        assert!(mgr.is_clear());
    }

    #[test]
    fn download_cancel_runs_once_and_drains_cleanly() {
        let mgr = HostRequestManager::new();
        let audit = NativeAudit::new();
        let caps = HostCapabilities::default();
        let _rules = Arc::new(
            crate::policy::CompiledPolicy::compile(&TrustedContentPolicy {
                allowed_origins: vec![],
                allow_embedded_data: true,
            })
            .unwrap(),
        );
        let browser = bid(3);
        let id = DownloadId {
            browser,
            cef_id: 7,
            sequence: 10,
        };
        let canceled = Arc::new(AtomicUsize::new(0));
        let canceled_c = canceled.clone();
        lock(&mgr.downloads).by_cef_id.insert((browser, 7), id);
        lock(&mgr.downloads).entries.insert(
            id,
            DownloadEntry {
                browser,
                cef_id: 7,
                before_callback: None,
                item_callback: None,
                confirm_claimed: true,
                confirmed: true,
                cancel_requested: false,
                test_cancel: Some(Box::new(move || {
                    canceled_c.fetch_add(1, Ordering::AcqRel);
                })),
            },
        );
        mgr.claim_cancel_download(id).unwrap();
        assert_eq!(
            mgr.claim_cancel_download(id),
            Err(HostCapabilityError::AlreadyConsumed)
        );
        mgr.resolve_cancel_download(id, &audit);
        assert_eq!(canceled.load(Ordering::Acquire), 1);
        mgr.cancel_browser_all(browser, true, &audit, &caps);
        assert!(mgr.is_clear());
    }

    #[test]
    fn untracked_download_update_only_cancels_when_denied_or_cancelled() {
        let mut book = DownloadBook::new(8);
        let key = (bid(11), 42);
        // Pre-OnBeforeDownload InProgress update with opt-in capability: do NOT cancel.
        assert!(!book.should_cancel_untracked_update(key, DownloadState::InProgress, true));
        // Default-deny (`download_enabled = false`): always cancel InProgress updates.
        assert!(book.should_cancel_untracked_update(key, DownloadState::InProgress, false));
        // Explicitly cancelled/rejected cef_id: cancel trailing InProgress updates even when enabled.
        book.mark_cancelled_cef(key);
        assert!(book.should_cancel_untracked_update(key, DownloadState::InProgress, true));
        // Terminal state clears cancelled cef_id tracking.
        assert!(!book.should_cancel_untracked_update(key, DownloadState::Canceled, true));
        assert!(!book.should_cancel_untracked_update(key, DownloadState::InProgress, true));
    }

    #[test]
    fn cancel_browser_all_retires_confirmed_download_and_notifies_canceled() {
        use rust_cef_core::host_capabilities::DownloadService;
        struct ProgressObs(Mutex<Vec<DownloadProgress>>);
        impl DownloadService for ProgressObs {
            fn on_before_download(
                &self,
                _request: rust_cef_core::host_capabilities::DownloadRequest,
            ) {
            }
            fn on_download_updated(&self, progress: DownloadProgress) {
                lock(&self.0).push(progress);
            }
        }
        let mgr = HostRequestManager::new();
        let audit = NativeAudit::new();
        let obs = Arc::new(ProgressObs(Mutex::new(Vec::new())));
        let caps = HostCapabilities {
            download: Some(obs.clone()),
            ..Default::default()
        };
        let browser = bid(12);
        let id = DownloadId {
            browser,
            cef_id: 99,
            sequence: 20,
        };
        lock(&mgr.downloads).by_cef_id.insert((browser, 99), id);
        lock(&mgr.downloads).entries.insert(
            id,
            DownloadEntry {
                browser,
                cef_id: 99,
                before_callback: None,
                item_callback: None,
                confirm_claimed: true,
                confirmed: true,
                cancel_requested: false,
                test_cancel: None,
            },
        );
        mgr.cancel_browser_all(browser, true, &audit, &caps);
        assert_eq!(mgr.pending_count(), 0);
        assert!(mgr.is_clear());
        assert_eq!(
            mgr.claim_cancel_download(id),
            Err(HostCapabilityError::AlreadyConsumed)
        );
        assert!(lock(&mgr.downloads).should_cancel_untracked_update(
            (browser, 99),
            DownloadState::InProgress,
            true
        ));
        let updates = lock(&obs.0);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].id, id);
        assert_eq!(updates[0].state, DownloadState::Canceled);
    }
}
