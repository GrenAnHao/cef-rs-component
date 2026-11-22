//! One counted original RequestContext wrapper, not a profile metadata guess.
//! Rust Arc checkouts never AddRef CEF. Last original-UI Drop releases the wrapper
//! BEFORE its book gate; client/native/window gates cannot retire across it.
use super::{AuxiliaryId, DevToolsLedger};
use crate::{menu_bridge::Scope, registry::UiRegistry};
use cef::*;
use rust_cef_core::ports::EngineError;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(crate) struct ExpectedContext<T = RequestContext> {
    id: AuxiliaryId,
    value: Option<T>,
    book: Arc<DevToolsLedger>,
    ui: std::thread::ThreadId,
}
impl<T> ExpectedContext<T> {
    pub fn id(&self) -> AuxiliaryId {
        self.id
    }
    fn from_counted(book: Arc<DevToolsLedger>, id: AuxiliaryId, value: T) -> Arc<Self> {
        Arc::new(Self {
            id,
            value: Some(value),
            book,
            ui: std::thread::current().id(),
        })
    }
    #[cfg(test)]
    fn adopt(
        book: Arc<DevToolsLedger>,
        id: AuxiliaryId,
        value: T,
    ) -> Result<Arc<Self>, EngineError> {
        book.attach_context(id)?;
        Ok(Self::from_counted(book, id, value))
    }
}
impl<T> Drop for ExpectedContext<T> {
    fn drop(&mut self) {
        let value = self.value.take();
        if self.ui != std::thread::current().id() {
            std::mem::forget(value);
            self.book.fault();
            return;
        }
        drop(value); // Actual original-context Release, no outer/data lock.
        if self.book.context_released(self.id).is_err() {
            self.book.fault();
        }
    }
}
impl ExpectedContext {
    pub fn capture(
        registry: &Arc<UiRegistry>,
        id: AuxiliaryId,
        scope: &Scope,
    ) -> Result<Arc<Self>, EngineError> {
        registry.preflight_auxiliary_menu_preparation(scope, id)?;
        registry.note_auxiliary_native_call();
        if currently_on(ThreadId::UI) == 0 {
            return Err(EngineError::InvalidRequest);
        }
        let (browser, frame) = registry
            .menu_original_frame(scope)
            .ok_or(EngineError::Closed)?;
        registry.note_auxiliary_native_call();
        let host = browser.host().ok_or(EngineError::Closed)?;
        let api = unsafe { host.get_raw().as_ref() }.ok_or(EngineError::InvalidRequest)?;
        if api.get_request_context.is_none() {
            return Err(EngineError::InvalidRequest);
        }
        registry.note_auxiliary_native_call();
        let context = host.request_context().ok_or(EngineError::NotReady)?;
        // Metadata/parent identity may have changed during a getter. Never adopt
        // another Browser or default/global context to make capture succeed.
        if !scope.current_native_epoch()
            || !registry.menu_original_frame_valid(scope, &browser, &frame)
        {
            return Err(EngineError::Closed);
        }
        drop(host);
        drop(frame);
        drop(browser);
        // Those final Release calls can reenter and revoke the original scope.
        // Keep the uncounted owned value on this stack until the pure Registry
        // gate either counts it or rejects it; in the latter case it drops
        // only after the Registry guard has returned.
        registry.attach_auxiliary_menu_context(scope, id)?;
        Ok(Self::from_counted(registry.devtools.clone(), id, context))
    }
    /// Scope-free capture for a host-initiated open. Uses the live original
    /// Browser's own RequestContext; never profile metadata, never the global
    /// context. Native identity is re-checked after the getter, and the counted
    /// value is released only on the owning UI thread via Drop.
    pub fn capture_host(
        registry: &Arc<UiRegistry>,
        id: AuxiliaryId,
        browser: &Browser,
        native_id: i32,
    ) -> Result<Arc<Self>, EngineError> {
        // reserve() already admitted this id; preflight here would reject the
        // row it just created (it requires the owner to have no pending row).
        registry.note_auxiliary_native_call();
        if currently_on(ThreadId::UI) == 0 || native_id <= 0 {
            return Err(EngineError::InvalidRequest);
        }
        if browser.is_valid() == 0 || browser.identifier() != native_id {
            return Err(EngineError::InvalidRequest);
        }
        let host = browser.host().ok_or(EngineError::Closed)?;
        let api = unsafe { host.get_raw().as_ref() }.ok_or(EngineError::InvalidRequest)?;
        if api.get_request_context.is_none() {
            return Err(EngineError::InvalidRequest);
        }
        registry.note_auxiliary_native_call();
        let context = host.request_context().ok_or(EngineError::NotReady)?;
        if browser.is_valid() == 0 || browser.identifier() != native_id {
            return Err(EngineError::Closed);
        }
        drop(host);
        registry.devtools.attach_context(id)?;
        Ok(Self::from_counted(registry.devtools.clone(), id, context))
    }
    pub fn verify(
        &self,
        registry: &UiRegistry,
        native_id: i32,
        browser: &Browser,
        closed: &AtomicBool,
    ) -> Result<bool, EngineError> {
        if self.ui != std::thread::current().id() || closed.load(Ordering::Acquire) {
            return Err(EngineError::Closed);
        }
        registry.note_auxiliary_native_call();
        if currently_on(ThreadId::UI) == 0 {
            return Err(EngineError::InvalidRequest);
        }
        registry.note_auxiliary_native_call();
        if browser.is_valid() == 0 || browser.identifier() != native_id {
            return Err(EngineError::InvalidRequest);
        }
        if closed.load(Ordering::Acquire) {
            return Err(EngineError::Closed);
        }
        registry.note_auxiliary_native_call();
        let host = browser.host().ok_or(EngineError::Closed)?;
        let api = unsafe { host.get_raw().as_ref() }.ok_or(EngineError::InvalidRequest)?;
        if api.get_request_context.is_none() {
            return Err(EngineError::InvalidRequest);
        }
        if closed.load(Ordering::Acquire) {
            return Err(EngineError::Closed);
        }
        registry.note_auxiliary_native_call();
        let actual = host.request_context().ok_or(EngineError::NotReady)?;
        drop(host);
        let expected = self.value.as_ref().ok_or(EngineError::Closed)?;
        registry.note_auxiliary_native_call();
        compare_contexts(expected, actual)
    }
}
/// Caller qualifies the production UI sequence. Tests use only Rust-owned CABI
/// stubs, not an initialized CEF context. Keep ABI ownership in the fixed wrapper.
fn compare_contexts(
    expected: &RequestContext,
    actual: RequestContext,
) -> Result<bool, EngineError> {
    let raw = ImplRequestContext::get_raw(&actual);
    if unsafe { raw.as_ref() }.is_none_or(|a| a.is_same.is_none()) {
        return Err(EngineError::InvalidRequest);
    }
    // IsSharingWith/cache-path equality is NOT a substitute. The fixed cef-rs
    // wrapper AddRefs its &mut argument for CAPI ownership transfer. A raw borrow
    // would consume the owner's reference. UI-local clone/drop remains outside
    // locks and precedes the last counted owner gate.
    let mut expected_argument = expected.clone();
    let matches = actual.is_same(Some(&mut expected_argument)) != 0;
    drop(expected_argument);
    drop(actual);
    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::contracts::{BrowserId, ProfileHandle, RuntimeGeneration};
    use std::{num::NonZeroUsize, sync::atomic::AtomicUsize};
    struct Probe {
        book: Arc<DevToolsLedger>,
        order: Arc<AtomicUsize>,
        id: AuxiliaryId,
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            let s = self
                .book
                .state
                .try_lock()
                .expect("No book lock during native-value Drop");
            assert!(s.rows.get(&self.id.owner).unwrap().context_live);
            assert_eq!(self.order.fetch_add(1, Ordering::AcqRel), 0);
        }
    }
    fn fixture() -> (Arc<DevToolsLedger>, AuxiliaryId, Arc<AtomicUsize>) {
        let b = Arc::new(DevToolsLedger::new(RuntimeGeneration::new(1)));
        b.configure(NonZeroUsize::new(1).unwrap()).unwrap();
        let id = b
            .reserve(BrowserId::new(RuntimeGeneration::new(1), 1), 1, 101)
            .unwrap();
        (b, id, Arc::new(AtomicUsize::new(0)))
    }
    struct AbiState {
        refs: AtomicUsize,
        adds: AtomicUsize,
        drops: AtomicUsize,
        seen: AtomicUsize,
        unlocked: AtomicBool,
        book: Arc<DevToolsLedger>,
    }
    #[repr(C)]
    struct AbiContext {
        raw: cef::sys::_cef_request_context_t,
        state: Arc<AbiState>,
    }
    fn abi_state(book: &Arc<DevToolsLedger>) -> Arc<AbiState> {
        Arc::new(AbiState {
            refs: AtomicUsize::new(1),
            adds: AtomicUsize::new(0),
            drops: AtomicUsize::new(0),
            seen: AtomicUsize::new(0),
            unlocked: AtomicBool::new(true),
            book: book.clone(),
        })
    }
    fn note_unlocked(state: &AbiState) {
        if state.book.state.try_lock().is_err() {
            state.unlocked.store(false, Ordering::Release);
        }
    }
    unsafe extern "C" fn abi_add(base: *mut cef::sys::_cef_base_ref_counted_t) {
        let state = &unsafe { &*(base as *mut AbiContext) }.state;
        note_unlocked(state);
        state.adds.fetch_add(1, Ordering::AcqRel);
        state.refs.fetch_add(1, Ordering::AcqRel);
    }
    unsafe extern "C" fn abi_release(base: *mut cef::sys::_cef_base_ref_counted_t) -> i32 {
        let state = unsafe { &*(base as *mut AbiContext) }.state.clone();
        note_unlocked(&state);
        if state.refs.fetch_sub(1, Ordering::AcqRel) == 1 {
            drop(unsafe { Box::from_raw(base as *mut AbiContext) });
            state.drops.fetch_add(1, Ordering::AcqRel);
            1
        } else {
            0
        }
    }
    unsafe extern "C" fn abi_consuming_same(
        _this: *mut cef::sys::_cef_request_context_t,
        other: *mut cef::sys::_cef_request_context_t,
    ) -> i32 {
        let state = unsafe { &*(other as *mut AbiContext) }.state.clone();
        state
            .seen
            .store(state.refs.load(Ordering::Acquire), Ordering::Release);
        // Deliberately model the transferred argument reference being consumed.
        // This is a Rust CABI stub, NOT the real CEF IsSame implementation.
        unsafe { abi_release(other as *mut cef::sys::_cef_base_ref_counted_t) };
        1
    }
    fn abi_context(state: Arc<AbiState>, with_same: bool) -> RequestContext {
        use cef::rc::ConvertReturnValue;
        assert_eq!(std::mem::offset_of!(AbiContext, raw), 0);
        assert_eq!(
            std::mem::offset_of!(cef::sys::_cef_request_context_t, base),
            0
        );
        assert_eq!(
            std::mem::offset_of!(cef::sys::_cef_preference_manager_t, base),
            0
        );
        let mut raw: cef::sys::_cef_request_context_t = unsafe { std::mem::zeroed() };
        raw.base.base.size = std::mem::size_of::<cef::sys::_cef_request_context_t>();
        raw.base.base.add_ref = Some(abi_add);
        raw.base.base.release = Some(abi_release);
        if with_same {
            raw.is_same = Some(abi_consuming_same);
        }
        let owned = Box::into_raw(Box::new(AbiContext { raw, state }));
        // Transfer the one initial owned reference via the SDK conversion, never
        // transmute the private RequestContext wrapper or forge a native Browser.
        (owned as *mut cef::sys::_cef_request_context_t).wrap_result()
    }
    #[test]
    fn auxiliary_context_comparison_uses_sdk_transferred_argument_and_rejects_missing_api() {
        let (b, id, _) = fixture();
        let expected_state = abi_state(&b);
        let expected =
            ExpectedContext::adopt(b.clone(), id, abi_context(expected_state.clone(), false))
                .unwrap();
        b.abandon_preissue(id).unwrap();
        let actual_state = abi_state(&b);
        assert_eq!(
            compare_contexts(
                expected.value.as_ref().unwrap(),
                abi_context(actual_state.clone(), true)
            ),
            Ok(true)
        );
        assert_eq!(expected_state.seen.load(Ordering::Acquire), 3);
        assert_eq!(expected_state.adds.load(Ordering::Acquire), 2);
        assert_eq!(expected_state.refs.load(Ordering::Acquire), 1);
        assert_eq!(expected_state.drops.load(Ordering::Acquire), 0);
        assert_eq!(actual_state.drops.load(Ordering::Acquire), 1);
        assert_eq!(b.pending(), 1);
        let missing_state = abi_state(&b);
        assert_eq!(
            compare_contexts(
                expected.value.as_ref().unwrap(),
                abi_context(missing_state.clone(), false)
            ),
            Err(EngineError::InvalidRequest)
        );
        assert_eq!(missing_state.drops.load(Ordering::Acquire), 1);
        assert_eq!(expected_state.adds.load(Ordering::Acquire), 2);
        drop(expected);
        assert_eq!(expected_state.drops.load(Ordering::Acquire), 1);
        assert_eq!(b.pending(), 0);
        assert!(expected_state.unlocked.load(Ordering::Acquire));
        assert!(actual_state.unlocked.load(Ordering::Acquire));
    }
    #[test]
    fn auxiliary_context_extra_arc_holds_abandoned_reservation_until_value_drop() {
        let (b, id, order) = fixture();
        let context = ExpectedContext::adopt(
            b.clone(),
            id,
            Probe {
                book: b.clone(),
                order: order.clone(),
                id,
            },
        )
        .unwrap();
        let extra = context.clone();
        assert_eq!(b.rollback(id), Err(EngineError::InvalidRequest));
        b.abandon_preissue(id).unwrap();
        drop(context);
        assert_eq!(b.pending(), 1);
        assert_eq!(order.load(Ordering::Acquire), 0);
        drop(extra);
        assert_eq!(order.load(Ordering::Acquire), 1);
        assert_eq!(b.pending(), 0);
        assert!(!b.faulted());
    }
    #[test]
    fn auxiliary_context_release_is_an_independent_issued_retirement_gate() {
        let (b, id, order) = fixture();
        let context = ExpectedContext::adopt(
            b.clone(),
            id,
            Probe {
                book: b.clone(),
                order: order.clone(),
                id,
            },
        )
        .unwrap();
        let profile = ProfileHandle::new(RuntimeGeneration::new(1), 7);
        b.attach_prepared_client(id, profile).unwrap();
        b.issued(id).unwrap();
        b.created(id, 201).unwrap();
        b.record_context_match(id, 201, Some(true)).unwrap();
        assert_eq!(
            b.record_context_match(id, 201, Some(false)),
            Err(EngineError::InvalidRequest)
        );
        b.before_close(id, 201).unwrap();
        b.native_references_released(id, 201).unwrap();
        b.host_resources_released(id, 201).unwrap();
        b.client_returned(id, 201).unwrap();
        assert_eq!(b.pending(), 1);
        drop(context);
        assert_eq!(order.load(Ordering::Acquire), 1);
        assert_eq!(b.pending(), 0);
    }
    #[test]
    fn auxiliary_context_and_prepared_client_release_orders_both_keep_reserved_gate() {
        for context_first in [false, true] {
            let (b, id, order) = fixture();
            let context = ExpectedContext::adopt(
                b.clone(),
                id,
                Probe {
                    book: b.clone(),
                    order: order.clone(),
                    id,
                },
            )
            .unwrap();
            let profile = ProfileHandle::new(RuntimeGeneration::new(1), 7);
            b.attach_prepared_client(id, profile).unwrap();
            b.abandon_preissue(id).unwrap();
            if context_first {
                drop(context);
                assert_eq!(b.pending(), 1);
                assert!(b.prepared_client_released(id, profile).unwrap());
            } else {
                assert!(b.prepared_client_released(id, profile).unwrap());
                assert_eq!(b.pending(), 1);
                drop(context);
            }
            assert_eq!(order.load(Ordering::Acquire), 1);
            assert_eq!(b.pending(), 0);
            assert!(!b.faulted());
        }
    }
    #[test]
    fn auxiliary_context_wrong_thread_forgets_value_and_preserves_faulted_gate() {
        let (b, id, order) = fixture();
        let context = ExpectedContext::adopt(
            b.clone(),
            id,
            Probe {
                book: b.clone(),
                order: order.clone(),
                id,
            },
        )
        .unwrap();
        b.abandon_preissue(id).unwrap();
        std::thread::spawn(move || drop(context)).join().unwrap();
        assert_eq!(order.load(Ordering::Acquire), 0);
        assert!(b.faulted());
        assert_eq!(b.pending(), 2);
    }
}
