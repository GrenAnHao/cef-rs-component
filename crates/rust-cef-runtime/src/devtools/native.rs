//! Dedicated lifecycle callbacks, still not a usable DevTools service. The
//! separate resource store adopts the auxiliary wrapper on UI; this Life holds
//! no Browser/RequestContext. Host/window retirement stays an independent gate.
use super::{AuxiliaryId, DevToolsLedger};
use crate::registry::UiRegistry;
use cef::*;
use rust_cef_core::{contracts::ProfileHandle, ports::EngineError};
pub(crate) mod policy;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, AtomicI32, Ordering},
};

struct Life {
    id: AuxiliaryId,
    _profile: ProfileHandle,
    book: Arc<DevToolsLedger>,
    registry: Weak<UiRegistry>,
    ui: std::thread::ThreadId,
    native: AtomicI32,
    closed: AtomicBool,
    prepared: bool,
    close_commands: policy::CloseCommands,
    expected_context: Weak<super::context::ExpectedContext>,
}
impl Life {
    fn on_ui(&self) -> bool {
        if self.ui == std::thread::current().id() {
            true
        } else {
            self.book.fault();
            false
        }
    }
    fn created(&self, native: i32) -> Result<bool, EngineError> {
        if self.native.load(Ordering::Acquire) != 0 {
            return Err(EngineError::InvalidRequest);
        }
        let close = self.book.created(self.id, native)?;
        self.native.store(native, Ordering::Release);
        Ok(close)
    }
    fn before_close(&self, native: i32) -> Result<(), EngineError> {
        if native != self.native.load(Ordering::Acquire) || native <= 0 {
            return Err(EngineError::InvalidRequest);
        }
        self.book.before_close(self.id, native)?;
        self.closed.store(true, Ordering::Release);
        Ok(())
    }
}
impl Drop for Life {
    fn drop(&mut self) {
        // Last Rust field/callback owner, not merely a local Client wrapper Drop.
        // There is no native work in this destructor, including failure paths.
        if !self.on_ui() {
            return;
        }
        if self.prepared {
            match self.book.prepared_client_released(self.id, self._profile) {
                Ok(true) => return, // No native admission ever happened.
                Ok(false) => {}     // Issued: the original full callback protocol.
                Err(_) => {
                    self.book.fault();
                    return;
                }
            }
        }
        if !self.closed.load(Ordering::Acquire) || self.book.faulted() {
            self.book.fault();
            return;
        }
        let Some(registry) = self.registry.upgrade() else {
            self.book.fault();
            return;
        };
        if registry
            .post_auxiliary_client_released(self.id, self.native.load(Ordering::Acquire))
            .is_err()
        {
            self.book.fault();
        }
    }
}
wrap_life_span_handler! {
    struct AuxiliaryLifeSpan { life: Arc<Life> }
    impl LifeSpanHandler {
        fn on_before_popup(
            &self, _browser: Option<&mut Browser>, _frame: Option<&mut Frame>, _popup_id: i32,
            _target_url: Option<&CefString>, _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition, _user_gesture: i32,
            _popup_features: Option<&PopupFeatures>, _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>, _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>, _no_javascript_access: Option<&mut i32>,
        ) -> i32 {
            let life = self.life.clone();let _ = life.on_ui();
            1 // Never inherit the SDK's default allow for ordinary webpage popups.
        }
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            let life = self.life.clone();
            if !life.on_ui() {return;}
            let Some(browser) = browser else {life.book.fault();return;};
            let Some(registry) = life.registry.upgrade() else {life.book.fault();return;};
            registry.note_auxiliary_native_call();
            let native_id=browser.identifier();
            if life.created(native_id).is_err() {life.book.fault();return;}
            // Native AddRef is outside every Registry/index/ledger lock.
            if registry.auxiliary_native.adopt(&registry,life.id,native_id,browser.clone()).is_err() {
                life.book.fault();return;
            }
            // Native context identity is independent of ProfileHandle metadata.
            // Adopt the exact auxiliary first so a mismatch can close that Browser
            // without touching the parent. Missing/late original context is never
            // replaced with global/default/cache-path/IsSharingWith guesses.
            let observed=life.expected_context.upgrade().ok_or(EngineError::NotReady)
                .and_then(|expected| expected.verify(&registry,native_id,browser,&life.closed));
            if life.book.record_context_match(life.id,native_id,observed.as_ref().ok().copied()).is_err() {life.book.fault();}
            if !matches!(observed,Ok(true)) {
                if let Some(binding)=registry.menu_bridge.get(life.id.owner).filter(|b|b.serial==life.id.serial) {
                    binding.fail("DevTools original native RequestContext not confirmed");
                }
                let _=life.book.request_close(Some(life.id.owner),Some(life.id.serial));
            }
            // Readiness follows adoption. A prior intent is caught here; a later
            // intent posts its own coalesced close, so neither ordering loses it.
            registry.schedule_auxiliary_close(life.id);
        }
        fn do_close(&self, _browser: Option<&mut Browser>) -> i32 {
            let life = self.life.clone();
            let _ = life.on_ui();
            0 // Default close handling, never a retirement observation.
        }
        fn on_before_close(&self, browser: Option<&mut Browser>) {
            let life = self.life.clone();
            if !life.on_ui() {return;}
            let Some(browser) = browser else {life.book.fault();return;};
            let Some(registry) = life.registry.upgrade() else {life.book.fault();return;};
            registry.note_auxiliary_native_call();
            let native_id=browser.identifier();
            if native_id<=0 || native_id!=life.native.load(Ordering::Acquire) {life.book.fault();return;}
            let recorded=life.before_close(native_id);
            // Release matching durable Browser refs even if the ledger faulted;
            // SDK OnBeforeClose is not permission to retain the Browser pin.
            let retired=registry.auxiliary_native.retire(life.id,native_id);
            if recorded.is_err() || retired.is_err() {life.book.fault();}
            // Our durable Browser reference is gone once retire returns; the
            // Client callback tail is queued after this handler returns. Both
            // are posted as UI tasks and never run under this callback.
            if registry
                .post_auxiliary_client_released(life.id, native_id)
                .is_err()
                || registry
                    .post_auxiliary_references_released(life.id, native_id)
                    .is_err()
            {
                life.book.fault();
            }
        }
    }
}
wrap_client! {
    struct AuxiliaryClient {life: Arc<Life>}
    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(AuxiliaryLifeSpan::new(self.life.clone()))
        }
        fn command_handler(&self) -> Option<CommandHandler> {
            Some(policy::AuxiliaryCommands::new(self.life.clone()))
        }
        fn request_handler(&self) -> Option<RequestHandler> {
            Some(policy::AuxiliaryRequests::new(self.life.clone()))
        }
        fn dialog_handler(&self) -> Option<DialogHandler> {
            Some(policy::AuxiliaryFileDialogs::new(self.life.clone()))
        }
        fn jsdialog_handler(&self) -> Option<JsdialogHandler> {
            Some(policy::AuxiliaryJsDialogs::new(self.life.clone()))
        }
        fn permission_handler(&self) -> Option<PermissionHandler> {
            Some(policy::AuxiliaryPermissions::new(self.life.clone()))
        }
    }
}

