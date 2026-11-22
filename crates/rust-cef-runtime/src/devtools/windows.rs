//! Native Chrome DevTools window accounting, not a Show service. The fixed CEF
//! creation path rejects OSR. Capture is AFTER explicit Show returns, from the
//! original auxiliary Host only; OnAfterCreated need not yet have an HWND.
//! No subclass/property/DestroyWindow/activation/input/timer or retry is installed.
use super::{AuxiliaryId, DevToolsLedger};
use crate::registry::UiRegistry;
use rust_cef_core::{contracts::BrowserId, ports::EngineError};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    thread::ThreadId,
};

type Hwnd = *mut std::ffi::c_void;
#[link(name = "user32")]
unsafe extern "system" {
    fn IsWindow(hwnd: Hwnd) -> i32;
    fn GetWindowThreadProcessId(hwnd: Hwnd, process: *mut u32) -> u32;
    fn GetAncestor(hwnd: Hwnd, flags: u32) -> Hwnd;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcessId() -> u32;
    fn GetCurrentThreadId() -> u32;
}
trait WindowSystem {
    fn identity(&self) -> (u32, u32);
    fn owner(&self, hwnd: usize) -> (u32, u32);
    fn root(&self, hwnd: usize) -> usize;
    fn exists(&self, hwnd: usize) -> bool;
}
struct Windows;
impl WindowSystem for Windows {
    fn identity(&self) -> (u32, u32) {
        unsafe { (GetCurrentProcessId(), GetCurrentThreadId()) }
    }
    fn owner(&self, hwnd: usize) -> (u32, u32) {
        let mut process = 0;
        let thread = unsafe { GetWindowThreadProcessId(hwnd as Hwnd, &mut process) };
        (process, thread)
    }
    fn root(&self, hwnd: usize) -> usize {
        unsafe { GetAncestor(hwnd as Hwnd, 2) as usize }
    }
    fn exists(&self, hwnd: usize) -> bool {
        unsafe { IsWindow(hwnd as Hwnd) != 0 }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct WindowProof {
    hwnd: usize,
    process: u32,
    thread: u32,
    ui: ThreadId,
}
impl WindowProof {
    fn capture(hwnd: usize, system: &impl WindowSystem) -> Result<Self, EngineError> {
        if hwnd == 0 {
            return Err(EngineError::NotReady);
        }
        let (process, thread) = system.identity();
        if process == 0
            || thread == 0
            || system.owner(hwnd) != (process, thread)
            || system.root(hwnd) != hwnd
        {
            return Err(EngineError::InvalidRequest);
        }
        // Recheck after qualification; these samples are NOT an atomic identity
        // guarantee. Live handle reuse remains conservative pending, never success.
        if !system.exists(hwnd) {
            return Err(EngineError::NotReady);
        }
        Ok(Self {
            hwnd,
            process,
            thread,
            ui: std::thread::current().id(),
        })
    }
    fn absent(self, system: &impl WindowSystem) -> Result<bool, EngineError> {
        if self.ui != std::thread::current().id() {
            return Err(EngineError::InvalidRequest);
        }
        if system.identity() != (self.process, self.thread) {
            return Err(EngineError::InvalidRequest);
        }
        // A destroyed handle cannot answer owner/root queries. Only this invalid
        // numeric-handle sample admits retirement, not HWND/PID/TID equality.
        if !system.exists(self.hwnd) {
            return Ok(true);
        }
        if system.owner(self.hwnd) != (self.process, self.thread)
            || system.root(self.hwnd) != self.hwnd
        {
            return Err(EngineError::InvalidRequest);
        }
        Ok(false)
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Record {
    id: AuxiliaryId,
    native_id: i32,
    proof: WindowProof,
}
pub(crate) struct NativeWindows {
    rows: Mutex<BTreeMap<BrowserId, Record>>,
    book: Arc<DevToolsLedger>,
}
impl NativeWindows {
    pub fn new(book: Arc<DevToolsLedger>) -> Self {
        Self {
            rows: Mutex::new(BTreeMap::new()),
            book,
        }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<BrowserId, Record>> {
        self.rows.lock().unwrap_or_else(|e| {
            self.book.fault();
            e.into_inner()
        })
    }
    /// Future Show caller must validate original menu/window/context admission
    /// and an exclusive non-Views, non-child native Chrome window role before
    /// issuing creation. Returned HWND/PID/TID/root samples do not prove that
    /// role or an unchanged HWND. This function neither issues nor focuses.
    pub fn capture(
        &self,
        registry: &UiRegistry,
        id: AuxiliaryId,
        native_id: i32,
    ) -> Result<(), EngineError> {
        self.book.window_capture_ready(id, native_id)?;
        // Native resource lookup clones Rust Arc only, native getters/release are
        // outside its locks. Never substitute the parent or GetForegroundWindow.
        let hwnd = registry
            .auxiliary_native
            .window_handle(registry, id, native_id)?;
        self.insert_observed(id, native_id, hwnd, &Windows)
    }
    fn insert_observed(
        &self,
        id: AuxiliaryId,
        native_id: i32,
        hwnd: usize,
        system: &impl WindowSystem,
    ) -> Result<(), EngineError> {
        self.book.window_capture_ready(id, native_id)?;
        let proof = WindowProof::capture(hwnd, system)?;
        self.book.window_capture_ready(id, native_id)?;
        let mut rows = self.lock();
        if rows.contains_key(&id.owner) || rows.values().any(|r| r.proof.hwnd == hwnd) {
            return Err(EngineError::Backpressure);
        }
        rows.insert(
            id.owner,
            Record {
                id,
                native_id,
                proof,
            },
        );
        Ok(())
    }
    fn original(&self, id: AuxiliaryId, native_id: i32) -> Result<Option<Record>, EngineError> {
        let record = self.lock().get(&id.owner).copied();
        if record.is_some_and(|r| r.id != id || r.native_id != native_id) {
            return Err(EngineError::InvalidRequest);
        }
        Ok(record)
    }
    /// At most one sample per explicit invocation. Tail tasks may call once each;
    /// a live window is NOT requeued, retried or retired. Explicit later sampling
    /// remains available on CEF UI. Missing capture stays pending in the ledger.
    /// Data-only rejection/no-observation takes precedence over native thread
    /// qualification, so empty/retired registries never call CEF after shutdown.
    /// Ok(false) does NOT qualify the caller or claim any native sampling.
    pub fn observe(
        &self,
        registry: &UiRegistry,
        id: AuxiliaryId,
        native_id: i32,
    ) -> Result<bool, EngineError> {
        if self.original(id, native_id)?.is_none() {
            return Ok(false);
        }
        if !self.book.window_retirement_ready(id, native_id)? {
            return Ok(false);
        }
        registry.note_auxiliary_native_call();
        if cef::currently_on(cef::ThreadId::UI) == 0 {
            return Err(EngineError::InvalidRequest);
        }
        self.observe_with(id, native_id, &Windows)
    }
    fn observe_with(
        &self,
        id: AuxiliaryId,
        native_id: i32,
        system: &impl WindowSystem,
    ) -> Result<bool, EngineError> {
        let Some(record) = self.original(id, native_id)? else {
            return Ok(false);
        };
        if !self.book.window_retirement_ready(id, native_id)? || !record.proof.absent(system)? {
            return Ok(false);
        }
        {
            let mut rows = self.lock();
            if rows.get(&id.owner) != Some(&record) {
                return Err(EngineError::InvalidRequest);
            }
            rows.remove(&id.owner);
        }
        // Index is empty before the last possible ledger gate. No index/Registry
        // lock spans book completion or Win32 calls. Restore evidence on failure.
        if let Err(error) = self.book.host_resources_released(id, native_id) {
            let mut rows = self.lock();
            if let std::collections::btree_map::Entry::Vacant(slot) = rows.entry(id.owner) {
                slot.insert(record);
            } else {
                self.book.fault();
            }
            return Err(error);
        }
        Ok(true)
    }
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}
impl Drop for NativeWindows {
    fn drop(&mut self) {
        if !self
            .rows
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
        {
            self.book.fault();
        }
        // No native destruction/guessed retirement in Drop; values are metadata.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::contracts::{ProfileHandle, RuntimeGeneration};
    use std::{cell::Cell, num::NonZeroUsize};
    struct Fake<'a> {
        windows: &'a NativeWindows,
        exists: Cell<bool>,
        owner: Cell<(u32, u32)>,
        root: Cell<usize>,
        calls: Cell<usize>,
    }
    impl<'a> Fake<'a> {
        fn new(windows: &'a NativeWindows) -> Self {
            Self {
                windows,
                exists: Cell::new(true),
                owner: Cell::new((10, 20)),
                root: Cell::new(300),
                calls: Cell::new(0),
            }
        }
        fn probe(&self) {
            assert!(self.windows.rows.try_lock().is_ok());
            assert!(self.windows.book.state.try_lock().is_ok());
            self.calls.set(self.calls.get() + 1);
        }
    }
    impl WindowSystem for Fake<'_> {
        fn identity(&self) -> (u32, u32) {
            self.probe();
            (10, 20)
        }
        fn owner(&self, _: usize) -> (u32, u32) {
            self.probe();
            self.owner.get()
        }
        fn root(&self, _: usize) -> usize {
            self.probe();
            self.root.get()
        }
        fn exists(&self, _: usize) -> bool {
            self.probe();
            self.exists.get()
        }
    }
    fn fixture() -> (Arc<DevToolsLedger>, Arc<NativeWindows>, AuxiliaryId) {
        let generation = RuntimeGeneration::new(1);
        let book = Arc::new(DevToolsLedger::new(generation));
        book.configure(NonZeroUsize::new(2).unwrap()).unwrap();
        let id = book.reserve(BrowserId::new(generation, 1), 1, 101).unwrap();
        book.issued(id).unwrap();
        book.attach_client(id, ProfileHandle::new(generation, 1))
            .unwrap();
        book.created(id, 201).unwrap();
        book.native_adopted(id, 201).unwrap();
        let windows = Arc::new(NativeWindows::new(book.clone()));
        (book, windows, id)
    }
    #[test]
    fn auxiliary_window_waits_for_both_tails_and_actual_invalid_handle_sample() {
        let (book, windows, id) = fixture();
        let fake = Fake::new(&windows);
        windows.insert_observed(id, 201, 300, &fake).unwrap();
        let calls = fake.calls.get();
        assert!(!windows.observe_with(id, 201, &fake).unwrap());
        assert_eq!(fake.calls.get(), calls);
        book.before_close(id, 201).unwrap();
        book.native_references_released(id, 201).unwrap();
        assert!(!windows.observe_with(id, 201, &fake).unwrap());
        assert_eq!(fake.calls.get(), calls);
        book.client_returned(id, 201).unwrap();
        assert!(!windows.observe_with(id, 201, &fake).unwrap());
        assert!(!windows.is_empty());
        assert_eq!(book.pending(), 1);
        fake.exists.set(false);
        fake.owner.set((0, 0));
        fake.root.set(0);
        assert!(windows.observe_with(id, 201, &fake).unwrap());
        assert!(windows.is_empty());
        assert_eq!(book.pending(), 0);
        assert!(!windows.observe_with(id, 201, &fake).unwrap());
    }
    #[test]
    fn auxiliary_window_missing_duplicate_reused_and_wrong_identity_never_retire() {
        let (book, windows, id) = fixture();
        let fake = Fake::new(&windows);
        assert_eq!(
            windows.insert_observed(id, 201, 0, &fake),
            Err(EngineError::NotReady)
        );
        assert_eq!(fake.calls.get(), 0);
        fake.owner.set((99, 20));
        assert_eq!(
            windows.insert_observed(id, 201, 300, &fake),
            Err(EngineError::InvalidRequest)
        );
        fake.owner.set((10, 20));
        fake.root.set(301);
        assert_eq!(
            windows.insert_observed(id, 201, 300, &fake),
            Err(EngineError::InvalidRequest)
        );
        fake.root.set(300);
        windows.insert_observed(id, 201, 300, &fake).unwrap();
        assert_eq!(
            windows.insert_observed(id, 201, 300, &fake),
            Err(EngineError::Backpressure)
        );
        assert_eq!(
            windows.observe_with(id, 202, &fake),
            Err(EngineError::InvalidRequest)
        );
        book.before_close(id, 201).unwrap();
        book.client_returned(id, 201).unwrap();
        book.native_references_released(id, 201).unwrap();
        // Same numeric handle/PID/thread/root is NOT proof of no reuse: alive waits.
        assert!(!windows.observe_with(id, 201, &fake).unwrap());
        fake.owner.set((99, 99));
        assert_eq!(
            windows.observe_with(id, 201, &fake),
            Err(EngineError::InvalidRequest)
        );
        assert!(!windows.is_empty());
        assert_eq!(book.pending(), 1);
        let (other, missing, other_id) = fixture();
        other.before_close(other_id, 201).unwrap();
        other.client_returned(other_id, 201).unwrap();
        other.native_references_released(other_id, 201).unwrap();
        assert!(
            !missing
                .observe_with(other_id, 201, &Fake::new(&missing))
                .unwrap()
        );
        assert_eq!(other.pending(), 1);
    }
    #[test]
    fn auxiliary_window_duplicate_hwnd_and_late_capture_keep_original_owner() {
        let (book, windows, id) = fixture();
        let fake = Fake::new(&windows);
        windows.insert_observed(id, 201, 300, &fake).unwrap();
        let peer = book
            .reserve(BrowserId::new(RuntimeGeneration::new(1), 2), 2, 102)
            .unwrap();
        book.issued(peer).unwrap();
        book.attach_client(peer, ProfileHandle::new(RuntimeGeneration::new(1), 1))
            .unwrap();
        book.created(peer, 202).unwrap();
        book.native_adopted(peer, 202).unwrap();
        assert_eq!(
            windows.insert_observed(peer, 202, 300, &fake),
            Err(EngineError::Backpressure)
        );
        assert_eq!(windows.original(id, 201).unwrap().unwrap().proof.hwnd, 300);
        book.before_close(peer, 202).unwrap();
        let calls = fake.calls.get();
        assert_eq!(
            windows.insert_observed(peer, 202, 301, &fake),
            Err(EngineError::NotReady)
        );
        assert_eq!(fake.calls.get(), calls);
        assert!(windows.original(peer, 202).unwrap().is_none());
        assert_eq!(book.pending(), 2);
    }
    #[test]
    fn auxiliary_window_wrong_thread_samples_nothing_and_fault_preserves_index() {
        let (book, windows, id) = fixture();
        let fake = Fake::new(&windows);
        windows.insert_observed(id, 201, 300, &fake).unwrap();
        book.before_close(id, 201).unwrap();
        book.client_returned(id, 201).unwrap();
        book.native_references_released(id, 201).unwrap();
        let w = windows.clone();
        std::thread::spawn(move || {
            let f = Fake::new(&w);
            assert_eq!(
                w.observe_with(id, 201, &f),
                Err(EngineError::InvalidRequest)
            );
            assert_eq!(f.calls.get(), 0);
        })
        .join()
        .unwrap();
        assert!(!windows.is_empty());
        book.fault();
        fake.exists.set(false);
        assert_eq!(
            windows.observe_with(id, 201, &fake),
            Err(EngineError::InvalidRequest)
        );
        assert!(!windows.is_empty());
        assert_eq!(book.pending(), 2);
    }
}
