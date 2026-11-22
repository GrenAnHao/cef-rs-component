//! Private CEF continuation ownership. No presenter, native model/params/frame,
//! window-liveness authority, selection execution, or host service registration.
//! The production barrier/close hooks use this table; its first producer is an
//! explicit test-support deferred-menu fixture, not the default client handler.
use crate::executor::NativeAudit;
use cef::{
    ImplRunContextMenuCallback, ImplRunQuickMenuCallback, RunContextMenuCallback,
    RunQuickMenuCallback, ThreadId, currently_on,
};
use rust_cef_core::{
    contracts::BrowserId,
    menu::{ContextMenuRequestId, ContextMenuResponse, MenuLimits},
    menu_context::{MenuContext, MenuContextLimits, MenuPresentation},
    menu_model::MenuModel,
    menu_requests::{MenuReplyReservation, MenuRequestError, MenuRequests},
    ports::{EngineError, WindowBinding},
};
use std::{
    collections::BTreeSet,
    marker::PhantomData,
    num::NonZeroUsize,
    rc::Rc,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

enum Reply {
    Native(RunContextMenuCallback),
    /// Windowless browsers request the quick menu; the continuation is a separate
    /// CEF type with the same Continue/Cancel contract.
    Quick(RunQuickMenuCallback),
    #[cfg(test)]
    Test(Box<dyn FnOnce() + Send>),
}

/// A forgotten answer must not turn into implicit last-reference cancellation
/// on an unknown thread or under a data lock. Deliberate abandonment faults the
/// barrier permanently; it does not authorize shutdown or imply native cleanup.
pub(crate) struct RetainedMenu {
    reply: Option<Reply>,
    faults: Arc<AtomicU64>,
    scope: Option<Arc<crate::menu_bridge::Scope>>,
    #[cfg(feature = "test-support")]
    probe: Option<Arc<crate::menu_probe::MenuProbe>>,
}
impl Drop for RetainedMenu {
    fn drop(&mut self) {
        if let Some(reply) = self.reply.take() {
            self.faults.fetch_max(1, Ordering::AcqRel);
            std::mem::forget(reply);
        }
    }
}
impl RetainedMenu {
    pub(crate) fn capture_bound(
        table: &NativeMenus,
        callback: &RunContextMenuCallback,
        scope: Arc<crate::menu_bridge::Scope>,
    ) -> Self {
        Self {
            reply: Some(Reply::Native(callback.clone())),
            faults: table.faults.clone(),
            scope: Some(scope),
            #[cfg(feature = "test-support")]
            probe: None,
        }
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn capture(
        table: &NativeMenus,
        callback: &RunContextMenuCallback,
        probe: Arc<crate::menu_probe::MenuProbe>,
    ) -> Self {
        Self {
            reply: Some(Reply::Native(callback.clone())),
            faults: table.faults.clone(),
            scope: None,
            probe: Some(probe),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MenuCancel {
    token: MenuReplyReservation,
    binding: WindowBinding,
    revision: u64,
}

#[derive(Debug)]
pub(crate) enum MenuOpenError {
    Unavailable,
    Request(MenuRequestError),
}

struct State {
    book: Option<MenuRequests<RetainedMenu>>,
    in_flight: u64,
    active: BTreeSet<(BrowserId, u64)>,
    pending_ids: std::collections::BTreeMap<BrowserId, u64>,
    work_counts: std::collections::BTreeMap<BrowserId, u64>,
    callbacks: u64,
    /// Latest windowless quick-menu continuation, if one is outstanding.
    quick: Option<RunQuickMenuCallback>,
}
pub(crate) struct NativeMenus {
    state: Arc<Mutex<State>>,
    faults: Arc<AtomicU64>,
}
impl NativeMenus {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                book: None,
                in_flight: 0,
                active: BTreeSet::new(),
                pending_ids: Default::default(),
                work_counts: Default::default(),
                callbacks: 0,
                quick: None,
            })),
            faults: Arc::new(AtomicU64::new(0)),
        }
    }
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| {
            self.faults.fetch_max(1, Ordering::AcqRel);
            e.into_inner()
        })
    }
    /// Configuration is explicit and one-shot; no reset after a drain/failure.
    pub(crate) fn configure(
        &self,
        capacity: NonZeroUsize,
        limits: MenuLimits,
    ) -> Result<(), EngineError> {
        let mut s = self.lock();
        if s.book.is_some() || self.faults.load(Ordering::Acquire) != 0 {
            return Err(EngineError::InvalidRequest);
        }
        s.book = Some(MenuRequests::new(capacity, limits));
        Ok(())
    }
    pub(crate) fn pending(&self) -> u64 {
        let s = self.lock();
        s.book.as_ref().map_or(0, |b| b.len() as u64)
            + s.in_flight
            + s.callbacks
            + self.faults.load(Ordering::Acquire)
    }
    pub(crate) fn fault(&self) {
        self.faults.fetch_max(1, Ordering::AcqRel);
    }
    pub(crate) fn callback_guard(&self) -> MenuCallbackGuard {
        self.lock().callbacks += 1;
        MenuCallbackGuard {
            state: self.state.clone(),
            faults: self.faults.clone(),
            _ui: PhantomData,
        }
    }
    pub(crate) fn pending_for(&self, browser: BrowserId) -> u64 {
        let s = self.lock();
        u64::from(s.pending_ids.contains_key(&browser))
            + s.work_counts.get(&browser).copied().unwrap_or(0)
            + s.callbacks
    }
    pub(crate) fn is_active(&self, browser: BrowserId) -> bool {
        self.lock().active.iter().any(|(b, _)| *b == browser)
    }
    fn work(&self, s: &mut State, id: ContextMenuRequestId, payload: RetainedMenu) -> MenuWork {
        s.in_flight += 1;
        *s.work_counts.entry(id.browser).or_default() += 1;
        MenuWork {
            state: self.state.clone(),
            faults: self.faults.clone(),
            id,
            payload: Some(payload),
            _ui: PhantomData,
        }
    }
    /// The entire result (including a rejected/displaced callback) leaves the
    /// mutex. Never use `?`/discard a payload-bearing result inside this lock.
    /// Retain a windowless quick-menu continuation. The base CEF handler returns
    /// false, which cancels the menu immediately; holding the callback here keeps
    /// it alive until the presenter answers. No model is captured yet: this step
    /// exists to prove the dismissal stops.
    pub(crate) fn retain_quick(&self, callback: &RunQuickMenuCallback) -> bool {
        let mut s = self.lock();
        if self.faults.load(Ordering::Acquire) != 0 {
            return false;
        }
        s.callbacks += 0; // continuation lifetimes are owned by the reply below
        s.quick = Some(callback.clone());
        true
    }
    pub(crate) fn take_quick(&self) -> Option<RunQuickMenuCallback> {
        self.lock().quick.take()
    }
    pub(crate) fn open(
        &self,
        model: MenuModel,
        context: MenuContext,
        limits: MenuContextLimits,
        payload: RetainedMenu,
    ) -> Result<(MenuPresentation, Option<MenuWork>), (MenuOpenError, MenuWork)> {
        let browser = context.binding.browser;
        let mut s = self.lock();
        if s.book.is_none() || self.faults.load(Ordering::Acquire) != 0 {
            return Err((
                MenuOpenError::Unavailable,
                self.work(
                    &mut s,
                    ContextMenuRequestId {
                        browser,
                        sequence: 0,
                    },
                    payload,
                ),
            ));
        }
        let result = s
            .book
            .as_mut()
            .expect("checked configured book")
            .open_model(browser, model, context, limits, payload);
        match result {
            Ok(opened) => {
                s.pending_ids
                    .insert(browser, opened.presentation.request.sequence);
                let displaced = opened
                    .displaced
                    .map(|m| self.work(&mut s, m.id(), m.into_payload()));
                Ok((opened.presentation, displaced))
            }
            Err(rejected) => Err((
                MenuOpenError::Request(rejected.error),
                self.work(
                    &mut s,
                    ContextMenuRequestId {
                        browser,
                        sequence: 0,
                    },
                    rejected.payload,
                ),
            )),
        }
    }
    pub(crate) fn can_capture(&self) -> bool {
        let s = self.lock();
        s.book.is_some() && self.faults.load(Ordering::Acquire) == 0
    }
    pub(crate) fn reserve_cancel(
        &self,
        id: ContextMenuRequestId,
        binding: WindowBinding,
        revision: u64,
    ) -> Result<MenuCancel, EngineError> {
        self.reserve_response(id, binding, revision, ContextMenuResponse::Cancel)
    }
    pub(crate) fn reserve_response(
        &self,
        id: ContextMenuRequestId,
        binding: WindowBinding,
        revision: u64,
        response: ContextMenuResponse,
    ) -> Result<MenuCancel, EngineError> {
        let mut s = self.lock();
        if self.faults.load(Ordering::Acquire) != 0 {
            return Err(EngineError::InvalidRequest);
        }
        let token = s
            .book
            .as_mut()
            .ok_or(EngineError::InvalidRequest)?
            .reserve_context_reply(id, binding, revision, response)?;
        Ok(MenuCancel {
            token,
            binding,
            revision,
        })
    }
    pub(crate) fn rollback(&self, ticket: MenuCancel) -> bool {
        self.lock()
            .book
            .as_mut()
            .is_some_and(|b| b.rollback(ticket.token))
    }
    pub(crate) fn take_cancel(&self, ticket: MenuCancel) -> Option<MenuWork> {
        self.take_reply(ticket).map(|(work, _)| work)
    }
    pub(crate) fn take_reply(&self, ticket: MenuCancel) -> Option<(MenuWork, ContextMenuResponse)> {
        let mut s = self.lock();
        let completed = s.book.as_mut()?.take_context_reserved(
            ticket.token,
            ticket.binding,
            ticket.revision,
        )?;
        s.pending_ids.remove(&completed.id.browser);
        Some((
            self.work(&mut s, completed.id, completed.payload),
            completed.response,
        ))
    }
    pub(crate) fn take_browser(&self, browser: BrowserId) -> Option<MenuWork> {
        let mut s = self.lock();
        let m = s.book.as_mut()?.take_browser(browser)?;
        s.pending_ids.remove(&browser);
        Some(self.work(&mut s, m.id(), m.into_payload()))
    }
    pub(crate) fn is_pending(&self, id: ContextMenuRequestId) -> bool {
        let s = self.lock();
        self.faults.load(Ordering::Acquire) == 0
            && s.pending_ids.get(&id.browser) == Some(&id.sequence)
    }
    pub(crate) fn take_id(&self, id: ContextMenuRequestId) -> Option<MenuWork> {
        let mut s = self.lock();
        let m = s.book.as_mut()?.take(id)?;
        s.pending_ids.remove(&id.browser);
        Some(self.work(&mut s, m.id(), m.into_payload()))
    }
    pub(crate) fn drain(&self) -> Vec<MenuWork> {
        let mut s = self.lock();
        let menus = s.book.as_mut().map(|b| b.drain()).unwrap_or_default();
        s.pending_ids.clear();
        menus
            .into_iter()
            .map(|m| self.work(&mut s, m.id(), m.into_payload()))
            .collect()
    }
    /// Unexpected native close is not permission to call a possibly destroyed
    /// menu manager from OnBeforeClose. Leak/fault rather than guess liveness.
    pub(crate) fn native_closed(&self, browser: BrowserId) {
        if let Some(work) = self.take_browser(browser) {
            drop(work);
        }
    }
    pub(crate) fn abandon(&self) {
        drop(self.drain());
    }
}

