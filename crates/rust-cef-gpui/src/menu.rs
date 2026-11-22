//! Host-owned, bounded menu registration and explicit UI dispatch.
//!
//! No Runtime receiver, pump, timer, Application or global service is installed.
//! The data-only target can cross to Runtime; the owner, View and focus handles
//! cannot. This is lifecycle/delivery wiring, not a default visual presenter or
//! an implementation of the required managed menu actions.
use crate::CefBrowser;
use gpui::{AnyWindowHandle, App, Entity, FocusHandle, Subscription, WeakEntity, Window};
use rust_cef_core::{
    contracts::BrowserId,
    menu_lease::{MenuLease, MenuLeaseError, MenuLeaseOwner, MenuLeaseSnapshot, MenuLeaseStamp},
    ports::{
        BrowserEvent, BrowserEventEnvelope, ContextMenuDeliverySink, ContextMenuResponseSink,
        ContextMenuTarget, EngineError, MenuDeliveryError,
    },
};
pub use rust_cef_core::{
    menu::{ContextMenuRequestId, ContextMenuResponse, MenuLimits},
    menu_context::{MenuContext, MenuContextLimits, MenuPresentation},
    menu_model::{MenuChild, MenuCommand, MenuItem, MenuModel, MenuSubmenu, MenuTreeError},
};
use std::{
    cell::{Cell, RefCell},
    num::NonZeroUsize,
    rc::{Rc, Weak},
    sync::{Arc, Mutex, Weak as SyncWeak},
};
#[cfg(test)]
mod review_tests;
#[cfg(test)]
mod tests;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuServiceError {
    Capacity,
    AlreadyRegistered,
    Missing,
    WrongWindow,
    Retired,
    AllocationFailed,
    Lease(MenuLeaseError),
    Delivery(MenuDeliveryError),
}
impl std::fmt::Display for MenuServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "menu service: {self:?}")
    }
}
impl std::error::Error for MenuServiceError {}

