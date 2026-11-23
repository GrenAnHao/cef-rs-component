//! Thin GPUI adapter over the UI-framework-agnostic Win32 native popup menu
//! host (`rust_cef_runtime::win32_menu::Win32ContextMenuHost`).
//!
//! All native Win32 menu construction, cascading submenus, `&` mnemonics,
//! keyboard navigation, Escape/outside-click/focus-loss dismissal, and modal
//! tracking live in `rust_cef_runtime::win32_menu` with zero GPUI dependency,
//! so migrating to another rendering framework (e.g. Rune) only replaces this
//! thin HWND/DIP-to-screen adapter.
use crate::{CefBrowser, binding::WindowToken, menu::*};
use gpui::{
    AnyElement, AnyWindowHandle, App, Entity, FocusHandle, Point, Subscription, WeakEntity, Window,
    px,
};
use rust_cef_core::ports::{ContextMenuResponseSink, EngineError};
use rust_cef_runtime::win32_menu::{
    Win32ContextMenuHost, Win32MenuAnchor, Win32MenuObservation, Win32MenuOutcome,
    Win32MenuRequest, Win32MenuToken,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};

#[cfg(test)]
mod tests;

const MAX_DEFERRED_REPLIES: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuPresenterFailure {
    WrongWindow,
    OriginalNotReady,
    StaleRequest,
    BindingOrAnchorMismatch,
    InvalidGeometry,
    Model(MenuTreeError),
    Context(rust_cef_core::menu_context::MenuContextError),
    Focus(MenuServiceError),
    Capacity,
    Reply(EngineError),
}
impl MenuPresenterFailure {
    pub fn engine_error(&self) -> EngineError {
        match self {
            Self::OriginalNotReady | Self::Focus(_) => EngineError::NotReady,
            Self::Capacity => EngineError::Backpressure,
            Self::Reply(e) => e.clone(),
            _ => EngineError::InvalidRequest,
        }
    }
}

