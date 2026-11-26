//! Opt-in headless Chrome DevTools Protocol (CDP) bridge, completely separate
//! from visual managed DevTools UI ([`crate::devtools`]).
//!
//! Disabled by default unless [`BrowserOptions::headless_cdp_enabled`] is true.
//! Every `cef::Registration` is kept exclusively on the CEF UI sequence and
//! dropped there during detach / `OnBeforeClose` / `DrainRequests` before
//! `cef::shutdown`.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicI32, AtomicU64, Ordering},
    },
};

use cef::*;
use rust_cef_core::{
    contracts::BrowserId,
    host_capabilities::{
        CdpEvent, CdpMethodId, CdpMethodResult, CdpObserver, HostCapabilityError,
        MAX_CDP_JSON_BYTES, validate_cdp_json, validate_cdp_method_name,
    },
};

use crate::executor::NativeAudit;

/// Dedicated bounded ceiling (32 MiB) for transient `Page.captureScreenshot`
/// responses in `NativeChildHwnd`, kept separate from the 512 KiB general CDP cap.
pub(crate) const MAX_CDP_SCREENSHOT_JSON_BYTES: usize = 32 * 1024 * 1024;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CdpDiagnostics {
    pub attached: u64,
    pub detached: u64,
    pub dropped_on_ui: u64,
    pub active_registrations: u64,
    pub rejected_off_ui: u64,
    pub oversized_dropped: u64,
}