/// Internal lifecycle scaffold only. Before enabling Show, a real caller must
/// supply/verify the original window/render/policy/context and resource owner.
/// An issued slot is required; preparation failure cannot undo native admission.
pub(crate) fn build(registry: &Arc<UiRegistry>, id: AuxiliaryId) -> Result<Client, EngineError> {
    registry.devtools.client_preflight(id)?;
    let profile = registry.recorded_auxiliary_profile(id.owner)?;
    // Pending issued ownership keeps Runtime from sealing CEF/profile resources.
    if currently_on(ThreadId::UI) == 0 {
        return Err(EngineError::InvalidRequest);
    }
    let close_commands = policy::CloseCommands::resolve(registry)?;
    registry.devtools.attach_client(id, profile)?;
    Ok(AuxiliaryClient::new(Arc::new(Life {
        id,
        _profile: profile,
        book: registry.devtools.clone(),
        registry: Arc::downgrade(registry),
        ui: std::thread::current().id(),
        native: AtomicI32::new(0),
        closed: AtomicBool::new(false),
        prepared: false,
        close_commands,
        expected_context: Weak::new(),
    })))
}

/// Host-initiated Life construction without a menu scope. Still no Show: the
/// caller must already hold a counted original ExpectedContext for this id and
/// pass all native/window/policy gates before Show. Mirrors `build` but keeps
/// the real context weak instead of dropping it, and marks the slot prepared.
pub(crate) fn build_for_host(
    registry: &Arc<UiRegistry>,
    id: AuxiliaryId,
    expected_context: Weak<super::context::ExpectedContext>,
) -> Result<Client, EngineError> {
    registry.devtools.client_preflight(id)?;
    let profile = registry.recorded_auxiliary_profile(id.owner)?;
    if currently_on(ThreadId::UI) == 0 {
        return Err(EngineError::InvalidRequest);
    }
    let _context = expected_context
        .upgrade()
        .filter(|c| c.id() == id)
        .ok_or(EngineError::InvalidRequest)?;
    let close_commands = policy::CloseCommands::resolve(registry)?;
    registry.devtools.attach_client(id, profile)?;
    Ok(AuxiliaryClient::new(Arc::new(Life {
        id,
        _profile: profile,
        book: registry.devtools.clone(),
        registry: Arc::downgrade(registry),
        ui: std::thread::current().id(),
        native: AtomicI32::new(0),
        closed: AtomicBool::new(false),
        prepared: true,
        close_commands,
        expected_context,
    })))
}

