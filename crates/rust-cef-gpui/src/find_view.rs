//! Explicit original-View Find result routing. No Find product window, timer,
//! receiver consumer, pump, or hidden retry. Host renders owned updates in its
//! existing UI, outside CEF callbacks and original Entity/window update borrows.
use crate::{CefBrowser, ContextMenuServices, binding::WindowToken};
use gpui::{AnyWindowHandle, App, Entity, Subscription, WeakEntity, Window};
use rust_cef_core::{contracts::BrowserId, find::*, ports::EngineError};
use rust_cef_runtime::ContextMenuRegistration;
use rust_cef_ui::find_inbox::Inbox;
use std::{
    cell::{Cell, RefCell},
    num::NonZeroUsize,
    rc::Rc,
    sync::Arc,
};
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FindUiError {
    Missing,
    Duplicate,
    Capacity,
    WrongWindow,
    Retired,
    WrongRegistration,
    NotDrained,
    Runtime(EngineError),
}
#[derive(Clone, Debug)]
pub struct FindUiUpdate {
    pub browser: BrowserId,
    pub status: FindStatus,
}
#[derive(Clone, Debug, Default)]
pub struct FindUiStatus {
    pub retired: bool,
    pub retire_submitted: bool,
    pub active: bool,
    pub started: u64,
    pub finished: u64,
    pub delivered: u64,
    pub last: Option<FindStatus>,
    pub faulted: bool,
    pub first_error: Option<FindUiError>,
}
struct Route {
    browser: BrowserId,
    original: AnyWindowHandle,
    view: WeakEntity<CefBrowser>,
    token: WindowToken,
    native: Arc<ContextMenuRegistration>,
    inbox: Arc<Inbox>,
    retired: Cell<bool>,
    retire_attempted: Cell<bool>,
    retire_submitted: Cell<bool>,
    deferred: Cell<bool>,
    first_error: RefCell<Option<FindUiError>>,
    _subscriptions: RefCell<Vec<Subscription>>,
}
impl Route {
    fn mark_retired(&self) {
        self.retired.set(true);
        self.inbox.retire();
    }
    fn live(&self, cx: &App) -> bool {
        !self.retired.get()
            && self.token.is_live()
            && self.view.upgrade().is_some()
            && cx.windows().contains(&self.original)
            && self
                .inbox
                .lease
                .snapshot()
                .is_ok_and(|s| !s.retired && !s.faulted)
    }
    fn submit_retire(&self) {
        if self.retire_attempted.replace(true) {
            return;
        }
        let result = self.native.retire();
        match result {
            Ok(()) => self.retire_submitted.set(true),
            Err(e) => {
                self.first_error
                    .borrow_mut()
                    .get_or_insert(FindUiError::Runtime(e));
            }
        }
    }
    fn schedule_retire(self: &Rc<Self>, cx: &mut App) {
        self.mark_retired();
        if self.deferred.replace(true) {
            return;
        }
        let route = self.clone();
        cx.defer(move |_| {
            route.deferred.set(false);
            route.submit_retire();
        });
    }
    fn status(&self) -> FindUiStatus {
        let s = self.inbox.snapshot();
        FindUiStatus {
            retired: self.retired.get() || s.retired,
            retire_submitted: self.retire_submitted.get(),
            active: s.active,
            started: s.started,
            finished: s.finished,
            delivered: s.delivered,
            last: s.last,
            faulted: s.faulted,
            first_error: self.first_error.borrow().clone(),
        }
    }
}
/// One bounded result slot per original Browser. A terminal must be explicitly
/// dispatched before the next start can replace it. Limits are logical query
/// UTF8 bytes and route count, not allocator/native Find memory/RSS bounds.
/// Drop only retires admission. Host must shutdown, drive its existing loops,
/// and observe Runtime/operations drained before releasing this service.
pub struct FindServices {
    capacity: NonZeroUsize,
    max_query_bytes: NonZeroUsize,
    routes: Vec<Rc<Route>>,
}
impl FindServices {
    pub fn new(capacity: NonZeroUsize, max_query_bytes: NonZeroUsize) -> Self {
        Self {
            capacity,
            max_query_bytes,
            routes: Vec::new(),
        }
    }
    pub fn register(
        &mut self,
        view: &Entity<CefBrowser>,
        window: &Window,
        menus: &ContextMenuServices,
        native: Arc<ContextMenuRegistration>,
        cx: &mut App,
    ) -> Result<(), FindUiError> {
        let (token, original, _, _) = view.read(cx).menu_attachment();
        let browser = token.binding().browser;
        if original != window.window_handle() {
            return Err(FindUiError::WrongWindow);
        }
        if !token.is_live() {
            return Err(FindUiError::Retired);
        }
        if self.routes.iter().any(|r| r.browser == browser) {
            return Err(FindUiError::Duplicate);
        }
        if self.routes.len() >= self.capacity.get() {
            return Err(FindUiError::Capacity);
        }
        self.routes
            .try_reserve(1)
            .map_err(|_| FindUiError::Capacity)?;
        let target = menus.target(browser).map_err(|_| FindUiError::Missing)?;
        if !native
            .matches_target(&target)
            .map_err(FindUiError::Runtime)?
        {
            return Err(FindUiError::WrongRegistration);
        }
        let route = Rc::new(Route {
            browser,
            original,
            view: view.downgrade(),
            inbox: Inbox::new(token.binding(), target.lease, self.max_query_bytes),
            token,
            native,
            retired: Cell::new(false),
            retire_attempted: Cell::new(false),
            retire_submitted: Cell::new(false),
            deferred: Cell::new(false),
            first_error: RefCell::new(None),
            _subscriptions: RefCell::new(Vec::new()),
        });
        let weak = Rc::downgrade(&route);
        let release = cx.observe_release(view, move |_, cx| {
            if let Some(r) = weak.upgrade() {
                r.schedule_retire(cx);
            }
        });
        let weak = Rc::downgrade(&route);
        let closed = cx.on_window_closed(move |cx, id| {
            if id == original.window_id() {
                if let Some(r) = weak.upgrade() {
                    r.schedule_retire(cx);
                }
            }
        });
        route._subscriptions.borrow_mut().extend([release, closed]);
        route
            .native
            .set_find_results(route.inbox.clone())
            .map_err(FindUiError::Runtime)?;
        self.routes.push(route);
        Ok(())
    }
    /// Call at top-level App after Entity/window borrows return. Updates carry only
    /// owned metadata; apply them to the matching existing host UI explicitly.
    pub fn dispatch(&mut self, cx: &mut App) -> Vec<FindUiUpdate> {
        let mut updates = Vec::new();
        for route in &self.routes {
            if !route.live(cx) {
                route.schedule_retire(cx);
                continue;
            }
            if let Some(status) = route.inbox.take_update() {
                // Original receipt.status is an opaque reentry point; requalify afterwards.
                if route.live(cx) {
                    updates.push(FindUiUpdate {
                        browser: route.browser,
                        status,
                    });
                } else {
                    route.schedule_retire(cx);
                }
            }
        }
        updates
    }
    pub fn status(&self, browser: BrowserId) -> Result<FindUiStatus, FindUiError> {
        self.routes
            .iter()
            .find(|r| r.browser == browser)
            .map(|r| r.status())
            .ok_or(FindUiError::Missing)
    }
    /// Opaque cancellation: call outside Entity/window update borrows.
    pub fn cancel(&self, browser: BrowserId) -> Result<(), FindUiError> {
        let r = self
            .routes
            .iter()
            .find(|r| r.browser == browser)
            .ok_or(FindUiError::Missing)?;
        r.inbox
            .receipt()
            .ok_or(FindUiError::Missing)?
            .cancel()
            .map_err(FindUiError::Runtime)
    }
    pub fn shutdown(&mut self, cx: &mut App) {
        for r in &self.routes {
            r.schedule_retire(cx);
        }
    }
    /// Only explicitly retries a completed failed deferred admission. Call outside
    /// Entity/window borrows; a still-pending defer is never bypassed.
    pub fn retry_retire(&mut self, browser: BrowserId) -> Result<(), FindUiError> {
        let r = self
            .routes
            .iter()
            .find(|r| r.browser == browser)
            .ok_or(FindUiError::Missing)?;
        if !r.retired.get() {
            return Err(FindUiError::Retired);
        }
        if r.deferred.get() || !r.retire_attempted.get() {
            return Err(FindUiError::NotDrained);
        }
        if r.retire_submitted.get() {
            return Ok(());
        }
        r.retire_attempted.set(false);
        r.submit_retire();
        if r.retire_submitted.get() {
            Ok(())
        } else {
            Err(r
                .first_error
                .borrow()
                .clone()
                .unwrap_or(FindUiError::NotDrained))
        }
    }
    pub fn unregister(&mut self, browser: BrowserId) -> Result<(), FindUiError> {
        let index = self
            .routes
            .iter()
            .position(|r| r.browser == browser)
            .ok_or(FindUiError::Missing)?;
        let status = self.routes[index].status();
        if !status.retired || !status.retire_submitted || status.active || status.faulted {
            return Err(FindUiError::NotDrained);
        }
        let old = self.routes.remove(index);
        drop(old);
        Ok(())
    }
}
impl Drop for FindServices {
    fn drop(&mut self) {
        for r in &self.routes {
            r.mark_retired();
        }
    }
}