enum RegistrationSlot {
    Native(#[allow(dead_code)] Registration),
    #[cfg(test)]
    Test(Arc<std::sync::atomic::AtomicBool>),
}

impl Drop for RegistrationSlot {
    fn drop(&mut self) {
        #[cfg(test)]
        if let Self::Test(dropped) = self {
            dropped.store(true, Ordering::Release);
        }
    }
}

pub(crate) struct HeadlessCdpManager {
    next_message_id: AtomicI32,
    next_transient_token: AtomicU64,
    registrations: Mutex<BTreeMap<BrowserId, RegistrationSlot>>,
    pending_observers: Mutex<BTreeMap<BrowserId, Arc<dyn CdpObserver>>>,
    transient_registrations: Mutex<BTreeMap<(BrowserId, u64), RegistrationSlot>>,
    pending_transient_observers: Mutex<BTreeMap<(BrowserId, u64), Arc<dyn CdpObserver>>>,
    attached: AtomicU64,
    detached: AtomicU64,
    dropped_on_ui: AtomicU64,
    rejected_off_ui: AtomicU64,
    oversized_dropped: Arc<AtomicU64>,
}

impl HeadlessCdpManager {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            next_message_id: AtomicI32::new(1),
            next_transient_token: AtomicU64::new(1),
            registrations: Mutex::new(BTreeMap::new()),
            pending_observers: Mutex::new(BTreeMap::new()),
            transient_registrations: Mutex::new(BTreeMap::new()),
            pending_transient_observers: Mutex::new(BTreeMap::new()),
            attached: AtomicU64::new(0),
            detached: AtomicU64::new(0),
            dropped_on_ui: AtomicU64::new(0),
            rejected_off_ui: AtomicU64::new(0),
            oversized_dropped: Arc::new(AtomicU64::new(0)),
        })
    }

    pub(crate) fn alloc_transient_token(&self) -> u64 {
        self.next_transient_token.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn set_pending_transient_observer(
        &self,
        browser: BrowserId,
        token: u64,
        observer: Arc<dyn CdpObserver>,
    ) {
        lock(&self.pending_transient_observers).insert((browser, token), observer);
    }

    pub(crate) fn take_pending_transient_observer(
        &self,
        browser: BrowserId,
        token: u64,
    ) -> Option<Arc<dyn CdpObserver>> {
        lock(&self.pending_transient_observers).remove(&(browser, token))
    }

    pub(crate) fn set_pending_observer(&self, browser: BrowserId, observer: Arc<dyn CdpObserver>) {
        lock(&self.pending_observers).insert(browser, observer);
    }

    pub(crate) fn take_pending_observer(&self, browser: BrowserId) -> Option<Arc<dyn CdpObserver>> {
        lock(&self.pending_observers).remove(&browser)
    }

    pub(crate) fn alloc_message_id(
        &self,
        browser: BrowserId,
    ) -> Result<CdpMethodId, HostCapabilityError> {
        let raw = self.next_message_id.fetch_add(1, Ordering::Relaxed);
        let message_id = (raw & 0x3fff_ffff).max(1);
        Ok(CdpMethodId {
            browser,
            message_id,
        })
    }

    pub(crate) fn active_registrations(&self) -> u64 {
        lock(&self.registrations).len() as u64 + lock(&self.transient_registrations).len() as u64
    }

    pub(crate) fn diagnostics(&self) -> CdpDiagnostics {
        CdpDiagnostics {
            attached: self.attached.load(Ordering::Acquire),
            detached: self.detached.load(Ordering::Acquire),
            dropped_on_ui: self.dropped_on_ui.load(Ordering::Acquire),
            active_registrations: self.active_registrations(),
            rejected_off_ui: self.rejected_off_ui.load(Ordering::Acquire),
            oversized_dropped: self.oversized_dropped.load(Ordering::Acquire),
        }
    }

    pub(crate) fn attach_observer(
        &self,
        on_ui: bool,
        browser: BrowserId,
        host: &BrowserHost,
        observer: Arc<dyn CdpObserver>,
        audit: &NativeAudit,
    ) -> Result<(), HostCapabilityError> {
        if !on_ui {
            self.rejected_off_ui.fetch_add(1, Ordering::AcqRel);
            return Err(HostCapabilityError::Cancelled);
        }
        let mut native_obs = RuntimeDevToolsObserver::new(
            browser,
            observer,
            self.oversized_dropped.clone(),
            MAX_CDP_JSON_BYTES,
        );
        audit.record();
        let registration = host.add_dev_tools_message_observer(Some(&mut native_obs));
        drop(native_obs);
        let Some(registration) = registration else {
            return Err(HostCapabilityError::OperationFailed);
        };
        let previous =
            lock(&self.registrations).insert(browser, RegistrationSlot::Native(registration));
        self.attached.fetch_add(1, Ordering::AcqRel);
        if previous.is_some() {
            self.detached.fetch_add(1, Ordering::AcqRel);
            self.dropped_on_ui.fetch_add(1, Ordering::AcqRel);
        }
        drop(previous);
        Ok(())
    }

    pub(crate) fn attach_transient_screenshot_observer(
        &self,
        on_ui: bool,
        browser: BrowserId,
        token: u64,
        host: &BrowserHost,
        observer: Arc<dyn CdpObserver>,
        audit: &NativeAudit,
    ) -> Result<(), HostCapabilityError> {
        if !on_ui {
            self.rejected_off_ui.fetch_add(1, Ordering::AcqRel);
            return Err(HostCapabilityError::Cancelled);
        }
        let mut native_obs = RuntimeDevToolsObserver::new(
            browser,
            observer,
            self.oversized_dropped.clone(),
            MAX_CDP_SCREENSHOT_JSON_BYTES,
        );
        audit.record();
        let registration = host.add_dev_tools_message_observer(Some(&mut native_obs));
        drop(native_obs);
        let Some(registration) = registration else {
            return Err(HostCapabilityError::OperationFailed);
        };
        let previous = lock(&self.transient_registrations)
            .insert((browser, token), RegistrationSlot::Native(registration));
        self.attached.fetch_add(1, Ordering::AcqRel);
        if previous.is_some() {
            self.detached.fetch_add(1, Ordering::AcqRel);
            self.dropped_on_ui.fetch_add(1, Ordering::AcqRel);
        }
        drop(previous);
        Ok(())
    }

    pub(crate) fn detach_transient_observer(&self, on_ui: bool, browser: BrowserId, token: u64) {
        if !on_ui {
            self.rejected_off_ui.fetch_add(1, Ordering::AcqRel);
            return;
        }
        lock(&self.pending_transient_observers).remove(&(browser, token));
        let removed = lock(&self.transient_registrations).remove(&(browser, token));
        if removed.is_some() {
            self.detached.fetch_add(1, Ordering::AcqRel);
            self.dropped_on_ui.fetch_add(1, Ordering::AcqRel);
        }
        drop(removed);
    }

    pub(crate) fn detach_browser(&self, on_ui: bool, browser: BrowserId) {
        if !on_ui {
            self.rejected_off_ui.fetch_add(1, Ordering::AcqRel);
            return;
        }
        lock(&self.pending_observers).remove(&browser);
        lock(&self.pending_transient_observers).retain(|(b, _), _| *b != browser);
        let removed = lock(&self.registrations).remove(&browser);
        if removed.is_some() {
            self.detached.fetch_add(1, Ordering::AcqRel);
            self.dropped_on_ui.fetch_add(1, Ordering::AcqRel);
        }
        drop(removed);
        let mut transient_removed = Vec::new();
        {
            let mut guard = lock(&self.transient_registrations);
            let keys: Vec<(BrowserId, u64)> = guard
                .keys()
                .copied()
                .filter(|(b, _)| *b == browser)
                .collect();
            for key in keys {
                if let Some(slot) = guard.remove(&key) {
                    transient_removed.push(slot);
                }
            }
        }
        let transient_count = transient_removed.len() as u64;
        if transient_count > 0 {
            self.detached.fetch_add(transient_count, Ordering::AcqRel);
            self.dropped_on_ui
                .fetch_add(transient_count, Ordering::AcqRel);
        }
        drop(transient_removed);
    }

    pub(crate) fn detach_all(&self, on_ui: bool) {
        if !on_ui {
            self.rejected_off_ui.fetch_add(1, Ordering::AcqRel);
            return;
        }
        lock(&self.pending_observers).clear();
        lock(&self.pending_transient_observers).clear();
        let mut removed: Vec<RegistrationSlot> = std::mem::take(&mut *lock(&self.registrations))
            .into_values()
            .collect();
        removed.extend(std::mem::take(&mut *lock(&self.transient_registrations)).into_values());
        let count = removed.len() as u64;
        if count > 0 {
            self.detached.fetch_add(count, Ordering::AcqRel);
            self.dropped_on_ui.fetch_add(count, Ordering::AcqRel);
        }
        drop(removed);
    }

    pub(crate) fn send_message(
        &self,
        on_ui: bool,
        host: &BrowserHost,
        json_message: &str,
        audit: &NativeAudit,
    ) -> Result<(), HostCapabilityError> {
        if !on_ui {
            self.rejected_off_ui.fetch_add(1, Ordering::AcqRel);
            return Err(HostCapabilityError::Cancelled);
        }
        validate_cdp_json(json_message)?;
        audit.record();
        if host.send_dev_tools_message(Some(json_message.as_bytes())) != 0 {
            Ok(())
        } else {
            Err(HostCapabilityError::OperationFailed)
        }
    }

    pub(crate) fn execute_method(
        &self,
        on_ui: bool,
        host: &BrowserHost,
        method: &str,
        params_json: Option<&str>,
        message_id: i32,
        audit: &NativeAudit,
    ) -> Result<(), HostCapabilityError> {
        if !on_ui {
            self.rejected_off_ui.fetch_add(1, Ordering::AcqRel);
            return Err(HostCapabilityError::Cancelled);
        }
        validate_cdp_method_name(method)?;
        let mut dict_params: Option<DictionaryValue> = if let Some(raw) = params_json {
            validate_cdp_json(raw)?;
            let trimmed = raw.trim();
            if !trimmed.starts_with('{') {
                return Err(HostCapabilityError::InvalidCdpPayload);
            }
            let parsed = cef::parse_json(Some(&CefString::from(trimmed)), JsonParserOptions::RFC);
            let Some(dict) = parsed.and_then(|v| v.dictionary()) else {
                return Err(HostCapabilityError::InvalidCdpPayload);
            };
            Some(dict)
        } else {
            None
        };
        let cef_method = CefString::from(method);
        audit.record();
        let returned_id =
            host.execute_dev_tools_method(message_id, Some(&cef_method), dict_params.as_mut());
        drop(dict_params);
        if returned_id > 0 {
            Ok(())
        } else {
            Err(HostCapabilityError::OperationFailed)
        }
    }
}