/// Preparation only, still no Show, native profile/window/policy permission or
/// capability. Pure preflight precedes CEF UI qualification to avoid post-shutdown
/// CEF calls for already-retired IDs. Rollback waits for the final interface field.
pub(super) fn prepare(
    registry: &Arc<UiRegistry>,
    id: AuxiliaryId,
    scope: &crate::menu_bridge::Scope,
    expected_context: Weak<super::context::ExpectedContext>,
) -> Result<Client, EngineError> {
    registry.preflight_auxiliary_menu_preparation(scope, id)?;
    let _context = expected_context
        .upgrade()
        .filter(|c| c.id() == id)
        .ok_or(EngineError::InvalidRequest)?;
    if currently_on(ThreadId::UI) == 0 {
        return Err(EngineError::InvalidRequest);
    }
    let close_commands = policy::CloseCommands::resolve(registry)?;
    let profile = registry.attach_auxiliary_menu_client(scope, id)?;
    Ok(AuxiliaryClient::new(Arc::new(Life {
        id,
        _profile: profile,
        book: registry.devtools.clone(),
        registry: Arc::downgrade(registry),
        ui: std::thread::current().id(),
        native: AtomicI32::new(0),
        closed: AtomicBool::new(false),
        prepared: true,
        close_commands,
        expected_context,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{Executor, NativeAudit, TaskPayload};
    use rust_cef_core::{
        contracts::{BrowserId, RuntimeGeneration},
        ports::{
            BrowserEventEnvelope, BrowserEventSink, FrameSink, OwnedFrame, PublishResult,
            UiDispatcher,
        },
    };
    use std::{num::NonZeroUsize, sync::Mutex};
    struct Sinks;
    impl BrowserEventSink for Sinks {
        fn publish(&self, _: BrowserEventEnvelope) -> PublishResult {
            PublishResult::Published
        }
    }
    impl FrameSink for Sinks {
        fn publish(&self, _: OwnedFrame) -> PublishResult {
            PublishResult::Published
        }
    }
    impl UiDispatcher for Sinks {
        fn wake(&self) -> Result<(), EngineError> {
            Ok(())
        }
    }
    type Posted = Arc<Mutex<Vec<TaskPayload>>>;
    pub(super) fn fixture(
        accept: bool,
    ) -> (Arc<UiRegistry>, Arc<Executor>, Posted, Client, Arc<Life>) {
        let sinks = Arc::new(Sinks);
        let r = UiRegistry::new(
            RuntimeGeneration::new(1),
            sinks.clone(),
            sinks.clone(),
            sinks,
            NativeAudit::new(),
        );
        let e = Executor::new(RuntimeGeneration::new(1), &r, NativeAudit::new());
        r.attach_executor(&e);
        let posted = Arc::new(Mutex::new(Vec::new()));
        let p = posted.clone();
        e.set_test_post(Arc::new(move |payload| {
            if accept {
                p.lock().unwrap().push(payload.clone());
            }
            accept
        }));
        r.devtools.configure(NonZeroUsize::new(1).unwrap()).unwrap();
        let id = r
            .devtools
            .reserve(BrowserId::new(RuntimeGeneration::new(1), 1), 1, 101)
            .unwrap();
        r.devtools.issued(id).unwrap();
        let profile = ProfileHandle::new(RuntimeGeneration::new(1), 7);
        r.devtools.attach_client(id, profile).unwrap();
        // Explicit unit adapter: synthetic identities/no CEF UI initialization.
        // It exercises the real generated interface ownership and final Drop,
        // not a real CEF Browser lifecycle or production build's thread check.
        let life = Arc::new(Life {
            id,
            _profile: profile,
            book: r.devtools.clone(),
            registry: Arc::downgrade(&r),
            ui: std::thread::current().id(),
            native: AtomicI32::new(0),
            closed: AtomicBool::new(false),
            prepared: false,
            close_commands: policy::CloseCommands::checked(34012, 34015).unwrap(),
            expected_context: Weak::new(),
        });
        let client = AuxiliaryClient::new(life.clone());
        (r, e, posted, client, life)
    }
    pub(super) fn prepared_fixture() -> (Arc<UiRegistry>, Client, AuxiliaryId, ProfileHandle) {
        let sinks = Arc::new(Sinks);
        let r = UiRegistry::new(
            RuntimeGeneration::new(1),
            sinks.clone(),
            sinks.clone(),
            sinks,
            NativeAudit::new(),
        );
        r.devtools.configure(NonZeroUsize::new(1).unwrap()).unwrap();
        let id = r
            .devtools
            .reserve(BrowserId::new(RuntimeGeneration::new(1), 1), 1, 101)
            .unwrap();
        let profile = ProfileHandle::new(RuntimeGeneration::new(1), 7);
        r.devtools.attach_prepared_client(id, profile).unwrap();
        // Generated Rust-owned interfaces; deliberate test adapter, no native CEF.
        let life = Arc::new(Life {
            id,
            _profile: profile,
            book: r.devtools.clone(),
            registry: Arc::downgrade(&r),
            ui: std::thread::current().id(),
            native: AtomicI32::new(0),
            closed: AtomicBool::new(false),
            prepared: true,
            close_commands: policy::CloseCommands::checked(34012, 34015).unwrap(),
            expected_context: Weak::new(),
        });
        (r, AuxiliaryClient::new(life), id, profile)
    }
    #[test]
    fn auxiliary_prepared_client_waits_for_last_unissued_interface_before_rollback() {
        let (r, client, id, _) = prepared_fixture();
        let handler = client.life_span_handler().unwrap();
        let copy = handler.clone();
        assert_eq!(r.devtools.rollback(id), Err(EngineError::InvalidRequest));
        r.devtools.abandon_preissue(id).unwrap();
        assert_eq!(r.devtools.issued(id), Err(EngineError::Closing));
        drop(client);
        drop(handler);
        assert_eq!(r.devtools.pending(), 1);
        drop(copy);
        assert_eq!(r.devtools.pending(), 0);
        assert!(!r.devtools.faulted());
    }
    #[test]
    fn auxiliary_prepared_client_close_intent_is_not_abandonment_or_issued_cleanup() {
        let (r, client, id, _) = prepared_fixture();
        r.devtools.request_close(Some(id.owner), Some(id.serial));
        drop(client);
        assert_eq!(r.devtools.pending(), 1);
        assert!(!r.devtools.faulted());
        r.devtools.rollback(id).unwrap();
        assert_eq!(r.devtools.pending(), 0);
        let (r, client, id, _) = prepared_fixture();
        r.devtools.issued(id).unwrap();
        assert_eq!(
            r.devtools.abandon_preissue(id),
            Err(EngineError::InvalidRequest)
        );
        drop(client);
        assert_eq!(r.devtools.pending(), 2);
        assert!(r.devtools.faulted());
    }
    #[test]
    fn auxiliary_prepared_client_wrong_thread_last_field_never_rolls_back() {
        let (r, client, id, _) = prepared_fixture();
        let handler = client.life_span_handler().unwrap();
        r.devtools.abandon_preissue(id).unwrap();
        drop(client);
        std::thread::spawn(move || drop(handler)).join().unwrap();
        assert!(r.devtools.faulted());
        assert_eq!(r.devtools.pending(), 2);
    }
    #[test]
    fn auxiliary_client_generated_fields_wait_for_last_interface_and_separate_resources() {
        let (r, _e, posted, client, life) = fixture(true);
        let id = life.id;
        assert!(!life.created(201).unwrap());
        life.before_close(201).unwrap();
        let handler = client.life_span_handler().unwrap();
        let handler_copy = handler.clone();
        let client_copy = client.clone();
        drop(life);
        drop(client);
        drop(client_copy);
        drop(handler);
        assert!(posted.lock().unwrap().is_empty());
        assert_eq!(r.devtools.pending(), 1);
        drop(handler_copy);
        let work = posted.lock().unwrap().clone();
        assert_eq!(work.len(), 1);
        assert_eq!(
            work[0],
            TaskPayload::AuxiliaryClientReleased { id, native_id: 201 }
        );
        // Simulated execution of the same data-only completion, not Native proof.
        r.devtools.client_returned(id, 201).unwrap();
        assert_eq!(r.devtools.pending(), 1);
        r.devtools.references_released(id, 201).unwrap();
        assert_eq!(r.devtools.pending(), 0);
    }
    #[test]
    fn auxiliary_client_rejected_tail_and_missing_before_close_keep_pending() {
        let (r, _e, posted, client, life) = fixture(false);
        life.created(201).unwrap();
        life.before_close(201).unwrap();
        drop(life);
        drop(client);
        assert!(posted.lock().unwrap().is_empty());
        assert_eq!(r.devtools.pending(), 2);
        let (r, _e, posted, client, life) = fixture(true);
        life.created(201).unwrap();
        drop(life);
        drop(client);
        assert!(posted.lock().unwrap().is_empty());
        assert_eq!(r.devtools.pending(), 2);
    }
    #[test]
    fn auxiliary_client_wrong_thread_do_close_quarantines_later_tail() {
        let (r, _e, posted, client, life) = fixture(true);
        life.created(201).unwrap();
        let handler = client.life_span_handler().unwrap();
        std::thread::spawn(move || assert_eq!(handler.do_close(None), 0))
            .join()
            .unwrap();
        assert!(r.devtools.faulted());
        assert_eq!(life.before_close(201), Err(EngineError::InvalidRequest));
        drop(life);
        drop(client);
        assert!(posted.lock().unwrap().is_empty());
        assert_eq!(r.devtools.pending(), 2);
    }
    #[test]
    fn auxiliary_client_wrong_thread_final_field_never_posts_or_retires() {
        let (r, _e, posted, client, life) = fixture(true);
        life.created(201).unwrap();
        life.before_close(201).unwrap();
        drop(life);
        std::thread::spawn(move || drop(client)).join().unwrap();
        assert!(posted.lock().unwrap().is_empty());
        assert_eq!(r.devtools.pending(), 2);
    }
}