struct Delivery {
    presentation: MenuPresentation,
    response: Arc<dyn ContextMenuResponseSink>,
    stamp: MenuLeaseStamp,
}
struct Latest {
    id: ContextMenuRequestId,
    raw: Arc<dyn ContextMenuResponseSink>,
    attempted: bool,
    submission_in_flight: bool,
    reply_admitted: bool,
    select_admitted: bool,
    select_in_flight: bool,
    focus_return_pending: bool,
    cancel_requested: bool,
    cancel_error: Option<EngineError>,
    selection_error: Option<EngineError>,
}
struct InboxState {
    ready: bool,
    closed: bool,
    latest_sequence: u64,
    terminal_sequence: u64,
    pending: Option<Delivery>,
    latest: Option<Latest>,
    first_cancel_error: Option<EngineError>,
    first_lease_error: Option<MenuLeaseError>,
    first_selection_error: Option<EngineError>,
}
struct Inbox {
    state: Mutex<InboxState>,
    lease: MenuLease,
    limits: MenuLimits,
    context_limits: MenuContextLimits,
}
impl Inbox {
    fn new(lease: MenuLease, limits: MenuLimits, context_limits: MenuContextLimits) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(InboxState {
                ready: false,
                closed: false,
                latest_sequence: 0,
                terminal_sequence: 0,
                pending: None,
                latest: None,
                first_cancel_error: None,
                first_lease_error: None,
                first_selection_error: None,
            }),
            lease,
            limits,
            context_limits,
        })
    }
    fn qualified(&self, s: &InboxState, stamp: &MenuLeaseStamp) -> Result<(), MenuDeliveryError> {
        if s.closed {
            return Err(MenuDeliveryError::Closed);
        }
        if !s.ready {
            return Err(MenuDeliveryError::NotReady);
        }
        if !self
            .lease
            .is_current(stamp)
            .map_err(MenuDeliveryError::Lease)?
        {
            return Err(MenuDeliveryError::Stale);
        }
        Ok(())
    }
    fn record_lease_error(&self, error: MenuLeaseError) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .first_lease_error
            .get_or_insert(error);
    }
    // Only a return to the actual original Browser in its still-active window
    // calls this. In-flight Select is NOT admitted: remember the benign focus
    // transition, then make a refusal explicit cleanup rather than auto-post.
    #[cfg(test)]
    fn returned_to_browser(
        &self,
        id: ContextMenuRequestId,
        stamp: &MenuLeaseStamp,
    ) -> Option<bool> {
        let mut s = self.state.lock().ok()?;
        self.classify_browser_return(&mut s, id, stamp)
    }
    fn classify_browser_return(
        &self,
        s: &mut InboxState,
        id: ContextMenuRequestId,
        stamp: &MenuLeaseStamp,
    ) -> Option<bool> {
        if s.closed || !self.lease.is_current(stamp).ok()? {
            return None;
        }
        if s.terminal_sequence >= id.sequence {
            return None;
        }
        let last = s.latest.as_mut().filter(|x| x.id == id)?;
        if last.select_admitted {
            return Some(true);
        }
        if last.select_in_flight {
            last.focus_return_pending = true;
            return Some(true);
        }
        Some(false)
    }
    // UI owner is borrowed only for this data transaction. Recheck id AND
    // allocation/epoch under the very lock which advances the owner epoch;
    // never classify an old menu and later revoke whichever new slot arrived.
    fn apply_browser_return(
        &self,
        id: ContextMenuRequestId,
        stamp: &MenuLeaseStamp,
        owner: &mut MenuLeaseOwner,
        visible: bool,
    ) -> Option<ContextMenuRequestId> {
        let mut s = match self.state.lock() {
            Ok(s) => s,
            Err(_) => {
                let _ = owner.retire();
                return None;
            }
        };
        let keep = self.classify_browser_return(&mut s, id, stamp)?;
        let cancelled = if keep {
            None
        } else {
            if !owner.lease().is_current(stamp).unwrap_or(false) {
                return None;
            }
            if let Err(e) = owner.invalidate() {
                s.first_lease_error.get_or_insert(e);
            }
            Some(id)
        };
        s.ready = visible
            && owner
                .lease()
                .snapshot()
                .is_ok_and(|x| !x.retired && !x.faulted);
        cancelled
    }
    fn live_request(
        &self,
        id: ContextMenuRequestId,
        stamp: &MenuLeaseStamp,
    ) -> Result<bool, MenuDeliveryError> {
        let s = self.state.lock().map_err(|_| MenuDeliveryError::Poisoned)?;
        self.qualified(&s, stamp)?;
        Ok(s.terminal_sequence < id.sequence
            && s.latest
                .as_ref()
                .is_some_and(|x| x.id == id && !x.reply_admitted && !x.cancel_requested))
    }
    fn begin_select(
        &self,
        id: ContextMenuRequestId,
        stamp: &MenuLeaseStamp,
    ) -> Result<(), EngineError> {
        let mut s = self.state.lock().map_err(|_| EngineError::InvalidRequest)?;
        self.qualified(&s, stamp)
            .map_err(|_| EngineError::NotReady)?;
        if s.terminal_sequence >= id.sequence {
            return Err(EngineError::InvalidRequest);
        }
        let last = s
            .latest
            .as_mut()
            .filter(|x| x.id == id)
            .ok_or(EngineError::InvalidRequest)?;
        if last.reply_admitted {
            return Err(EngineError::InvalidRequest);
        }
        if last.submission_in_flight || last.cancel_requested {
            return Err(EngineError::Backpressure);
        }
        last.submission_in_flight = true;
        last.select_in_flight = true;
        Ok(())
    }
    fn selected(&self, id: ContextMenuRequestId, result: &Result<(), EngineError>) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(last) = s.latest.as_mut().filter(|x| x.id == id) {
            last.submission_in_flight = false;
            last.select_in_flight = false;
            if result.is_ok() {
                last.reply_admitted = true;
                last.select_admitted = true;
                last.cancel_error = None;
            } else if let Err(e) = result {
                last.selection_error = Some(e.clone());
                if last.focus_return_pending {
                    last.cancel_requested = true;
                }
                // A refused Select is not a Cancel submission/failure. Cleanup
                // can remain requested without inventing cancellation history.
                s.first_selection_error.get_or_insert(e.clone());
            }
            // If a cancellation arrived while submit was outside the mutex,
            // a refused selection leaves explicit cleanup pending; no hidden post.
        }
    }
    fn set_ready(&self, ready: bool) -> Result<bool, MenuDeliveryError> {
        let mut s = self.state.lock().map_err(|_| MenuDeliveryError::Poisoned)?;
        if s.closed {
            return Ok(false);
        }
        let lost = s.ready && !ready;
        s.ready = ready;
        Ok(lost)
    }
    fn take_update(&self) -> Result<(Option<Delivery>, u64), MenuDeliveryError> {
        let mut s = self.state.lock().map_err(|_| MenuDeliveryError::Poisoned)?;
        Ok((s.pending.take(), s.terminal_sequence))
    }
    fn cancel(&self, explicit_retry: bool) -> Result<(), EngineError> {
        let id = {
            self.state
                .lock()
                .map_err(|_| EngineError::InvalidRequest)?
                .latest
                .as_ref()
                .map(|x| x.id)
        };
        id.map_or(Ok(()), |id| self.cancel_id(id, explicit_retry))
    }
    fn cancel_id(&self, id: ContextMenuRequestId, explicit_retry: bool) -> Result<(), EngineError> {
        let request = {
            let mut s = self.state.lock().map_err(|_| EngineError::InvalidRequest)?;
            let Some(last) = s.latest.as_mut().filter(|x| x.id == id) else {
                return Ok(());
            };
            if last.reply_admitted {
                return Ok(());
            }
            last.cancel_requested = true;
            if last.submission_in_flight {
                last.cancel_error = Some(EngineError::Backpressure);
                s.first_cancel_error
                    .get_or_insert(EngineError::Backpressure);
                return Err(EngineError::Backpressure);
            }
            if last.attempted && !explicit_retry {
                return last.cancel_error.clone().map_or(Ok(()), Err);
            }
            // Explicit retry is meaningful only after a refused submission.
            if last.attempted && last.cancel_error.is_none() {
                return Ok(());
            }
            last.attempted = true;
            last.submission_in_flight = true;
            Some((last.id, last.raw.clone()))
        };
        let Some((id, response)) = request else {
            return Ok(());
        };
        let result = response.submit(id, ContextMenuResponse::Cancel);
        let mut s = self.state.lock().map_err(|_| EngineError::InvalidRequest)?;
        if let Err(e) = &result {
            s.first_cancel_error.get_or_insert(e.clone());
        }
        if let Some(last) = s.latest.as_mut().filter(|x| x.id == id) {
            last.submission_in_flight = false;
            if result.is_ok() {
                last.reply_admitted = true;
            }
            last.cancel_error = result.as_ref().err().cloned();
        }
        result
    }
    fn close(&self) {
        // Move all owned endpoints/data out, then drop/call them with no lock.
        let pending = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.closed = true;
            s.ready = false;
            s.pending.take()
        };
        drop(pending);
        let _ = self.cancel(false); // Failure remains in status; Runtime still owns native drainage.
    }
}
impl ContextMenuDeliverySink for Inbox {
    fn capture(&self) -> Result<MenuLeaseStamp, MenuDeliveryError> {
        let s = self.state.lock().map_err(|_| MenuDeliveryError::Poisoned)?;
        let stamp = self.lease.capture().map_err(MenuDeliveryError::Lease)?;
        self.qualified(&s, &stamp)?;
        Ok(stamp)
    }
    fn publish(
        &self,
        presentation: MenuPresentation,
        response: Arc<dyn ContextMenuResponseSink>,
        stamp: MenuLeaseStamp,
    ) -> Result<(), MenuDeliveryError> {
        let id = presentation.request;
        if id.sequence == 0
            || id.browser != stamp.binding().browser
            || presentation.context.binding != stamp.binding()
            || presentation.context.binding_revision != stamp.revision()
        {
            return Err(MenuDeliveryError::WrongScope);
        }
        presentation
            .context
            .validate(id.browser, self.context_limits)
            .map_err(MenuDeliveryError::Context)?;
        presentation
            .model
            .validate(self.limits)
            .map_err(MenuDeliveryError::Model)?;
        if (presentation.model.x, presentation.model.y)
            != (presentation.context.x, presentation.context.y)
        {
            return Err(MenuDeliveryError::WrongScope);
        }
        let (old_pending, old_latest) = {
            let mut s = self.state.lock().map_err(|_| MenuDeliveryError::Poisoned)?;
            self.qualified(&s, &stamp)?;
            if id.sequence <= s.latest_sequence || id.sequence <= s.terminal_sequence {
                return Err(MenuDeliveryError::Stale);
            }
            if s.latest.as_ref().is_some_and(|x| {
                x.attempted || x.reply_admitted || x.submission_in_flight || x.cancel_requested
            }) {
                return Err(MenuDeliveryError::Backpressure);
            }
            s.latest_sequence = id.sequence;
            let old = s.latest.replace(Latest {
                id,
                raw: response.clone(),
                attempted: false,
                submission_in_flight: false,
                reply_admitted: false,
                select_admitted: false,
                select_in_flight: false,
                focus_return_pending: false,
                cancel_requested: false,
                cancel_error: None,
                selection_error: None,
            });
            (
                s.pending.replace(Delivery {
                    presentation,
                    response,
                    stamp,
                }),
                old,
            )
        };
        // Supersession is a backend responsibility. Dropping an owned UI
        // delivery is NOT a native answer; no arbitrary endpoint Drop under lock.
        drop(old_pending);
        drop(old_latest);
        Ok(())
    }
    fn dismiss(&self, id: ContextMenuRequestId) -> Result<(), MenuDeliveryError> {
        if id.browser
            != self
                .lease
                .snapshot()
                .map_err(MenuDeliveryError::Lease)?
                .binding
                .browser
            || id.sequence == 0
        {
            return Err(MenuDeliveryError::WrongScope);
        }
        let (pending, latest) = {
            let mut s = self.state.lock().map_err(|_| MenuDeliveryError::Poisoned)?;
            s.terminal_sequence = s.terminal_sequence.max(id.sequence);
            let pending = if s
                .pending
                .as_ref()
                .is_some_and(|p| p.presentation.request.sequence <= s.terminal_sequence)
            {
                s.pending.take()
            } else {
                None
            };
            let latest = if s
                .latest
                .as_ref()
                .is_some_and(|p| p.id.sequence <= s.terminal_sequence)
            {
                s.latest.take()
            } else {
                None
            };
            (pending, latest)
        };
        drop(pending);
        drop(latest);
        Ok(())
    }
}
struct ScopedResponse {
    id: ContextMenuRequestId,
    stamp: MenuLeaseStamp,
    inbox: SyncWeak<Inbox>,
    allowed: Vec<i32>,
    inner: Arc<dyn ContextMenuResponseSink>,
}
impl ContextMenuResponseSink for ScopedResponse {
    fn submit(
        &self,
        id: ContextMenuRequestId,
        response: ContextMenuResponse,
    ) -> Result<(), EngineError> {
        if id != self.id {
            return Err(EngineError::InvalidRequest);
        }
        if let ContextMenuResponse::Select { command_id } = response {
            if !self.allowed.contains(&command_id) {
                return Err(EngineError::InvalidRequest);
            }
            let inbox = self.inbox.upgrade().ok_or(EngineError::Closed)?;
            inbox.begin_select(id, &self.stamp)?;
            // Local linearized admission only. Backend still reserves/posts and
            // obtains the original CEF execution permit; Ok is not completion.
            let result = self.inner.submit(id, response);
            inbox.selected(id, &result);
            result
        } else if let Some(inbox) = self.inbox.upgrade() {
            inbox.cancel_id(id, true)
        } else {
            self.inner.submit(id, response)
        }
    }
}
fn allowed(model: &rust_cef_core::menu_model::MenuModel) -> Vec<i32> {
    let mut ids = Vec::new();
    for entry in &model.entries {
        match entry {
            MenuItem::Command(c) if c.visible && c.enabled => ids.push(c.command_id),
            MenuItem::Submenu(g) if g.visible && g.enabled => {
                for child in &g.entries {
                    if let MenuChild::Command(c) = child {
                        if c.visible && c.enabled {
                            ids.push(c.command_id);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    ids
}

pub(crate) type MenuProjection = Rc<RefCell<Weak<ViewMenuBinding>>>;
pub(crate) fn project(slot: &MenuProjection) -> Option<Rc<ViewMenuBinding>> {
    slot.borrow().upgrade()
}
pub(crate) fn retire_projection(slot: &MenuProjection) {
    if let Some(b) = project(slot) {
        b.retire();
    }
}

pub(crate) struct ViewMenuBinding {
    owner: RefCell<MenuLeaseOwner>,
    inbox: Arc<Inbox>,
    window: AnyWindowHandle,
    browser_focus: FocusHandle,
    current: Cell<Option<ContextMenuRequestId>>,
    focus: RefCell<Option<FocusHandle>>,
    focus_companions: RefCell<Vec<FocusHandle>>,
    focus_subscription: RefCell<Option<Subscription>>,
    hidden: Cell<bool>,
}
impl ViewMenuBinding {
    fn invalidate(&self) {
        // Use the same inbox->lease lock order as publication. Capture the
        // old request at this cutpoint, not whichever newer opening happens to
        // arrive before the external cancellation post.
        let (result, id) = {
            let s = self.inbox.state.lock().unwrap_or_else(|e| e.into_inner());
            let mut owner = self.owner.borrow_mut();
            if owner.lease().snapshot().is_ok_and(|s| s.retired) {
                return;
            }
            (owner.invalidate(), s.latest.as_ref().map(|x| x.id))
        };
        if let Err(e) = result {
            self.inbox.record_lease_error(e);
        }
        if let Some(id) = id {
            let _ = self.inbox.cancel_id(id, false);
        }
    }
    pub(crate) fn retire(&self) {
        let result = { self.owner.borrow_mut().retire() };
        if let Err(e) = result {
            self.inbox.record_lease_error(e);
        }
        self.clear_focus();
        self.inbox.close();
    }
    pub(crate) fn geometry_changed(&self) {
        self.invalidate();
    }
    pub(crate) fn visible(&self, visible: bool) {
        if self.hidden.replace(!visible) != !visible && !visible {
            let _ = self.inbox.set_ready(false);
            self.invalidate();
        }
    }
    pub(crate) fn readiness(&self, ready: bool) {
        match self.inbox.set_ready(ready && !self.hidden.get()) {
            Ok(true) => self.invalidate(),
            Err(_) => self.retire(),
            _ => {}
        }
    }
    pub(crate) fn owns_focus(&self, window: &Window, cx: &App) -> bool {
        let focus = self.focus.borrow().clone();
        window.window_handle() == self.window
            && (focus.is_some_and(|f| f.contains_focused(window, cx))
                || self
                    .focus_companions
                    .borrow()
                    .iter()
                    .any(|f| f.contains_focused(window, cx)))
    }
    fn clear_focus(&self) {
        self.current.set(None);
        let subscription = self.focus_subscription.borrow_mut().take();
        let focus = self.focus.borrow_mut().take();
        let companions = std::mem::take(&mut *self.focus_companions.borrow_mut());
        drop(subscription);
        drop(focus);
        drop(companions);
    }
}
impl Drop for ViewMenuBinding {
    fn drop(&mut self) {
        self.retire();
    }
}

/// UI-only capability for the current menu's focus tree. Declaring a focus
/// handle observes ownership; it never activates a window or changes focus.
#[derive(Clone)]
pub struct MenuFocusRegistration {
    binding: Weak<ViewMenuBinding>,
    request: ContextMenuRequestId,
    stamp: MenuLeaseStamp,
}
impl MenuFocusRegistration {
    pub(crate) fn captured_matches(
        request: ContextMenuRequestId,
        stamp: &MenuLeaseStamp,
        presentation: &MenuPresentation,
    ) -> bool {
        request == presentation.request
            && presentation
                .context
                .matches_binding(stamp.binding(), stamp.revision())
    }
    pub(crate) fn matches_presentation(&self, presentation: &MenuPresentation) -> bool {
        Self::captured_matches(self.request, &self.stamp, presentation)
    }
    pub(crate) fn is_current(&self) -> bool {
        self.binding.upgrade().is_some_and(|b| {
            b.current.get() == Some(self.request)
                && b.inbox
                    .live_request(self.request, &self.stamp)
                    .unwrap_or(false)
        })
    }
    pub fn declare(
        &self,
        focus: FocusHandle,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), MenuServiceError> {
        self.declare_family(focus, Vec::new(), window, cx)
    }
    // Only the default adapter supplies its actual bounded, newly constructed
    // child handles. A child may receive focus before the next dispatch tree
    // has been painted; numerical tree ancestry is not yet available then.
    pub(crate) fn declare_family(
        &self,
        focus: FocusHandle,
        companions: Vec<FocusHandle>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), MenuServiceError> {
        let b = self.binding.upgrade().ok_or(MenuServiceError::Retired)?;
        if companions.len() > b.inbox.limits.max_entries.get() {
            return Err(MenuServiceError::Capacity);
        }
        if b.window != window.window_handle() {
            return Err(MenuServiceError::WrongWindow);
        }
        if b.current.get() != Some(self.request)
            || !b
                .inbox
                .live_request(self.request, &self.stamp)
                .map_err(MenuServiceError::Delivery)?
        {
            return Err(MenuServiceError::Retired);
        }
        let weak = self.binding.clone();
        let request = self.request;
        let stamp = self.stamp.clone();
        let subscription = window.on_focus_out(&focus, cx, move |_, window, cx| {
            if let Some(b) = weak.upgrade() {
                if b.current.get() == Some(request) && !b.owns_focus(window, cx) {
                    if window.window_handle() == b.window
                        && window.is_window_active()
                        && b.browser_focus.is_focused(window)
                    {
                        let cancel = {
                            b.inbox.apply_browser_return(
                                request,
                                &stamp,
                                &mut b.owner.borrow_mut(),
                                !b.hidden.get(),
                            )
                        };
                        if let Some(id) = cancel {
                            let _ = b.inbox.cancel_id(id, false);
                        }
                    } else {
                        b.readiness(false);
                    }
                }
            }
        });
        let old = b.focus_subscription.borrow_mut().replace(subscription);
        let old_focus = b.focus.borrow_mut().replace(focus);
        let old_companions = b.focus_companions.replace(companions);
        drop(old);
        drop(old_focus);
        drop(old_companions);
        Ok(())
    }
}

pub enum MenuUiUpdate {
    Open {
        presentation: MenuPresentation,
        response: Arc<dyn ContextMenuResponseSink>,
        focus: MenuFocusRegistration,
    },
    Dismiss {
        request: ContextMenuRequestId,
    },
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MenuDispatchReport {
    pub opened: bool,
    pub dismissed: bool,
    pub stale: bool,
    pub presenter_error: Option<EngineError>,
    pub cancellation_error: Option<EngineError>,
}
#[derive(Clone, Debug)]
pub struct MenuServiceStatus {
    pub selection_error: Option<EngineError>,
    pub first_selection_error: Option<EngineError>,
    pub cleanup_requested: bool,
    pub reply_admitted: bool,
    pub submission_in_flight: bool,
    pub lease: MenuLeaseSnapshot,
    pub ready: bool,
    pub pending: bool,
    pub cancellation_error: Option<EngineError>,
    pub first_cancellation_error: Option<EngineError>,
    pub first_lease_error: Option<MenuLeaseError>,
}
/// Data-only cleanup receipt, with no UI owner/entity or direct CEF reference.
/// It retains an opaque response endpoint under the owned-endpoint contract;
/// its type cannot certify what arbitrary foreign sink implementations retain. `awaiting_terminal` is UI delivery state, never Runtime/native drain.
#[derive(Clone)]
pub struct MenuCleanup {
    inbox: Arc<Inbox>,
}
#[derive(Clone, Debug)]
pub struct MenuCleanupStatus {
    pub selection_error: Option<EngineError>,
    pub first_selection_error: Option<EngineError>,
    pub cleanup_requested: bool,
    pub reply_admitted: bool,
    pub submission_in_flight: bool,
    pub lease: MenuLeaseSnapshot,
    pub awaiting_terminal: bool,
    pub first_lease_error: Option<MenuLeaseError>,
    pub cancellation_error: Option<EngineError>,
    pub first_cancellation_error: Option<EngineError>,
}
impl MenuCleanup {
    pub fn retry(&self) -> Result<(), EngineError> {
        self.inbox.cancel(true)
    }
    pub fn status(&self) -> Result<MenuCleanupStatus, MenuServiceError> {
        let lease = self
            .inbox
            .lease
            .snapshot()
            .map_err(MenuServiceError::Lease)?;
        let s = self
            .inbox
            .state
            .lock()
            .map_err(|_| MenuServiceError::Delivery(MenuDeliveryError::Poisoned))?;
        Ok(MenuCleanupStatus {
            selection_error: s.latest.as_ref().and_then(|x| x.selection_error.clone()),
            first_selection_error: s.first_selection_error.clone(),
            cleanup_requested: s
                .latest
                .as_ref()
                .is_some_and(|x| x.cancel_requested && !x.reply_admitted),
            reply_admitted: s.latest.as_ref().is_some_and(|x| x.reply_admitted),
            submission_in_flight: s.latest.as_ref().is_some_and(|x| x.submission_in_flight),
            lease,
            awaiting_terminal: s.latest.is_some(),
            first_lease_error: s.first_lease_error,
            cancellation_error: s.latest.as_ref().and_then(|x| x.cancel_error.clone()),
            first_cancellation_error: s.first_cancel_error.clone(),
        })
    }
}
struct Current {
    id: ContextMenuRequestId,
    stamp: MenuLeaseStamp,
}
struct Route {
    browser: BrowserId,
    view: WeakEntity<CefBrowser>,
    binding: Rc<ViewMenuBinding>,
    current: Option<Current>,
    event_sequence: u64,
}
fn retired_dispatch(
    route: &mut Route,
    window: &mut Window,
    cx: &mut App,
    presenter: &mut impl FnMut(MenuUiUpdate, &mut Window, &mut App) -> Result<(), EngineError>,
) -> MenuDispatchReport {
    route.binding.retire();
    let mut report = MenuDispatchReport {
        stale: true,
        ..Default::default()
    };
    if let Some(current) = route.current.take() {
        route.binding.clear_focus();
        report.dismissed = true;
        report.presenter_error = presenter(
            MenuUiUpdate::Dismiss {
                request: current.id,
            },
            window,
            cx,
        )
        .err();
    }
    report.cancellation_error = route
        .binding
        .inbox
        .state
        .lock()
        .ok()
        .and_then(|s| s.latest.as_ref().and_then(|x| x.cancel_error.clone()));
    report
}
/// Explicit UI-thread-only owner. Dispatch touches just the requested original
/// window; caller owns its UI turn and the visual presenter. No receiver/task is
/// started. Native producers/actions remain explicit; the optional visual
/// adapter is `DefaultMenuPresenter`, installed and rendered separately.
pub struct ContextMenuServices {
    capacity: NonZeroUsize,
    permit_capacity: NonZeroUsize,
    limits: MenuLimits,
    context_limits: MenuContextLimits,
    routes: Vec<Route>,
}
impl ContextMenuServices {
    pub fn new(
        capacity: NonZeroUsize,
        permit_capacity: NonZeroUsize,
        limits: MenuLimits,
        context_limits: MenuContextLimits,
    ) -> Self {
        Self {
            capacity,
            permit_capacity,
            limits,
            context_limits,
            routes: Vec::new(),
        }
    }
    pub fn register(
        &mut self,
        view: &Entity<CefBrowser>,
        window: &Window,
        cx: &mut App,
    ) -> Result<ContextMenuTarget, MenuServiceError> {
        let (token, handle, slot, browser_focus) = view.read(cx).menu_attachment();
        let browser = token.binding().browser;
        if handle != window.window_handle() {
            return Err(MenuServiceError::WrongWindow);
        }
        if !token.is_live() {
            return Err(MenuServiceError::Retired);
        }
        if project(&slot).is_some() || self.routes.iter().any(|r| r.browser == browser) {
            return Err(MenuServiceError::AlreadyRegistered);
        }
        if self.routes.len() >= self.capacity.get() {
            return Err(MenuServiceError::Capacity);
        }
        self.routes
            .try_reserve(1)
            .map_err(|_| MenuServiceError::AllocationFailed)?;
        let owner = MenuLeaseOwner::new(token.binding(), self.permit_capacity)
            .map_err(MenuServiceError::Lease)?;
        let inbox = Inbox::new(owner.lease(), self.limits, self.context_limits);
        let binding = Rc::new(ViewMenuBinding {
            owner: RefCell::new(owner),
            inbox: inbox.clone(),
            window: handle,
            browser_focus,
            current: Cell::new(None),
            focus: RefCell::new(None),
            focus_companions: RefCell::new(Vec::new()),
            focus_subscription: RefCell::new(None),
            hidden: Cell::new(false),
        });
        *slot.borrow_mut() = Rc::downgrade(&binding);
        self.routes.push(Route {
            browser,
            view: view.downgrade(),
            binding,
            current: None,
            event_sequence: 0,
        });
        view.update(cx, |view, cx| view.project_menu_focus(window, cx));
        Ok(ContextMenuTarget {
            lease: inbox.lease.clone(),
            delivery: inbox,
        })
    }
    pub fn target(&self, browser: BrowserId) -> Result<ContextMenuTarget, MenuServiceError> {
        let b = &self
            .routes
            .iter()
            .find(|r| r.browser == browser)
            .ok_or(MenuServiceError::Missing)?
            .binding;
        Ok(ContextMenuTarget {
            lease: b.inbox.lease.clone(),
            delivery: b.inbox.clone(),
        })
    }
    pub fn status(&self, browser: BrowserId) -> Result<MenuServiceStatus, MenuServiceError> {
        let b = &self
            .routes
            .iter()
            .find(|r| r.browser == browser)
            .ok_or(MenuServiceError::Missing)?
            .binding;
        let lease = b.inbox.lease.snapshot().map_err(MenuServiceError::Lease)?;
        let s = b
            .inbox
            .state
            .lock()
            .map_err(|_| MenuServiceError::Delivery(MenuDeliveryError::Poisoned))?;
        Ok(MenuServiceStatus {
            selection_error: s.latest.as_ref().and_then(|x| x.selection_error.clone()),
            first_selection_error: s.first_selection_error.clone(),
            cleanup_requested: s
                .latest
                .as_ref()
                .is_some_and(|x| x.cancel_requested && !x.reply_admitted),
            reply_admitted: s.latest.as_ref().is_some_and(|x| x.reply_admitted),
            submission_in_flight: s.latest.as_ref().is_some_and(|x| x.submission_in_flight),
            lease,
            ready: s.ready,
            pending: s.pending.is_some(),
            cancellation_error: s.latest.as_ref().and_then(|x| x.cancel_error.clone()),
            first_cancellation_error: s.first_cancel_error.clone(),
            first_lease_error: s.first_lease_error,
        })
    }
    /// Explicitly forwards already-owned Runtime events; does not consume them.
    /// Event sequence/URL are NOT native frame/navigation identity or an atomic
    /// liveness notification. CEF execution still needs original-frame checks.
    pub fn observe(&mut self, event: &BrowserEventEnvelope) -> Result<(), MenuServiceError> {
        let r = self
            .routes
            .iter_mut()
            .find(|r| r.browser == event.browser)
            .ok_or(MenuServiceError::Missing)?;
        if event.sequence == 0 {
            return Err(MenuServiceError::Delivery(MenuDeliveryError::WrongScope));
        }
        // An out-of-order non-navigation event must not hide a later-delivered
        // navigation/termination signal. Late signals conservatively revoke;
        // this watermark is NOT a native frame/navigation epoch.
        r.event_sequence = r.event_sequence.max(event.sequence);
        match event.event {
            BrowserEvent::Closed | BrowserEvent::CreateFailed(_) => r.binding.retire(),
            BrowserEvent::NavigationRequested { .. }
            | BrowserEvent::LoadStarted { .. }
            | BrowserEvent::AddressChanged(_)
            | BrowserEvent::RendererTerminated { .. } => r.binding.invalidate(),
            _ => {}
        }
        Ok(())
    }
    pub fn cleanup_handle(&self, browser: BrowserId) -> Result<MenuCleanup, MenuServiceError> {
        let inbox = self
            .routes
            .iter()
            .find(|r| r.browser == browser)
            .ok_or(MenuServiceError::Missing)?
            .binding
            .inbox
            .clone();
        Ok(MenuCleanup { inbox })
    }
    pub fn unregister(&mut self, browser: BrowserId) -> Result<MenuCleanup, MenuServiceError> {
        let i = self
            .routes
            .iter()
            .position(|r| r.browser == browser)
            .ok_or(MenuServiceError::Missing)?;
        let route = self.routes.remove(i);
        let cleanup = MenuCleanup {
            inbox: route.binding.inbox.clone(),
        };
        route.binding.retire();
        drop(route);
        Ok(cleanup)
    }
    /// At most one explicit retry for this binding in this host call. First
    /// failure stays diagnostic even after a subsequent successful admission.
    pub fn retry_cancel(&self, browser: BrowserId) -> Result<(), EngineError> {
        self.routes
            .iter()
            .find(|r| r.browser == browser)
            .ok_or(EngineError::Closed)?
            .binding
            .inbox
            .cancel(true)
    }
    pub fn dispatch(
        &mut self,
        browser: BrowserId,
        window: &mut Window,
        cx: &mut App,
        mut presenter: impl FnMut(MenuUiUpdate, &mut Window, &mut App) -> Result<(), EngineError>,
    ) -> Result<MenuDispatchReport, MenuServiceError> {
        let route = self
            .routes
            .iter_mut()
            .find(|r| r.browser == browser)
            .ok_or(MenuServiceError::Missing)?;
        if route.binding.window != window.window_handle() {
            return Err(MenuServiceError::WrongWindow);
        }
        let Some(view) = route.view.upgrade() else {
            return Ok(retired_dispatch(route, window, cx, &mut presenter));
        };
        let (token, _, slot, _) = view.read(cx).menu_attachment();
        if !token.is_live() || !project(&slot).is_some_and(|b| Rc::ptr_eq(&b, &route.binding)) {
            return Ok(retired_dispatch(route, window, cx, &mut presenter));
        }
        view.update(cx, |v, cx| v.project_menu_focus(window, cx));
        let (delivery, terminal) = route
            .binding
            .inbox
            .take_update()
            .map_err(MenuServiceError::Delivery)?;
        let mut report = MenuDispatchReport::default();
        let dismiss = route.current.as_ref().is_some_and(|c| {
            c.id.sequence <= terminal
                || !route
                    .binding
                    .inbox
                    .lease
                    .is_current(&c.stamp)
                    .unwrap_or(false)
                || delivery
                    .as_ref()
                    .is_some_and(|d| d.presentation.request != c.id)
        });
        if dismiss {
            let old = route.current.take().unwrap();
            route.binding.clear_focus();
            report.dismissed = true;
            if let Err(e) = presenter(MenuUiUpdate::Dismiss { request: old.id }, window, cx) {
                report.presenter_error = Some(e);
            }
        }
        if let Some(d) = delivery {
            if !route
                .binding
                .inbox
                .live_request(d.presentation.request, &d.stamp)
                .unwrap_or(false)
            {
                report.stale = true;
                report.cancellation_error = route
                    .binding
                    .inbox
                    .cancel_id(d.presentation.request, false)
                    .err();
            } else {
                let request = d.presentation.request;
                let stamp = d.stamp.clone();
                let response = Arc::new(ScopedResponse {
                    id: request,
                    stamp: d.stamp.clone(),
                    inbox: Arc::downgrade(&route.binding.inbox),
                    allowed: allowed(&d.presentation.model),
                    inner: d.response,
                });
                let focus = MenuFocusRegistration {
                    binding: Rc::downgrade(&route.binding),
                    request,
                    stamp: d.stamp.clone(),
                };
                route.current = Some(Current {
                    id: request,
                    stamp: d.stamp,
                });
                route.binding.current.set(Some(request));
                if let Err(e) = presenter(
                    MenuUiUpdate::Open {
                        presentation: d.presentation,
                        response,
                        focus,
                    },
                    window,
                    cx,
                ) {
                    report.presenter_error.get_or_insert(e);
                    route.binding.clear_focus();
                    route.current = None;
                    report.cancellation_error = route.binding.inbox.cancel_id(request, false).err();
                } else if !route
                    .binding
                    .inbox
                    .live_request(request, &stamp)
                    .unwrap_or(false)
                {
                    // External presenter may reenter a data-only terminal
                    // publisher. Reconcile in this turn, never hold a lock
                    // across UI code to pretend atomic native presentation.
                    route.binding.clear_focus();
                    route.current = None;
                    report.stale = true;
                    if let Err(e) = presenter(MenuUiUpdate::Dismiss { request }, window, cx) {
                        report.presenter_error.get_or_insert(e);
                    }
                    report.cancellation_error = route.binding.inbox.cancel_id(request, false).err();
                } else {
                    report.opened = true;
                }
            }
        }
        Ok(report)
    }
}
impl Drop for ContextMenuServices {
    fn drop(&mut self) {
        for r in self.routes.drain(..) {
            r.binding.retire();
        }
    }
}