/// Submission is an admission attempt, not a native action/effect acknowledgement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuPresenterReply {
    pub request: ContextMenuRequestId,
    pub requested: ContextMenuResponse,
    pub submitted: ContextMenuResponse,
    pub result: Result<(), EngineError>,
}
/// Retained for API compatibility with hosts that used the previous presenter.
/// The presenter delegates directly to Win32 native popup menus via
/// `rust_cef_runtime::win32_menu`, so no UI toolkit initialization is required.
pub fn init_default_menu_ui(_cx: &mut App) {}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MenuBounds {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}
impl MenuBounds {
    fn from_dip(left: f32, top: f32, width: f32, height: f32) -> Option<Self> {
        let b = Self {
            left,
            top,
            width,
            height,
        };
        ([b.left, b.top, b.width, b.height]
            .iter()
            .all(|v| v.is_finite())
            && b.width > 0.
            && b.height > 0.)
            .then_some(b)
    }
    #[cfg(test)]
    fn from_bounds(bounds: gpui::Bounds<gpui::Pixels>) -> Option<Self> {
        Self::from_dip(
            bounds.origin.x.into(),
            bounds.origin.y.into(),
            bounds.size.width.into(),
            bounds.size.height.into(),
        )
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct MenuCommandBounds {
    pub command_id: i32,
    pub enabled: bool,
    pub bounds: Option<MenuBounds>,
}
/// Last observed native Win32 menu bounds in original-window client DIPs.
#[derive(Clone, Debug, PartialEq)]
pub struct MenuPresenterLayout {
    pub request: ContextMenuRequestId,
    pub menu: Option<MenuBounds>,
    pub commands: Vec<MenuCommandBounds>,
}
impl MenuPresenterLayout {
    fn new(request: ContextMenuRequestId, model: &MenuModel) -> Self {
        let mut commands = Vec::new();
        let mut add = |c: &MenuCommand, parent_enabled: bool| {
            if c.visible {
                commands.push(MenuCommandBounds {
                    command_id: c.command_id,
                    enabled: parent_enabled && c.enabled,
                    bounds: None,
                });
            }
        };
        for row in &model.entries {
            match row {
                MenuItem::Command(c) => add(c, true),
                MenuItem::Submenu(sub) if sub.visible => {
                    for row in &sub.entries {
                        if let MenuChild::Command(c) = row {
                            add(c, sub.enabled);
                        }
                    }
                }
                _ => {}
            }
        }
        Self {
            request,
            menu: None,
            commands,
        }
    }
    #[cfg(test)]
    fn command(&mut self, id: i32, bounds: gpui::Bounds<gpui::Pixels>) {
        if let Some(row) = self.commands.iter_mut().find(|row| row.command_id == id) {
            row.bounds = MenuBounds::from_bounds(bounds);
        }
    }
    fn apply_win32_observation(
        &mut self,
        obs: &Win32MenuObservation,
        owner_hwnd: isize,
        scale: f32,
    ) {
        self.menu = obs
            .menu_rect
            .and_then(|r| r.to_client_dip(owner_hwnd, scale))
            .and_then(|(l, t, w, h)| MenuBounds::from_dip(l, t, w, h));
        for item in &obs.items {
            if let Some(row) = self
                .commands
                .iter_mut()
                .find(|row| row.command_id == item.command_id)
            {
                row.bounds = item
                    .rect
                    .and_then(|r| r.to_client_dip(owner_hwnd, scale))
                    .and_then(|(l, t, w, h)| MenuBounds::from_dip(l, t, w, h));
            }
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MenuPresenterStatus {
    pub retired: bool,
    /// Logical open presentation, NOT proof of paint, visibility or native liveness.
    pub open_request: Option<ContextMenuRequestId>,
    pub deferred_replies: usize,
    pub last_reply: Option<MenuPresenterReply>,
    pub first_error: Option<MenuPresenterFailure>,
    pub last_layout: Option<MenuPresenterLayout>,
    pub dismiss_click: Option<[f32; 2]>,
    pub dismiss_popup: Option<MenuBounds>,
    /// Command ids selected from the menu, in order.
    pub clicked_rows: Vec<i32>,
    pub opens_seen: u64,
    pub dismiss_updates: u64,
    pub withdraws: u64,
}

/// No opaque endpoint is called or released under this cell's borrow.
struct Reply {
    id: ContextMenuRequestId,
    port: RefCell<Option<Arc<dyn ContextMenuResponseSink>>>,
    choice: Cell<Option<ContextMenuResponse>>,
    refused: Cell<bool>,
    withdrawn: Cell<bool>,
}
impl Reply {
    fn claim(&self, choice: ContextMenuResponse) -> Option<Arc<dyn ContextMenuResponseSink>> {
        if self.choice.get().is_some() {
            return None;
        }
        let port = self.port.borrow_mut().take();
        if port.is_some() {
            self.choice.set(Some(choice));
        }
        port
    }
    fn attempt(
        &self,
        port: Arc<dyn ContextMenuResponseSink>,
        requested: ContextMenuResponse,
        original_current: bool,
    ) -> MenuPresenterReply {
        let submitted = self.actual(requested, original_current);
        let result = port.submit(self.id, submitted);
        self.refused.set(result.is_err());
        drop(port);
        MenuPresenterReply {
            request: self.id,
            requested,
            submitted,
            result,
        }
    }
    fn presented(&self) -> bool {
        !self.withdrawn.get() && (self.choice.get().is_none() || self.refused.get())
    }
    fn actual(
        &self,
        requested: ContextMenuResponse,
        original_current: bool,
    ) -> ContextMenuResponse {
        if self.withdrawn.get() || !original_current {
            ContextMenuResponse::Cancel
        } else {
            requested
        }
    }
}
struct Open {
    id: ContextMenuRequestId,
    focus: MenuFocusRegistration,
    surface: (f32, f32, f32, f32),
    layout: Rc<RefCell<MenuPresenterLayout>>,
    reply: Rc<Reply>,
    win32_token: Cell<Option<Win32MenuToken>>,
}
struct State {
    original: WeakEntity<CefBrowser>,
    window: AnyWindowHandle,
    owner_hwnd: isize,
    win32_host: Win32ContextMenuHost,
    token: WindowToken,
    browser_focus: FocusHandle,
    retired: Cell<bool>,
    pending: Cell<usize>,
    high_water: Cell<u64>,
    open: RefCell<Option<Open>>,
    last_reply: RefCell<Option<MenuPresenterReply>>,
    first_error: RefCell<Option<MenuPresenterFailure>>,
    dismiss_click: Cell<Option<[f32; 2]>>,
    dismiss_popup: RefCell<Option<MenuBounds>>,
    clicked_rows: RefCell<Vec<i32>>,
    opens_seen: Cell<u64>,
    dismiss_updates: Cell<u64>,
    withdraws: Cell<u64>,
    layout_observer: RefCell<Option<Box<dyn FnMut(&MenuPresenterLayout, f32)>>>,
}
impl State {
    fn reject(&self, e: MenuPresenterFailure) -> EngineError {
        let engine = e.engine_error();
        self.fail(e);
        engine
    }
    fn fail(&self, e: MenuPresenterFailure) {
        let mut first = self.first_error.borrow_mut();
        if first.is_none() {
            *first = Some(e);
        }
    }
    fn original_live(&self, cx: &App) -> bool {
        !self.retired.get()
            && self.token.is_live()
            && self.original.upgrade().is_some()
            && cx.windows().contains(&self.window)
    }
    fn current(&self, reply: &Rc<Reply>, cx: &App) -> bool {
        if !self.original_live(cx) {
            return false;
        }
        let Some(view) = self.original.upgrade() else {
            return false;
        };
        let d = view.read(cx).diagnostics();
        self.open.borrow().as_ref().is_some_and(|o| {
            Rc::ptr_eq(&o.reply, reply) && o.focus.is_current() && o.surface == d.surface_bounds
        })
    }
    fn current_without_app(&self, reply: &Rc<Reply>) -> bool {
        !self.retired.get()
            && self.token.is_live()
            && self
                .open
                .borrow()
                .as_ref()
                .is_some_and(|o| Rc::ptr_eq(&o.reply, reply) && o.focus.is_current())
    }
    fn record_reply(&self, reply: &Rc<Reply>, status: MenuPresenterReply) {
        if let Err(e) = &status.result {
            self.fail(MenuPresenterFailure::Reply(e.clone()));
        }
        let ok = status.result.is_ok();
        {
            let mut last = self.last_reply.borrow_mut();
            if last
                .as_ref()
                .is_none_or(|old| old.request.sequence <= reply.id.sequence)
            {
                *last = Some(status);
            }
        }
        if ok {
            let matches = self
                .open
                .borrow()
                .as_ref()
                .is_some_and(|o| Rc::ptr_eq(&o.reply, reply));
            if matches {
                let old = self.open.borrow_mut().take();
                drop(old);
            }
        }
    }
    fn queue(
        self: &Rc<Self>,
        reply: Rc<Reply>,
        requested: ContextMenuResponse,
        cx: &mut App,
    ) -> Result<(), EngineError> {
        if reply.choice.get().is_some() {
            return Ok(());
        }
        if self.pending.get() >= MAX_DEFERRED_REPLIES {
            return Err(self.reject(MenuPresenterFailure::Capacity));
        }
        let Some(port) = reply.claim(requested) else {
            return Ok(());
        };
        self.pending.set(self.pending.get() + 1);
        let weak = Rc::downgrade(self);
        cx.defer(move |cx| {
            let state = weak.upgrade();
            let live = state.as_ref().is_some_and(|s| {
                s.current(&reply, cx)
                    && s.window
                        .update(cx, |_, window, _| {
                            crate::native_view::is_window_effectively_active(window)
                        })
                        .unwrap_or(false)
            });
            // No Entity, Window, presenter RefCell or internal lock is borrowed.
            let status = reply.attempt(port, requested, live);
            let Some(s) = state else {
                return;
            };
            s.pending.set(s.pending.get() - 1);
            s.record_reply(&reply, status);
        });
        Ok(())
    }
    fn withdraw(self: &Rc<Self>, cx: &mut App) -> Result<(), EngineError> {
        self.withdraws.set(self.withdraws.get() + 1);
        let (reply, token) = self
            .open
            .borrow()
            .as_ref()
            .map(|o| (o.reply.clone(), o.win32_token.get()))
            .unzip();
        if let Some(reply) = reply {
            reply.withdrawn.set(true);
            if let Some(tok) = token.flatten() {
                self.win32_host.cancel(Some(tok));
            }
            self.queue(reply, ContextMenuResponse::Cancel, cx)?;
        }
        let old = self.open.borrow_mut().take();
        drop(old);
        Ok(())
    }
    fn retire(self: &Rc<Self>, cx: &mut App) -> Result<(), EngineError> {
        self.retired.set(true);
        self.withdraw(cx)
    }
}

fn extract_win32_hwnd(window: &Window) -> Result<isize, MenuPresenterFailure> {
    let raw = raw_window_handle::HasWindowHandle::window_handle(window)
        .map_err(|_| MenuPresenterFailure::WrongWindow)?
        .as_raw();
    match raw {
        raw_window_handle::RawWindowHandle::Win32(h) => Ok(h.hwnd.get()),
        _ => Err(MenuPresenterFailure::WrongWindow),
    }
}

/// UI-thread-only GPUI adapter for one original View and its original window.
///
/// Actual popup menu rendering and interaction are delegated to the
/// framework-independent [`Win32ContextMenuHost`]. `element()` returns `None`
/// because no GPUI overlay is rendered.
pub struct DefaultMenuPresenter {
    state: Rc<State>,
    limits: MenuLimits,
    context_limits: MenuContextLimits,
    _subscriptions: Vec<Subscription>,
}
impl DefaultMenuPresenter {
    pub fn new(
        view: &Entity<CefBrowser>,
        window: &Window,
        limits: MenuLimits,
        context_limits: MenuContextLimits,
        cx: &mut App,
    ) -> Result<Self, MenuPresenterFailure> {
        let (token, original, _, browser_focus) = view.read(cx).menu_attachment();
        if original != window.window_handle() {
            return Err(MenuPresenterFailure::WrongWindow);
        }
        if !token.is_live() {
            return Err(MenuPresenterFailure::OriginalNotReady);
        }
        let owner_hwnd = extract_win32_hwnd(window)?;
        let win32_host =
            Win32ContextMenuHost::new().map_err(|_| MenuPresenterFailure::OriginalNotReady)?;
        let state = Rc::new(State {
            original: view.downgrade(),
            window: original,
            owner_hwnd,
            win32_host,
            token,
            browser_focus,
            retired: Cell::new(false),
            pending: Cell::new(0),
            high_water: Cell::new(0),
            open: RefCell::new(None),
            last_reply: RefCell::new(None),
            first_error: RefCell::new(None),
            dismiss_click: Cell::new(None),
            dismiss_popup: RefCell::new(None),
            clicked_rows: RefCell::new(Vec::new()),
            opens_seen: Cell::new(0),
            dismiss_updates: Cell::new(0),
            withdraws: Cell::new(0),
            layout_observer: RefCell::new(None),
        });
        let weak = Rc::downgrade(&state);
        let release = cx.observe_release(view, move |_, cx| {
            if let Some(s) = weak.upgrade() {
                let _ = s.retire(cx);
            }
        });
        let weak = Rc::downgrade(&state);
        let closed = cx.on_window_closed(move |cx, id| {
            if id == original.window_id() {
                if let Some(s) = weak.upgrade() {
                    let _ = s.retire(cx);
                }
            }
        });
        Ok(Self {
            state,
            limits,
            context_limits,
            _subscriptions: vec![release, closed],
        })
    }
    /// Pass this callback to `ContextMenuServices::dispatch`.
    pub fn update(
        &mut self,
        event: &MenuUiUpdate,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), EngineError> {
        let s = &self.state;
        if window.window_handle() != s.window {
            return Err(s.reject(MenuPresenterFailure::WrongWindow));
        }
        match event {
            MenuUiUpdate::Dismiss { request } => {
                s.dismiss_updates.set(s.dismiss_updates.get() + 1);
                let exact = s.open.borrow().as_ref().is_some_and(|o| o.id == *request);
                if exact {
                    s.withdraw(cx)?;
                }
                Ok(())
            }
            MenuUiUpdate::Open {
                presentation,
                response,
                focus,
            } => {
                if !s.original_live(cx) || !crate::native_view::is_window_effectively_active(window)
                {
                    return Err(s.reject(MenuPresenterFailure::OriginalNotReady));
                }
                let id = presentation.request;
                if id.browser != s.token.binding().browser || id.sequence <= s.high_water.get() {
                    return Err(s.reject(MenuPresenterFailure::StaleRequest));
                }
                presentation
                    .model
                    .validate(self.limits)
                    .map_err(|e| s.reject(MenuPresenterFailure::Model(e)))?;
                presentation
                    .context
                    .validate(id.browser, self.context_limits)
                    .map_err(|e| s.reject(MenuPresenterFailure::Context(e)))?;
                let view = s
                    .original
                    .upgrade()
                    .ok_or_else(|| s.reject(MenuPresenterFailure::OriginalNotReady))?;
                let d = view.read(cx).diagnostics();
                if presentation.context.binding != s.token.binding()
                    || !focus.matches_presentation(presentation)
                    || (presentation.model.x, presentation.model.y)
                        != (presentation.context.x, presentation.context.y)
                {
                    return Err(s.reject(MenuPresenterFailure::BindingOrAnchorMismatch));
                }
                let origin = anchor(d.surface_bounds, presentation.model.x, presentation.model.y)
                    .ok_or_else(|| s.reject(MenuPresenterFailure::InvalidGeometry))?;
                let scale = window.scale_factor();
                let win32_anchor = Win32MenuAnchor::from_client_dip(
                    s.owner_hwnd,
                    f32::from(origin.x),
                    f32::from(origin.y),
                    scale,
                )
                .map_err(|_| s.reject(MenuPresenterFailure::InvalidGeometry))?;

                let needs_cancel = s
                    .open
                    .borrow()
                    .as_ref()
                    .is_some_and(|o| o.reply.choice.get().is_none());
                if !can_open(s.pending.get(), needs_cancel) {
                    return Err(s.reject(MenuPresenterFailure::Capacity));
                }
                s.withdraw(cx)?;
                focus
                    .declare(s.browser_focus.clone(), window, cx)
                    .map_err(|e| s.reject(MenuPresenterFailure::Focus(e)))?;

                let reply = Rc::new(Reply {
                    id,
                    port: RefCell::new(None),
                    choice: Cell::new(None),
                    refused: Cell::new(false),
                    withdrawn: Cell::new(false),
                });
                let layout = Rc::new(RefCell::new(MenuPresenterLayout::new(
                    id,
                    &presentation.model,
                )));

                let observe_layout = layout.clone();
                let observe_state = Rc::downgrade(s);
                let owner_hwnd = s.owner_hwnd;
                let complete_state = Rc::downgrade(s);
                let complete_reply = reply.clone();
                let token = s
                    .win32_host
                    .schedule_open(
                        Win32MenuRequest {
                            anchor: win32_anchor,
                            model: presentation.model.clone(),
                        },
                        Box::new(move |obs| {
                            let snapshot = {
                                let mut guard = observe_layout.borrow_mut();
                                guard.apply_win32_observation(&obs, owner_hwnd, scale);
                                guard.clone()
                            };
                            if let Some(st) = observe_state.upgrade()
                                && let Ok(mut guard) = st.layout_observer.try_borrow_mut()
                                && let Some(cb) = guard.as_mut()
                            {
                                cb(&snapshot, scale);
                            }
                        }),
                        Box::new(move |outcome| {
                            let requested = match outcome {
                                Win32MenuOutcome::Selected { command_id } => {
                                    if let Some(st) = complete_state.upgrade() {
                                        st.clicked_rows.borrow_mut().push(command_id);
                                    }
                                    ContextMenuResponse::Select { command_id }
                                }
                                Win32MenuOutcome::Cancelled => ContextMenuResponse::Cancel,
                            };
                            let Some(port) = complete_reply.claim(requested) else {
                                return;
                            };
                            let state = complete_state.upgrade();
                            let live = state
                                .as_ref()
                                .is_some_and(|st| st.current_without_app(&complete_reply));
                            // Out-of-lock, top-level Win32 message loop: zero GPUI borrows held.
                            let status = complete_reply.attempt(port, requested, live);
                            if let Some(st) = state {
                                st.record_reply(&complete_reply, status);
                            }
                        }),
                    )
                    .map_err(|_| s.reject(MenuPresenterFailure::InvalidGeometry))?;

                // Retain the opaque port only after every fallible step succeeded.
                *reply.port.borrow_mut() = Some(response.clone());
                s.opens_seen.set(s.opens_seen.get() + 1);
                s.high_water.set(id.sequence);
                *s.open.borrow_mut() = Some(Open {
                    id,
                    focus: focus.clone(),
                    surface: d.surface_bounds,
                    layout,
                    reply,
                    win32_token: Cell::new(Some(token)),
                });
                Ok(())
            }
        }
    }
    /// Native Win32 menus are OS-rendered popup windows, so no GPUI element is
    /// added to the host render tree. Calling this still enforces the window
    /// activity guard if the host keeps it in its render path.
    pub fn element(&self, window: &mut Window, cx: &mut App) -> Option<AnyElement> {
        let s = &self.state;
        if window.window_handle() != s.window {
            return None;
        }
        let reply = s.open.borrow().as_ref().map(|o| o.reply.clone())?;
        if !s.current(&reply, cx) || !crate::native_view::is_window_effectively_active(window) {
            let _ = s.withdraw(cx);
        }
        None
    }
    pub fn status(&self) -> MenuPresenterStatus {
        let s = &self.state;
        MenuPresenterStatus {
            retired: s.retired.get(),
            open_request: s
                .open
                .borrow()
                .as_ref()
                .filter(|o| o.reply.presented())
                .map(|o| o.id),
            deferred_replies: s.pending.get(),
            last_reply: s.last_reply.borrow().clone(),
            first_error: s.first_error.borrow().clone(),
            last_layout: s.open.borrow().as_ref().map(|o| o.layout.borrow().clone()),
            dismiss_click: s.dismiss_click.get(),
            dismiss_popup: s.dismiss_popup.borrow().clone(),
            clicked_rows: s.clicked_rows.borrow().clone(),
            opens_seen: s.opens_seen.get(),
            dismiss_updates: s.dismiss_updates.get(),
            withdraws: s.withdraws.get(),
        }
    }
    pub fn set_layout_observer(&self, observer: Option<Box<dyn FnMut(&MenuPresenterLayout, f32)>>) {
        *self.state.layout_observer.borrow_mut() = observer;
    }
    pub fn shutdown(&mut self, cx: &mut App) -> Result<(), EngineError> {
        self.state.retire(cx)
    }
}
impl Drop for DefaultMenuPresenter {
    fn drop(&mut self) {
        self.state.retired.set(true);
        self.state.win32_host.cancel(None);
    }
}

fn can_open(pending: usize, needs_cancel: bool) -> bool {
    pending
        .checked_add(usize::from(needs_cancel))
        .is_some_and(|n| n < MAX_DEFERRED_REPLIES)
}
fn anchor(surface: (f32, f32, f32, f32), x: i32, y: i32) -> Option<Point<gpui::Pixels>> {
    let (left, top, width, height) = surface;
    if ![left, top, width, height].iter().all(|v| v.is_finite())
        || width <= 0.
        || height <= 0.
        || x < 0
        || y < 0
        || x as f32 >= width
        || y as f32 >= height
    {
        return None;
    }
    let (x, y) = (left + x as f32, top + y as f32);
    (x.is_finite() && y.is_finite()).then(|| Point { x: px(x), y: px(y) })
}
#[cfg(test)]
fn point_in_bounds(at: [f32; 2], rect: Option<MenuBounds>) -> bool {
    rect.is_some_and(|r| {
        at[0].is_finite()
            && at[1].is_finite()
            && at[0] >= r.left
            && at[0] <= r.left + r.width
            && at[1] >= r.top
            && at[1] <= r.top + r.height
    })
}
#[cfg(test)]
fn marker(command: &MenuCommand) -> &'static str {
    match (command.radio_group, command.checked) {
        (Some(_), Some(true)) => "●",
        (Some(_), _) => "○",
        (None, Some(true)) => "☑",
        (None, Some(false)) => "☐",
        (None, None) => "",
    }
}
