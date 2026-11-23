//! Durable auxiliary Browser ownership. Index/checkout locks only move or clone
//! Rust Arcs. Actual wrapper adoption/final Release never hold these locks.
use super::{AuxiliaryCloseTicket, AuxiliaryId, DevToolsLedger};
use crate::registry::UiRegistry;
use cef::*;
use rust_cef_core::{
    contracts::{BrowserId, ProfileHandle},
    ports::EngineError,
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

// The same retirement helper is checked with non-native Drop probes. These
// probes do NOT establish actual CEF final-release behavior or UI ordering.
fn release_on_ui<T>(value: Option<T>, on_ui: bool, after: impl FnOnce()) -> bool {
    if !on_ui {
        std::mem::forget(value);
        return false;
    }
    drop(value);
    after();
    true
}
struct NativeReference {
    browser: Option<Browser>,
    ui: std::thread::ThreadId,
    released: Arc<AtomicBool>,
    book: Arc<DevToolsLedger>,
}
impl Drop for NativeReference {
    fn drop(&mut self) {
        let browser = self.browser.take();
        if !release_on_ui(browser, self.ui == std::thread::current().id(), || {
            self.released.store(true, Ordering::Release);
        }) {
            self.book.fault();
        }
    }
}
struct NativeResource {
    id: AuxiliaryId,
    native_id: i32,
    _profile: ProfileHandle,
    browser: Mutex<Option<Arc<NativeReference>>>,
    released: Arc<AtomicBool>,
    ui: std::thread::ThreadId,
    retiring: AtomicBool,
    registry: Weak<UiRegistry>,
    book: Arc<DevToolsLedger>,
}
impl NativeResource {
    fn close(&self, registry: &UiRegistry) -> Result<(), EngineError> {
        if self.ui != std::thread::current().id() {
            return Err(EngineError::InvalidRequest);
        }
        if self.retiring.load(Ordering::Acquire) {
            return Ok(());
        }
        let reference = {
            self.browser
                .lock()
                .unwrap_or_else(|e| {
                    self.book.fault();
                    e.into_inner()
                })
                .as_ref()
                .cloned()
        }
        .ok_or(EngineError::Closed)?;
        let browser = reference.browser.as_ref().ok_or(EngineError::Closed)?;
        registry.note_auxiliary_native_call();
        if browser.is_valid() == 0 {
            return Err(EngineError::Closed);
        }
        if self.retiring.load(Ordering::Acquire) {
            return Ok(());
        }
        registry.note_auxiliary_native_call();
        if browser.identifier() != self.native_id {
            return Err(EngineError::InvalidRequest);
        }
        if self.retiring.load(Ordering::Acquire) {
            return Ok(());
        }
        registry.note_auxiliary_native_call();
        let host = browser.host().ok_or(EngineError::Closed)?;
        // Do not pin a Browser wrapper across the call that may synchronously
        // trigger its own OnBeforeClose. The Resource lease tracks host scope.
        drop(reference);
        if self.retiring.load(Ordering::Acquire) {
            return Ok(());
        }
        registry.note_auxiliary_native_call();
        host.close_browser(1);
        Ok(())
    }
}
impl Drop for NativeResource {
    fn drop(&mut self) {
        let reference = self
            .browser
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let completed = release_on_ui(reference, self.ui == std::thread::current().id(), || {
            if !self.retiring.load(Ordering::Acquire) || !self.released.load(Ordering::Acquire) {
                self.book.fault();
                return;
            }
            let Some(registry) = self.registry.upgrade() else {
                self.book.fault();
                return;
            };
            if registry
                .post_auxiliary_native_released(self.id, self.native_id)
                .is_err()
            {
                self.book.fault();
            }
        });
        if !completed {
            self.book.fault();
        }
    }
}
pub(crate) struct NativeAuxiliaries {
    rows: Mutex<BTreeMap<BrowserId, Arc<NativeResource>>>,
    book: Arc<DevToolsLedger>,
}
impl NativeAuxiliaries {
    pub fn new(book: Arc<DevToolsLedger>) -> Self {
        Self {
            rows: Mutex::new(BTreeMap::new()),
            book,
        }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<BrowserId, Arc<NativeResource>>> {
        self.rows.lock().unwrap_or_else(|e| {
            self.book.fault();
            e.into_inner()
        })
    }
    /// Caller supplies the original callback's already-cloned wrapper on CEF UI.
    /// No parent Browser or extra RequestContext is retained. There is one real
    /// wrapper; every index/temporary checkout clones a Rust Arc only.
    pub fn adopt(
        &self,
        registry: &Arc<UiRegistry>,
        id: AuxiliaryId,
        native_id: i32,
        browser: Browser,
    ) -> Result<(), EngineError> {
        let profile = self.book.resource_profile(id, native_id)?;
        let ui = std::thread::current().id();
        let released = Arc::new(AtomicBool::new(false));
        let reference = Arc::new(NativeReference {
            browser: Some(browser),
            ui,
            released: released.clone(),
            book: self.book.clone(),
        });
        let resource = Arc::new(NativeResource {
            id,
            native_id,
            _profile: profile,
            browser: Mutex::new(Some(reference)),
            released,
            ui,
            retiring: AtomicBool::new(false),
            registry: Arc::downgrade(registry),
            book: self.book.clone(),
        });
        let inserted = {
            let mut rows = self.lock();
            if rows.contains_key(&id.owner) {
                false
            } else {
                rows.insert(id.owner, resource.clone());
                true
            }
        };
        if !inserted {
            drop(resource);
            return Err(EngineError::Backpressure);
        }
        drop(resource);
        self.book.native_adopted(id, native_id)
    }
    /// Actual matching auxiliary OnBeforeClose, even if the data ledger faulted.
    pub fn retire(&self, id: AuxiliaryId, native_id: i32) -> Result<(), EngineError> {
        let resource = {
            let mut rows = self.lock();
            if rows
                .get(&id.owner)
                .is_some_and(|r| r.id == id && r.native_id == native_id)
            {
                rows.remove(&id.owner)
            } else {
                None
            }
        }
        .ok_or(EngineError::InvalidRequest)?;
        resource.retiring.store(true, Ordering::Release);
        let reference = {
            resource
                .browser
                .lock()
                .unwrap_or_else(|e| {
                    self.book.fault();
                    e.into_inner()
                })
                .take()
        };
        drop(reference); // Last real Browser wrapper releases OUTSIDE both locks.
        drop(resource); // A close task's transient Host lease may still be alive.
        Ok(())
    }
    pub fn close(
        &self,
        registry: &UiRegistry,
        ticket: AuxiliaryCloseTicket,
    ) -> Result<(), EngineError> {
        let resource = {
            let rows = self.lock();
            rows.get(&ticket.id.owner)
                .filter(|r| r.id == ticket.id && r.native_id == ticket.native_id)
                .cloned()
        }
        .ok_or(EngineError::InvalidRequest)?;
        resource.close(registry)
    }
    pub fn is_empty(&self) -> bool {
        let empty = self.lock().is_empty();
        empty && !self.book.faulted()
    }
}
impl Drop for NativeAuxiliaries {
    fn drop(&mut self) {
        // Unexpected teardown is not OnBeforeClose. Do not run native Release on
        // Registry's destructor thread or manufacture a successful observation.
        let rows = std::mem::take(
            self.rows
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if !rows.is_empty() {
            self.book.fault();
            std::mem::forget(rows);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    struct Probe {
        gate: Arc<Mutex<()>>,
        order: Arc<AtomicUsize>,
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            assert!(self.gate.try_lock().is_ok());
            assert_eq!(self.order.fetch_add(1, Ordering::AcqRel), 0);
        }
    }
    #[test]
    fn auxiliary_resource_drop_precedes_notice_and_holds_no_outer_lock() {
        let gate = Arc::new(Mutex::new(()));
        let order = Arc::new(AtomicUsize::new(0));
        let slot = Mutex::new(Some(Probe {
            gate: gate.clone(),
            order: order.clone(),
        }));
        let value = { slot.lock().unwrap().take() };
        assert!(release_on_ui(value, true, || {
            assert!(gate.try_lock().is_ok());
            assert!(slot.try_lock().is_ok());
            assert_eq!(order.fetch_add(1, Ordering::AcqRel), 1);
        }));
        assert_eq!(order.load(Ordering::Acquire), 2);
    }
    #[test]
    fn auxiliary_resource_wrong_thread_release_abandons_without_notice() {
        let gate = Arc::new(Mutex::new(()));
        let order = Arc::new(AtomicUsize::new(0));
        let notice = Arc::new(AtomicUsize::new(0));
        let probe = Probe {
            gate,
            order: order.clone(),
        };
        let seen = notice.clone();
        let ui = std::thread::current().id();
        assert!(
            !std::thread::spawn(move || release_on_ui(
                Some(probe),
                std::thread::current().id() == ui,
                || {
                    seen.fetch_add(1, Ordering::AcqRel);
                }
            ))
            .join()
            .unwrap()
        );
        assert_eq!(order.load(Ordering::Acquire), 0);
        assert_eq!(notice.load(Ordering::Acquire), 0);
    }
}