/// UI-local, non-Clone work owns the callback until its release finishes. The
/// count is installed under the same lock which removes the pending entry.
pub(crate) struct MenuWork {
    state: Arc<Mutex<State>>,
    faults: Arc<AtomicU64>,
    id: ContextMenuRequestId,
    payload: Option<RetainedMenu>,
    _ui: PhantomData<Rc<()>>,
}
impl MenuWork {
    pub(crate) fn request(&self) -> ContextMenuRequestId {
        self.id
    }
    pub(crate) fn scope(&self) -> Option<Arc<crate::menu_bridge::Scope>> {
        self.payload.as_ref()?.scope.clone()
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn is_managed_native(&self) -> bool {
        self.payload.as_ref().is_some_and(|payload| {
            payload.scope.is_some() && matches!(payload.reply, Some(Reply::Native(_)))
        })
    }

    pub(crate) fn cancel(self, audit: &NativeAudit) {
        self.finish(audit, || {});
    }
    pub(crate) fn finish(mut self, audit: &NativeAudit, action: impl FnOnce()) {
        let payload = self.payload.as_mut().expect("owned menu work");
        let native = matches!(payload.reply, Some(Reply::Native(_) | Reply::Quick(_)));
        if native && currently_on(ThreadId::UI) == 0 {
            return;
        }
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !s.active.insert((self.id.browser, self.id.sequence)) {
                self.faults.fetch_max(1, Ordering::AcqRel);
                return;
            }
        }
        #[cfg(feature = "test-support")]
        if let Some(p) = &payload.probe {
            p.cancelling(self.id.sequence);
        }
        match payload.reply.take().expect("retained answer") {
            Reply::Native(callback) => {
                audit.record();
                callback.cancel();
                // Still in-flight/active during the last native Release too.
                drop(callback);
            }
            Reply::Quick(callback) => {
                audit.record();
                callback.cancel();
                drop(callback);
            }
            #[cfg(test)]
            Reply::Test(answer) => answer(),
        }
        #[cfg(feature = "test-support")]
        if let Some(p) = &payload.probe {
            p.returned(self.id.sequence);
        }
        if let Some(scope) = &payload.scope {
            scope.note_cancel();
        }
        action();
        if let Some(scope) = &payload.scope {
            scope.terminal(self.id);
        }
    }
}
impl Drop for MenuWork {
    fn drop(&mut self) {
        // An unexecuted payload abandons/faults, never native-drops under a lock.
        drop(self.payload.take());
        let mut s = self.state.lock().unwrap_or_else(|e| {
            self.faults.fetch_max(1, Ordering::AcqRel);
            e.into_inner()
        });
        s.active.remove(&(self.id.browser, self.id.sequence));
        if let Some(count) = s.work_counts.get_mut(&self.id.browser) {
            match count.checked_sub(1) {
                Some(0) => {
                    s.work_counts.remove(&self.id.browser);
                }
                Some(n) => *count = n,
                None => {
                    self.faults.fetch_max(1, Ordering::AcqRel);
                }
            }
        } else {
            self.faults.fetch_max(1, Ordering::AcqRel);
        }
        match s.in_flight.checked_sub(1) {
            Some(n) => s.in_flight = n,
            None => {
                self.faults.fetch_max(1, Ordering::AcqRel);
            }
        }
    }
}
pub(crate) struct MenuCallbackGuard {
    state: Arc<Mutex<State>>,
    faults: Arc<AtomicU64>,
    _ui: PhantomData<Rc<()>>,
}
impl Drop for MenuCallbackGuard {
    fn drop(&mut self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match s.callbacks.checked_sub(1) {
            Some(v) => s.callbacks = v,
            None => {
                self.faults.fetch_max(1, Ordering::AcqRel);
            }
        }
    }
}
#[cfg(test)]
mod tests;