wrap_dev_tools_message_observer! {
    struct RuntimeDevToolsObserver {
        browser: BrowserId,
        observer: Arc<dyn CdpObserver>,
        oversized_dropped: Arc<AtomicU64>,
        max_bytes: usize,
    }

    impl DevToolsMessageObserver {
        fn on_dev_tools_message(
            &self,
            _browser: Option<&mut Browser>,
            _message: Option<&[u8]>,
        ) -> ::std::os::raw::c_int {
            // Returning 0 allows CEF to parse and dispatch structured
            // `on_dev_tools_method_result` and `on_dev_tools_event`.
            0
        }

        fn on_dev_tools_method_result(
            &self,
            _browser: Option<&mut Browser>,
            message_id: ::std::os::raw::c_int,
            success: ::std::os::raw::c_int,
            result: Option<&[u8]>,
        ) {
            let bytes = result.unwrap_or(&[]);
            if bytes.len() > self.max_bytes {
                self.oversized_dropped.fetch_add(1, Ordering::AcqRel);
                self.observer.on_method_result(CdpMethodResult {
                    id: CdpMethodId {
                        browser: self.browser,
                        message_id,
                    },
                    success: false,
                    result_json: r#"{"error":"cdp result exceeded 512 KiB"}"#.into(),
                });
                return;
            }
            let result_json = String::from_utf8_lossy(bytes).into_owned();
            self.observer.on_method_result(CdpMethodResult {
                id: CdpMethodId {
                    browser: self.browser,
                    message_id,
                },
                success: success != 0,
                result_json,
            });
        }

        fn on_dev_tools_event(
            &self,
            _browser: Option<&mut Browser>,
            method: Option<&CefString>,
            params: Option<&[u8]>,
        ) {
            let bytes = params.unwrap_or(&[]);
            if bytes.len() > self.max_bytes {
                self.oversized_dropped.fetch_add(1, Ordering::AcqRel);
                return;
            }
            let method = method.map(ToString::to_string).unwrap_or_default();
            let params_json = String::from_utf8_lossy(bytes).into_owned();
            self.observer.on_event(CdpEvent {
                browser: self.browser,
                method,
                params_json,
            });
        }

        fn on_dev_tools_agent_attached(&self, _browser: Option<&mut Browser>) {
            self.observer.on_attached(self.browser);
        }

        fn on_dev_tools_agent_detached(&self, _browser: Option<&mut Browser>) {
            self.observer.on_detached(self.browser);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::contracts::RuntimeGeneration;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn registrations_refuse_off_ui_drop_and_release_cleanly_on_ui() {
        let mgr = HeadlessCdpManager::new();
        let browser = BrowserId::new(RuntimeGeneration::new(1), 8);
        let dropped = Arc::new(AtomicBool::new(false));
        lock(&mgr.registrations).insert(browser, RegistrationSlot::Test(dropped.clone()));
        mgr.attached.fetch_add(1, Ordering::AcqRel);

        mgr.detach_browser(false, browser);
        assert!(!dropped.load(Ordering::Acquire));
        assert_eq!(mgr.active_registrations(), 1);
        assert_eq!(mgr.diagnostics().rejected_off_ui, 1);

        mgr.detach_browser(true, browser);
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(mgr.active_registrations(), 0);
        assert_eq!(mgr.diagnostics().dropped_on_ui, 1);
    }

    #[test]
    fn transient_registrations_detach_on_ui_and_browser_close() {
        let mgr = HeadlessCdpManager::new();
        let browser = BrowserId::new(RuntimeGeneration::new(1), 10);
        let tok1 = mgr.alloc_transient_token();
        let tok2 = mgr.alloc_transient_token();
        let dropped1 = Arc::new(AtomicBool::new(false));
        let dropped2 = Arc::new(AtomicBool::new(false));
        lock(&mgr.transient_registrations)
            .insert((browser, tok1), RegistrationSlot::Test(dropped1.clone()));
        lock(&mgr.transient_registrations)
            .insert((browser, tok2), RegistrationSlot::Test(dropped2.clone()));
        mgr.attached.fetch_add(2, Ordering::AcqRel);
        assert_eq!(mgr.active_registrations(), 2);

        mgr.detach_transient_observer(false, browser, tok1);
        assert!(!dropped1.load(Ordering::Acquire));
        assert_eq!(mgr.active_registrations(), 2);

        mgr.detach_transient_observer(true, browser, tok1);
        assert!(dropped1.load(Ordering::Acquire));
        assert_eq!(mgr.active_registrations(), 1);

        mgr.detach_browser(true, browser);
        assert!(dropped2.load(Ordering::Acquire));
        assert_eq!(mgr.active_registrations(), 0);
    }

    #[test]
    fn oversize_cdp_observer_payload_is_bounded_and_counted() {
        struct Capture(Mutex<Vec<CdpMethodResult>>, Mutex<Vec<CdpEvent>>);
        impl CdpObserver for Capture {
            fn on_method_result(&self, result: CdpMethodResult) {
                lock(&self.0).push(result);
            }
            fn on_event(&self, event: CdpEvent) {
                lock(&self.1).push(event);
            }
        }
        let cap = Arc::new(Capture(Mutex::new(Vec::new()), Mutex::new(Vec::new())));
        let counter = Arc::new(AtomicU64::new(0));
        let browser = BrowserId::new(RuntimeGeneration::new(1), 9);
        let obs =
            RuntimeDevToolsObserver::new(browser, cap.clone(), counter.clone(), MAX_CDP_JSON_BYTES);

        let huge = vec![b'a'; MAX_CDP_JSON_BYTES + 1];
        obs.on_dev_tools_method_result(None, 3, 1, Some(&huge));
        obs.on_dev_tools_event(
            None,
            Some(&CefString::from("Runtime.consoleAPICalled")),
            Some(&huge),
        );
        assert_eq!(counter.load(Ordering::Acquire), 2);
        let results = lock(&cap.0);
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert!(results[0].result_json.contains("512 KiB"));
        assert!(lock(&cap.1).is_empty());
    }
}
