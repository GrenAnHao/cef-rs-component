//! Explicit host-owned GPUI source windows. The Send sink retains owned data
//! only. No Application, receiver, timer, pump or foreground workaround.
mod window;
use crate::{CefBrowser, ContextMenuServices, binding::WindowToken};
use gpui::{
    AnyWindowHandle, App, Bounds, Context, Entity, IntoElement, Pixels, Render, SharedString,
    Subscription, TitlebarOptions, WeakEntity, Window, WindowBounds, WindowOptions, div,
    prelude::*, rgb,
};
use rust_cef_core::{contracts::BrowserId, ports::EngineError, source_view::*};
use rust_cef_runtime::ContextMenuRegistration;
use rust_cef_ui::source_inbox::Inbox;
use std::{
    cell::{Cell, RefCell},
    num::NonZeroUsize,
    rc::Rc,
    sync::Arc,
};

/// Limits are per original binding and count logical UTF-8 bytes/operations,
/// not all UI layout, native allocation or process RSS. Bounds/focus are an
/// explicit host choice, never borrowed from another scenario's consent.
#[derive(Clone)]
pub struct SourceViewOptions {
    pub max_operations: NonZeroUsize,
    pub max_source_bytes: NonZeroUsize,
    pub bounds: Bounds<Pixels>,
    pub focus: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceUiError {
    Missing,
    Duplicate,
    Capacity,
    WrongWindow,
    Retired,
    WrongRegistration,
    NotDrained,
    WindowAccess,
    NativeWindow,
    CreateFailed,
    InvalidDocument,
    Runtime(EngineError),
}
#[derive(Clone, Debug, Default)]
pub struct SourceUiStatus {
    pub retired: bool,
    pub operations: usize,
    /// Tracked until window, Entity and payload retirement are ALL confirmed.
    pub live_windows: usize,
    pub created: u64,
    pub window_closed: u64,
    pub entities_released: u64,
    pub payloads_retired: u64,
    pub acknowledged: u64,
    pub last_operation: Option<SourceOperationStatus>,
    pub first_error: Option<SourceUiError>,
}
#[derive(Default)]
struct Life {
    window_closed: Cell<bool>,
    entity_released: Cell<bool>,
    payload_retired: Cell<bool>,
    deferred: Cell<bool>,
}
impl Life {
    fn retired(&self) -> bool {
        self.window_closed.get() && self.entity_released.get() && self.payload_retired.get()
    }
}
struct Record {
    id: SourceOperationId,
    handle: AnyWindowHandle,
    entity: WeakEntity<SourceViewer>,
    life: Rc<Life>,
    native: Option<window::WindowProof>,
    close_requested: bool,
    // Removed outside the RefCell, not accumulated over operation history.
    _subscriptions: Vec<Subscription>,
}
struct Route {
    browser: BrowserId,
    original: AnyWindowHandle,
    view: WeakEntity<CefBrowser>,
    token: WindowToken,
    native: Arc<ContextMenuRegistration>,
    inbox: Arc<Inbox>,
    options: SourceViewOptions,
    retired: Cell<bool>,
    retire_submitted: Cell<bool>,
    retire_deferred: Cell<bool>,
    records: RefCell<Vec<Record>>,
    subscriptions: RefCell<Vec<Subscription>>,
    diagnostics: RefCell<SourceUiStatus>,
}
impl Route {
    fn error(&self, e: SourceUiError) {
        self.diagnostics.borrow_mut().first_error.get_or_insert(e);
    }
    fn mark_retired(&self) {
        self.retired.set(true);
        self.inbox.retire();
        self.diagnostics.borrow_mut().retired = true;
    }
    fn original_live(&self, cx: &App) -> bool {
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
    fn schedule_retire(self: &Rc<Self>, cx: &mut App) {
        self.mark_retired();
        if self.retire_deferred.replace(true) {
            return;
        }
        let route = self.clone();
        cx.defer(move |cx| {
            route.retire_deferred.set(false);
            route.retire_now(cx)
        });
    }
    fn retire_now(self: &Rc<Self>, cx: &mut App) {
        self.mark_retired();
        if !self.retire_submitted.replace(true) {
            if let Err(e) = self.native.retire() {
                self.error(SourceUiError::Runtime(e));
            }
        }
        // No RefCell/mutex or Entity guard across opaque calls/native posts.
        for id in self.inbox.ids() {
            if let Some(control) = self.inbox.control(id) {
                let q = control.status();
                if q.outcome.is_none() && !q.cancelled {
                    if let Err(e) = control.request_close() {
                        self.error(SourceUiError::Runtime(e));
                    }
                }
            }
        }
        self.dispatch(cx);
    }
    fn schedule_collect(self: &Rc<Self>, id: SourceOperationId, cx: &mut App) {
        let life = self
            .records
            .borrow()
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.life.clone());
        let Some(life) = life else { return };
        if !life.retired() || life.deferred.replace(true) {
            return;
        }
        let route = self.clone();
        // Fixed WindowsWindow::drop schedules DestroyWindow on the foreground
        // executor. An App.defer may run BEFORE that native destruction task.
        // After actual Entity/payload release, schedule ONE completion behind
        // it; no timer, receiver consumer, polling loop or native-close retry.
        cx.spawn(async move |cx: &mut gpui::AsyncApp| {
            cx.update(|cx| {
                life.deferred.set(false);
                route.collect(id, cx);
            });
        })
        .detach();
    }
    fn collect(self: &Rc<Self>, id: SourceOperationId, cx: &mut App) {
        let candidate = self
            .records
            .borrow()
            .iter()
            .find(|r| r.id == id)
            .map(|r| (r.life.retired(), r.native, r.handle, r.entity.clone()));
        let proof = candidate.is_some_and(|(retired, native, handle, entity)| {
            retired
                && !cx.windows().contains(&handle)
                && entity.upgrade().is_none()
                && native.as_ref().is_some_and(window::WindowProof::absent)
        });
        if !proof {
            return;
        }
        let record = {
            let mut rows = self.records.borrow_mut();
            let Some(index) = rows.iter().position(|r| r.id == id) else {
                return;
            };
            rows.remove(index)
        };
        drop(record);
        self.acknowledge_absence(id);
    }
    fn acknowledge_absence(&self, id: SourceOperationId) {
        // This path is also used when publication was rejected before any window
        // was made. Pending data must still be dropped BEFORE the host ack.
        let document = self.inbox.take_document(id);
        drop(document);
        let Some(control) = self.inbox.control(id) else {
            return;
        };
        if control.status().host_live && self.inbox.reserve_ack(id, false) {
            if let Err(e) = control.closed() {
                self.error(SourceUiError::Runtime(e));
                return;
            }
            self.diagnostics.borrow_mut().acknowledged += 1;
        }
        let q = control.status();
        if q.outcome.is_some() && !q.native_live && !q.host_live {
            let old = self.inbox.remove(id);
            self.diagnostics.borrow_mut().last_operation = Some(q);
            drop(old);
        }
    }
    fn close_window(self: &Rc<Self>, id: SourceOperationId, cx: &mut App) {
        let handle = {
            let mut rows = self.records.borrow_mut();
            let Some(r) = rows.iter_mut().find(|r| r.id == id) else {
                return;
            };
            if r.close_requested {
                return;
            }
            r.close_requested = true;
            r.handle
        };
        let result = handle.update(cx, |_, window, _| {
            if window.window_handle() != handle {
                return false;
            }
            window.remove_window();
            true
        });
        if !matches!(result, Ok(true)) && cx.windows().contains(&handle) {
            self.error(SourceUiError::WindowAccess);
            // Retain the attempted cut. Only retry_close, never an ordinary
            // wake/dispatch, authorizes another close attempt.
        }
        self.schedule_collect(id, cx);
    }
    fn dispatch(self: &Rc<Self>, cx: &mut App) {
        if !self.original_live(cx) && !self.retired.get() {
            self.schedule_retire(cx);
            return;
        }
        for id in self.inbox.ids() {
            let Some(control) = self.inbox.control(id) else {
                continue;
            };
            let q = control.status();
            let has_window = self.records.borrow().iter().any(|r| r.id == id);
            if self.inbox.closing(id)
                || q.cancelled
                || q.phase == SourcePhase::Closing
                || q.outcome.is_some()
            {
                if has_window {
                    self.close_window(id, cx);
                    // An explicit host dispatch can recheck a retained external
                    // Entity; no timer, polling consumer or hidden retry exists.
                    self.schedule_collect(id, cx);
                } else {
                    self.acknowledge_absence(id);
                }
                continue;
            }
            if has_window {
                let retired = self
                    .records
                    .borrow()
                    .iter()
                    .find(|r| r.id == id)
                    .is_some_and(|r| r.life.retired());
                if retired {
                    self.schedule_collect(id, cx);
                }
                continue;
            }
            if let Some(document) = self.inbox.take_document(id) {
                if !self.original_live(cx) || !control.is_current() {
                    drop(document);
                    if let Err(e) = control.request_close() {
                        self.error(SourceUiError::Runtime(e));
                    }
                    self.acknowledge_absence(id);
                    continue;
                }
                self.open(document, control, cx);
            }
        }
    }
    fn open(
        self: &Rc<Self>,
        document: SourceViewRequest,
        control: Arc<dyn SourceViewControl>,
        cx: &mut App,
    ) {
        let id = document.operation;
        let route = self.clone();
        let created = Rc::new(RefCell::new(None));
        let capture = created.clone();
        // GPUI text, never HTML. This display copy is bounded, not an RSS cap or
        // a claim of browser pre/tab/vertical-text rendering equivalence.
        let text = SharedString::from(document.source.as_str().to_owned());
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(self.options.bounds)),
            focus: self.options.focus,
            show: true,
            titlebar: Some(TitlebarOptions {
                title: Some("Page source".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = cx.open_window(options, move |window, cx| {
            let handle = window.window_handle();
            let native = window::WindowProof::capture(window);
            let life = Rc::new(Life::default());
            let entity = cx.new(|_| SourceViewer {
                document: Some(document),
                text: Some(text),
                _route: route.clone(),
                renders: 0,
            });
            let weak = Rc::downgrade(&route);
            let release_life = life.clone();
            let release = cx.observe_release(&entity, move |viewer, cx| {
                let payload = viewer.document.take();
                let text = viewer.text.take();
                release_life.entity_released.set(true);
                if let Some(route) = weak.upgrade() {
                    route.diagnostics.borrow_mut().entities_released += 1;
                    // Observe_release runs BEFORE Entity destruction. Drop both
                    // payload copies only after this borrow/window stack ends.
                    cx.defer(move |cx| {
                        drop(payload);
                        drop(text);
                        release_life.payload_retired.set(true);
                        route.diagnostics.borrow_mut().payloads_retired += 1;
                        route.schedule_collect(id, cx);
                    });
                } else {
                    cx.defer(move |_| {
                        drop(payload);
                        drop(text);
                    });
                }
            });
            let weak = Rc::downgrade(&route);
            let close_life = life.clone();
            let closed = cx.on_window_closed(move |cx, closed| {
                if closed != handle.window_id() {
                    return;
                }
                close_life.window_closed.set(true);
                if let Some(route) = weak.upgrade() {
                    route.diagnostics.borrow_mut().window_closed += 1;
                    // GPUI has removed the map entry, but Box<Window> is still
                    // on this observer's stack. This event alone is NOT an ack.
                    route.schedule_collect(id, cx);
                }
            });
            *capture.borrow_mut() = Some(Record {
                id,
                handle,
                entity: entity.downgrade(),
                life,
                native,
                close_requested: false,
                _subscriptions: vec![release, closed],
            });
            entity
        });
        let captured = created.borrow_mut().take();
        match (result, captured) {
            (Ok(_), Some(record)) => {
                let native_valid = record.native.is_some();
                self.records.borrow_mut().push(record);
                self.diagnostics.borrow_mut().created += 1;
                // Creation may focus its OWN viewer. Do not reuse the original
                // menu's focus revision after creation; presented checks the
                // still-live original operation, owner retirement/native epoch.
                if !native_valid {
                    self.error(SourceUiError::NativeWindow);
                    self.close_window(id, cx);
                } else if self.original_live(cx) && !control.status().cancelled {
                    if let Err(e) = control.presented() {
                        self.error(SourceUiError::Runtime(e));
                        self.close_window(id, cx);
                    }
                } else {
                    if let Err(e) = control.request_close() {
                        self.error(SourceUiError::Runtime(e));
                    }
                    self.close_window(id, cx);
                }
            }
            (Err(_), None) => {
                // Fixed App::open_window failed before invoking the root
                // constructor: no viewer Entity/display copy escaped this call.
                self.error(SourceUiError::CreateFailed);
                if let Err(e) = control.request_close() {
                    self.error(SourceUiError::Runtime(e));
                }
                self.acknowledge_absence(id);
            }
            (_, Some(record)) => {
                self.error(SourceUiError::CreateFailed);
                self.records.borrow_mut().push(record);
                self.close_window(id, cx);
            }
            (Ok(_), None) => {
                self.error(SourceUiError::CreateFailed); /* Unproved: retain barrier. */
            }
        }
    }
    fn status(&self) -> SourceUiStatus {
        let mut s = self.diagnostics.borrow().clone();
        s.operations = self.inbox.ids().len();
        s.live_windows = self.records.borrow().len();
        s
    }
}
struct SourceViewer {
    document: Option<SourceViewRequest>,
    text: Option<SharedString>,
    _route: Rc<Route>,
    renders: u64,
}
impl Render for SourceViewer {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.renders += 1;
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x171b22))
            .text_color(rgb(0xe7edf3))
            .child(div().p_3().text_sm().child("Page source — read only"))
            .child(
                div()
                    .id("source-text")
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .p_3()
                    .font_family("monospace")
                    .text_sm()
                    .whitespace_nowrap()
                    .child(self.text.clone().unwrap_or_default()),
            )
    }
}
/// Explicit UI-thread owner. Dispatch outside Entity/root-update borrows;
/// shutdown before dropping. Drop retires admission, NEVER claims a close.
/// GPUI 0.3.12 has no specified native owner option: these are ordinary
/// top-level windows, host-owned by the authenticated original View/window,
/// not OS-owned/modal children. No Window::activate_window is used.
pub struct SourceViewServices {
    capacity: NonZeroUsize,
    options: SourceViewOptions,
    routes: Vec<Rc<Route>>,
}
impl SourceViewServices {
    pub fn new(capacity: NonZeroUsize, options: SourceViewOptions) -> Self {
        Self {
            capacity,
            options,
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
    ) -> Result<(), SourceUiError> {
        let (token, original, _, _) = view.read(cx).menu_attachment();
        let browser = token.binding().browser;
        if original != window.window_handle() {
            return Err(SourceUiError::WrongWindow);
        }
        if !token.is_live() {
            return Err(SourceUiError::Retired);
        }
        if self.routes.iter().any(|r| r.browser == browser) {
            return Err(SourceUiError::Duplicate);
        }
        if self.routes.len() >= self.capacity.get() {
            return Err(SourceUiError::Capacity);
        }
        self.routes
            .try_reserve(1)
            .map_err(|_| SourceUiError::Capacity)?;
        let target = menus.target(browser).map_err(|_| SourceUiError::Missing)?;
        if !native
            .matches_target(&target)
            .map_err(SourceUiError::Runtime)?
        {
            return Err(SourceUiError::WrongRegistration);
        }
        let inbox = Inbox::new(
            token.binding(),
            target.lease,
            self.options.max_operations,
            self.options.max_source_bytes,
        );
        let route = Rc::new(Route {
            browser,
            original,
            view: view.downgrade(),
            token,
            native,
            inbox,
            options: self.options.clone(),
            retired: Cell::new(false),
            retire_submitted: Cell::new(false),
            retire_deferred: Cell::new(false),
            records: RefCell::new(Vec::new()),
            subscriptions: RefCell::new(Vec::new()),
            diagnostics: RefCell::new(SourceUiStatus::default()),
        });
        let weak = Rc::downgrade(&route);
        let release = cx.observe_release(view, move |_, cx| {
            if let Some(route) = weak.upgrade() {
                route.schedule_retire(cx);
            }
        });
        let weak = Rc::downgrade(&route);
        let closed = cx.on_window_closed(move |cx, id| {
            if id == original.window_id() {
                if let Some(route) = weak.upgrade() {
                    route.schedule_retire(cx);
                }
            }
        });
        route.subscriptions.borrow_mut().extend([release, closed]);
        route
            .native
            .set_source_viewer(route.inbox.clone())
            .map_err(SourceUiError::Runtime)?;
        self.routes.push(route);
        Ok(())
    }
    pub fn dispatch(&mut self, cx: &mut App) {
        for route in &self.routes {
            route.dispatch(cx);
        }
    }
    /// Reclaim registration capacity only after original admission is retired
    /// and every native/host operation and tracked viewer has actually drained.
    pub fn unregister(&mut self, browser: BrowserId) -> Result<SourceUiStatus, SourceUiError> {
        let index = self
            .routes
            .iter()
            .position(|r| r.browser == browser)
            .ok_or(SourceUiError::Missing)?;
        let status = self.routes[index].status();
        if !status.retired
            || status.operations != 0
            || status.live_windows != 0
            || !self.routes[index].retire_submitted.get()
        {
            return Err(SourceUiError::NotDrained);
        }
        let old = self.routes.remove(index);
        drop(old);
        Ok(status)
    }
    /// Explicit host-selected retry; first errors remain observable. Ordinary
    /// dispatch/notifications do not repeat failed removal or closed ack calls.
    pub fn retry_close(
        &mut self,
        browser: BrowserId,
        id: SourceOperationId,
        cx: &mut App,
    ) -> Result<(), SourceUiError> {
        let route = self
            .routes
            .iter()
            .find(|r| r.browser == browser)
            .ok_or(SourceUiError::Missing)?;
        let control = route.inbox.control(id).ok_or(SourceUiError::Missing)?;
        control.request_close().map_err(SourceUiError::Runtime)?;
        route.inbox.reset_ack(id);
        let has_window = {
            let mut rows = route.records.borrow_mut();
            if let Some(r) = rows.iter_mut().find(|r| r.id == id) {
                r.close_requested = false;
                true
            } else {
                false
            }
        };
        if has_window {
            route.close_window(id, cx);
        } else {
            route.acknowledge_absence(id);
        }
        Ok(())
    }
    pub fn retry_retire(&mut self, browser: BrowserId) -> Result<(), SourceUiError> {
        let route = self
            .routes
            .iter()
            .find(|r| r.browser == browser)
            .ok_or(SourceUiError::Missing)?;
        if !route.retired.get() {
            return Err(SourceUiError::Retired);
        }
        route.native.retire().map_err(SourceUiError::Runtime)
    }
    pub fn shutdown(&mut self, cx: &mut App) {
        for route in &self.routes {
            route.schedule_retire(cx);
        }
    }
    pub fn status(&self, browser: BrowserId) -> Result<SourceUiStatus, SourceUiError> {
        self.routes
            .iter()
            .find(|r| r.browser == browser)
            .map(|r| r.status())
            .ok_or(SourceUiError::Missing)
    }
    pub fn operations(
        &self,
        browser: BrowserId,
    ) -> Result<Vec<SourceOperationStatus>, SourceUiError> {
        let route = self
            .routes
            .iter()
            .find(|r| r.browser == browser)
            .ok_or(SourceUiError::Missing)?;
        Ok(route
            .inbox
            .ids()
            .into_iter()
            .filter_map(|id| route.inbox.control(id).map(|c| c.status()))
            .collect())
    }
    /// Weak/handle-only validation; no strong Entity escapes.
    pub fn windows(&self, browser: BrowserId) -> Result<Vec<AnyWindowHandle>, SourceUiError> {
        self.routes
            .iter()
            .find(|r| r.browser == browser)
            .map(|r| r.records.borrow().iter().map(|r| r.handle).collect())
            .ok_or(SourceUiError::Missing)
    }
    #[cfg(feature = "validation")]
    pub fn validation_documents(
        &self,
        browser: BrowserId,
        cx: &App,
    ) -> Result<Vec<(SourceOperationId, usize, u64, bool, u64)>, SourceUiError> {
        let route = self
            .routes
            .iter()
            .find(|r| r.browser == browser)
            .ok_or(SourceUiError::Missing)?;
        let entities: Vec<_> = route
            .records
            .borrow()
            .iter()
            .map(|r| (r.id, r.entity.clone()))
            .collect();
        Ok(entities
            .into_iter()
            .filter_map(|(id, weak)| {
                let entity = weak.upgrade()?;
                let viewer = entity.read(cx);
                let text = viewer.text.as_ref()?;
                let hash = text
                    .as_bytes()
                    .iter()
                    .fold(14695981039346656037u64, |h, b| {
                        (h ^ *b as u64).wrapping_mul(1099511628211)
                    });
                Some((
                    id,
                    text.len(),
                    hash,
                    text.contains("<span>menu bridge selection</span>"),
                    viewer.renders,
                ))
            })
            .collect())
    }
}
impl Drop for SourceViewServices {
    fn drop(&mut self) {
        for route in &self.routes {
            route.mark_retired();
        }
    }
}
