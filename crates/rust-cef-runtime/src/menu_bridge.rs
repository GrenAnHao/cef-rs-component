//! Explicit, UI-toolkit-free production menu binding. Target publication and
//! terminal notification are always outside registry/request/data locks.
use crate::{client::BrowserClientState, registry::UiRegistry};
use cef::*;
use rust_cef_core::{
    contracts::BrowserId,
    menu::{ContextMenuRequestId, ContextMenuResponse, MenuLimits},
    menu_context::MenuContextLimits,
    menu_lease::{MenuLease, MenuLeaseStamp},
    menu_model::{MenuChild, MenuCommand, MenuItem, MenuModel, MenuSubmenu},
    ports::{ContextMenuDeliverySink, ContextMenuResponseSink, ContextMenuTarget, EngineError},
};
use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

#[derive(Clone, Copy, Debug)]
pub struct ContextMenuConfig {
    pub capacity: NonZeroUsize,
    pub model_limits: MenuLimits,
    pub context_limits: MenuContextLimits,
}
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "test-support", derive(serde::Serialize))]
pub struct ContextMenuStatus {
    pub opened: u64,
    pub published: u64,
    pub cancelled: u64,
    pub actions_submitted: u64,
    pub native_dismissed: u64,
    pub invalidated: u64,
    pub post_failures: u64,
    pub retired: bool,
    pub first_failure: Option<String>,
    /// Published, then found no longer pending when re-checked.
    pub cancel_unpending: u64,
    /// Published, then the UI wake refused.
    pub cancel_wake_refused: u64,
    /// Ordered trace of the publish/cancel decisions for the current menu.
    pub trace: Vec<String>,
}
pub(crate) struct Binding {
    pub browser: BrowserId,
    pub serial: u64,
    pub target: ContextMenuTarget,
    pub source: std::sync::OnceLock<Arc<dyn rust_cef_core::source_view::SourceViewSink>>,
    pub find: std::sync::OnceLock<Arc<dyn rust_cef_core::find::FindResultSink>>,
    pub spelling: crate::spelling::Spelling,
    pub retired: AtomicBool,
    pub epoch: AtomicU64,
    status: Mutex<ContextMenuStatus>,
    #[cfg(feature = "test-support")]
    pub fixture: std::sync::atomic::AtomicU8,
    #[cfg(feature = "test-support")]
    pub spelling_probe: crate::spelling_probe::Book,
}
impl Binding {
    pub fn note(&self, f: impl FnOnce(&mut ContextMenuStatus)) {
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut s);
    }
    pub fn fail(&self, reason: impl Into<String>) {
        self.note(|s| {
            s.first_failure.get_or_insert(reason.into());
        });
    }
    pub fn snapshot(&self) -> ContextMenuStatus {
        let mut s = self
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        s.retired = self.retired.load(Ordering::Acquire);
        s
    }
    pub fn advance(&self) {
        // Called on CEF UI only; retirement itself may happen on the host.
        let epoch = self.epoch.load(Ordering::Acquire);
        if let Some(next) = epoch.checked_add(1) {
            self.epoch.store(next, Ordering::Release);
        } else {
            self.retired.store(true, Ordering::Release);
            self.fail("native menu cutpoint sequence exhausted");
        }
        self.note(|s| s.invalidated += 1);
    }
}
struct Routes {
    config: Option<ContextMenuConfig>,
    next_serial: u64,
    bindings: BTreeMap<BrowserId, Arc<Binding>>,
}
pub(crate) struct MenuBridge {
    routes: Mutex<Routes>,
}
impl MenuBridge {
    pub fn new() -> Self {
        Self {
            routes: Mutex::new(Routes {
                config: None,
                next_serial: 1,
                bindings: BTreeMap::new(),
            }),
        }
    }
    pub fn configure(
        &self,
        config: ContextMenuConfig,
        configure_book: impl FnOnce() -> Result<(), EngineError>,
    ) -> Result<(), EngineError> {
        let mut r = self
            .routes
            .lock()
            .map_err(|_| EngineError::InvalidRequest)?;
        if r.config.is_some() {
            return Err(EngineError::InvalidRequest);
        }
        configure_book()?;
        r.config = Some(config);
        Ok(())
    }
    pub fn config(&self) -> Option<ContextMenuConfig> {
        self.routes.lock().ok()?.config
    }
    pub fn get(&self, browser: BrowserId) -> Option<Arc<Binding>> {
        self.routes.lock().ok()?.bindings.get(&browser).cloned()
    }
    pub fn bind(
        &self,
        browser: BrowserId,
        target: &ContextMenuTarget,
        old_drained: bool,
    ) -> Result<(Arc<Binding>, Option<Arc<Binding>>), EngineError> {
        let mut r = self
            .routes
            .lock()
            .map_err(|_| EngineError::InvalidRequest)?;
        let config = r.config.ok_or(EngineError::NotReady)?;
        if r.bindings
            .get(&browser)
            .is_some_and(|b| !b.retired.load(Ordering::Acquire) || !old_drained)
        {
            return Err(EngineError::Backpressure);
        }
        if !r.bindings.contains_key(&browser) && r.bindings.len() >= config.capacity.get() {
            return Err(EngineError::Backpressure);
        }
        let serial = r.next_serial;
        if serial == 0 {
            return Err(EngineError::InvalidRequest);
        }
        r.next_serial = serial.checked_add(1).unwrap_or(0);
        let b = Arc::new(Binding {
            browser,
            serial,
            target: target.clone(),
            source: std::sync::OnceLock::new(),
            find: std::sync::OnceLock::new(),
            spelling: crate::spelling::Spelling::new(),
            retired: AtomicBool::new(false),
            epoch: AtomicU64::new(1),
            status: Mutex::new(ContextMenuStatus::default()),
            #[cfg(feature = "test-support")]
            fixture: std::sync::atomic::AtomicU8::new(0),
            #[cfg(feature = "test-support")]
            spelling_probe: crate::spelling_probe::Book::new(),
        });
        let old = r.bindings.insert(browser, b.clone());
        drop(r);
        // Caller releases any outer lifecycle gate before dropping old target.
        Ok((b, old))
    }
    pub fn remove(&self, browser: BrowserId) {
        let b = self
            .routes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bindings
            .remove(&browser);
        if let Some(b) = b {
            b.retired.store(true, Ordering::Release);
            drop(b);
        }
    }
    pub fn retire_all(&self) {
        let bs: Vec<_> = self
            .routes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bindings
            .values()
            .cloned()
            .collect();
        for b in bs {
            b.retired.store(true, Ordering::Release);
        }
    }
}
/// A data-only, explicit retirement/status receipt. Drop does not retry a post
/// or masquerade as native drainage; host retires deliberately before UI teardown.
pub struct ContextMenuRegistration {
    pub(crate) binding: Arc<Binding>,
    pub(crate) registry: Weak<UiRegistry>,
}
impl ContextMenuRegistration {
    /// Same registered delivery endpoint AND lease allocation, not numeric
    /// metadata alone. UI adapters use this before attaching an owner.
    pub fn matches_target(&self, target: &ContextMenuTarget) -> Result<bool, EngineError> {
        if self.binding.retired.load(Ordering::Acquire) {
            return Ok(false);
        }
        let stamp = target
            .lease
            .capture()
            .map_err(|_| EngineError::InvalidRequest)?;
        Ok(Arc::ptr_eq(&self.binding.target.delivery, &target.delivery)
            && self
                .binding
                .target
                .lease
                .is_current(&stamp)
                .map_err(|_| EngineError::InvalidRequest)?)
    }
    pub fn status(&self) -> ContextMenuStatus {
        self.binding.snapshot()
    }
    pub fn set_source_viewer(
        &self,
        viewer: Arc<dyn rust_cef_core::source_view::SourceViewSink>,
    ) -> Result<(), EngineError> {
        self.registry
            .upgrade()
            .ok_or(EngineError::Closed)?
            .bind_source_viewer(&self.binding, viewer)
    }
    pub fn set_find_results(
        &self,
        sink: Arc<dyn rust_cef_core::find::FindResultSink>,
    ) -> Result<(), EngineError> {
        self.registry
            .upgrade()
            .ok_or(EngineError::Closed)?
            .bind_find_results(&self.binding, sink)
    }
    pub fn configure_spelling(&self, config: crate::SpellingConfig) -> Result<(), EngineError> {
        self.registry
            .upgrade()
            .ok_or(EngineError::Closed)?
            .bind_spelling(&self.binding, config)
    }
    #[cfg(feature = "test-support")]
    pub fn probe_spelling_context(&self) -> Option<crate::spelling_probe::SpellingContextSnapshot> {
        self.binding.spelling_probe.snapshot()
    }
    pub fn spelling_status(&self) -> crate::SpellingStatus {
        self.binding.spelling.status()
    }
    pub fn retire(&self) -> Result<(), EngineError> {
        self.binding.retired.store(true, Ordering::Release);
        let registry = self.registry.upgrade().ok_or(EngineError::Closed)?;
        registry.post_menu_retirement(self.binding.browser, self.binding.serial)
    }
}
pub(crate) struct Scope {
    pub browser: BrowserId,
    pub native_id: i32,
    pub frame_id: String,
    pub context: Arc<rust_cef_core::menu_context::MenuContext>,
    pub lease: MenuLease,
    pub stamp: MenuLeaseStamp,
    pub registry: Weak<UiRegistry>,
    pub registration: Weak<Binding>,
    pub serial: u64,
    pub epoch: u64,
    pub spelling_opening: u64,
    pub delivery: Weak<dyn ContextMenuDeliverySink>,
}
impl Scope {
    pub fn current(&self) -> bool {
        self.registration.upgrade().is_some_and(|r| {
            !r.retired.load(Ordering::Acquire)
                && r.serial == self.serial
                && r.epoch.load(Ordering::Acquire) == self.epoch
        }) && self.lease.is_current(&self.stamp).unwrap_or(false)
    }
    pub fn admission(&self) -> bool {
        let Some(b) = self.registration.upgrade() else {
            return false;
        };
        if !self.current() {
            return false;
        }
        let Ok(now) = b.target.delivery.capture() else {
            return false;
        };
        self.lease.is_current(&now).unwrap_or(false) && self.current()
    }
    pub fn terminal(&self, id: ContextMenuRequestId) {
        if id.sequence != 0 {
            if let Some(d) = self.delivery.upgrade() {
                if let Err(e) = d.dismiss(id) {
                    self.fail(format!("terminal delivery: {e:?}"));
                }
            }
        }
        if let Some(r) = self.registry.upgrade() {
            if let Err(e) = r.menu_wake() {
                self.fail(format!("terminal wake refused: {e:?}"));
            }
        }
    }
    pub fn fail(&self, reason: impl Into<String>) {
        if let Some(b) = self.registration.upgrade() {
            b.fail(reason);
        }
    }
    pub fn note_cancel(&self) {
        if let Some(b) = self.registration.upgrade() {
            b.note(|s| s.cancelled += 1);
        }
    }
    pub fn note_action(&self) {
        if let Some(b) = self.registration.upgrade() {
            b.note(|s| s.actions_submitted += 1);
        }
    }
}
struct Response {
    request: ContextMenuRequestId,
    scope: Arc<Scope>,
    registry: Weak<UiRegistry>,
}
impl ContextMenuResponseSink for Response {
    fn submit(
        &self,
        id: ContextMenuRequestId,
        response: ContextMenuResponse,
    ) -> Result<(), EngineError> {
        if id != self.request {
            return Err(EngineError::InvalidRequest);
        }
        self.registry
            .upgrade()
            .ok_or(EngineError::Closed)?
            .post_menu_response(id, &self.scope, response)
    }
}

/// Explicit diagnostic for the *whole* model, not a whitelist UI projection.
/// Missing host operations remain required for final12.1; no dead item is shown.
#[cfg(test)]
pub(crate) fn unsupported(model: &MenuModel) -> Option<(i32, bool)> {
    unsupported_for(model, false)
}
#[cfg(test)]
fn unsupported_for(model: &MenuModel, source: bool) -> Option<(i32, bool)> {
    unsupported_with(model, source, false)
}
#[cfg(test)]
fn unsupported_with(model: &MenuModel, source: bool, find: bool) -> Option<(i32, bool)> {
    unsupported_capabilities(model, source, find, false, false, |_| false)
}
/// Which host-managed capabilities to offer as real menu items.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MenuCapabilities {
    pub find: bool,
    pub source: bool,
    pub devtools: bool,
    /// Host-managed structural items (checked command + one-level submenu).
    pub structure: bool,
}

/// Host-managed structural ids. CEF's windowless edit model has neither a
/// checkable item nor a submenu, so 12.2's structural forms can only be reached
/// as host items; they are never CEF menu ids.
pub(crate) const MENU_CHECKED_ID: i32 = 28500;
pub(crate) const MENU_SUBMENU_CHILD_ID: i32 = 28501;

impl MenuCapabilities {
    fn items(self) -> Vec<(i32, &'static str)> {
        // CEF's windowless model carries the edit commands only, so a command
        // this component can actually perform would otherwise never be
        // reachable from a real menu. Same opt-in shape CefSharp uses for its
        // DevTools pair: the host capability decides, never a fixed list.
        let mut items = Vec::new();
        if self.find {
            items.push((130, "Find..."));
        }
        if self.source {
            items.push((132, "View source"));
        }
        if self.devtools {
            items.push((28440, "Show DevTools (Inspect)"));
            items.push((28441, "Close DevTools"));
        }
        items
    }
}

/// Append the opt-in capability items. DevTools keeps CefSharp's exact labels,
/// ids and order; Find/ViewSource use the fixed CEF menu ids this component
/// already routes.
pub(crate) fn append_capability_items(model: &mut MenuModel, caps: MenuCapabilities) {
    // Never duplicate an id the model already offers: a full Chromium model can
    // carry ViewSource/Find itself, and a duplicate would make the whole model
    // invalid (Hierarchy(DuplicateCommandId)) and refuse an otherwise fine menu.
    let mut present = Vec::new();
    for item in &model.entries {
        match item {
            MenuItem::Command(c) => present.push(c.command_id),
            MenuItem::Submenu(g) => present.extend(g.entries.iter().filter_map(|c| match c {
                MenuChild::Command(c) => Some(c.command_id),
                _ => None,
            })),
            _ => {}
        }
    }
    let items: Vec<(i32, &'static str)> = caps
        .items()
        .into_iter()
        .filter(|(id, _)| !present.contains(id))
        .collect();
    // An empty flat list must not skip the structural items below: the structure
    // opt-in can be the only capability the host enabled.
    if !items.is_empty() {
        if !model.entries.is_empty() {
            model.entries.push(MenuItem::Separator { visible: true });
        }
    }
    for (command_id, label) in items {
        model.entries.push(MenuItem::Command(MenuCommand {
            command_id,
            label: label.to_string(),
            enabled: true,
            visible: true,
            checked: None,
            radio_group: None,
        }));
    }
    if caps.structure && !model.entries.is_empty() && !present.contains(&MENU_CHECKED_ID) {
        model.entries.push(MenuItem::Separator { visible: true });
    }
    if caps.structure && !present.contains(&MENU_CHECKED_ID) {
        // A checkable item the host owns, so the checked form is exercised by a
        // real click rather than asserted from a synthetic model.
        model.entries.push(MenuItem::Command(MenuCommand {
            command_id: MENU_CHECKED_ID,
            label: "Show bookmarks bar".to_string(),
            enabled: true,
            visible: true,
            checked: Some(true),
            radio_group: None,
        }));
    }
    if caps.structure && !present.contains(&MENU_SUBMENU_CHILD_ID) {
        model.entries.push(MenuItem::Submenu(MenuSubmenu {
            label: "More".to_string(),
            enabled: true,
            visible: true,
            entries: vec![MenuChild::Command(MenuCommand {
                command_id: MENU_SUBMENU_CHILD_ID,
                label: "Host submenu item".to_string(),
                enabled: true,
                visible: true,
                checked: None,
                radio_group: None,
            })],
        }));
    }
}

pub(crate) fn unsupported_capabilities(
    model: &MenuModel,
    source: bool,
    find: bool,
    devtools: bool,
    structure: bool,
    spelling: impl Fn(i32) -> bool,
) -> Option<(i32, bool)> {
    let command = |id: i32, source: bool| match id {
        100..=104 => None,
        _ if edit_command(id).is_some() => None,
        132 if source => None,
        130 if find => None,
        200..=204 | 206 if spelling(id) => None,
        28440..=28441 if devtools => None,
        MENU_CHECKED_ID | MENU_SUBMENU_CHILD_ID if structure => None,
        130..=132 | 200..=204 | 206 | 28440..=28441 => Some((id, true)),
        _ if id == MENU_CHECKED_ID || id == MENU_SUBMENU_CHILD_ID => Some((id, true)),
        _ => Some((id, false)),
    };
    for item in &model.entries {
        match item {
            MenuItem::Command(c) => {
                if let Some(e) = command(c.command_id, source) {
                    return Some(e);
                }
            }
            MenuItem::Submenu(g) => {
                for c in &g.entries {
                    if let MenuChild::Command(c) = c {
                        if let Some(e) = command(c.command_id, source) {
                            return Some(e);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditCommand {
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    PasteMatchStyle,
    Delete,
    SelectAll,
}
fn edit_command(id: i32) -> Option<EditCommand> {
    use cef::MenuId;
    [
        (MenuId::UNDO, EditCommand::Undo),
        (MenuId::REDO, EditCommand::Redo),
        (MenuId::CUT, EditCommand::Cut),
        (MenuId::COPY, EditCommand::Copy),
        (MenuId::PASTE, EditCommand::Paste),
        (MenuId::PASTE_MATCH_STYLE, EditCommand::PasteMatchStyle),
        (MenuId::DELETE, EditCommand::Delete),
        (MenuId::SELECT_ALL, EditCommand::SelectAll),
    ]
    .into_iter()
    .find_map(|(native, command)| (id == native.get_raw()).then_some(command))
}
fn apply_command(browser: &Browser, frame: &Frame, id: i32) {
    if let Some(command) = edit_command(id) {
        match command {
            EditCommand::Undo => frame.undo(),
            EditCommand::Redo => frame.redo(),
            EditCommand::Cut => frame.cut(),
            EditCommand::Copy => frame.copy(),
            EditCommand::Paste => frame.paste(),
            EditCommand::PasteMatchStyle => frame.paste_and_match_style(),
            EditCommand::Delete => frame.del(),
            EditCommand::SelectAll => frame.select_all(),
        }
        return;
    }
    match id {
        100 => browser.go_back(),
        101 => browser.go_forward(),
        102 => browser.reload(),
        103 => browser.reload_ignore_cache(),
        104 => browser.stop_load(),
        _ => unreachable!("validated menu command"),
    }
}
impl UiRegistry {
    pub(crate) fn perform_menu_reply(
        &self,
        work: crate::menu_native::MenuWork,
        response: ContextMenuResponse,
    ) {
        let Some(scope) = work.scope() else {
            work.cancel(self.menu_audit());
            return;
        };
        if let ContextMenuResponse::Select { command_id } = response {
            let permit = if scope.admission() {
                scope.lease.try_enter(&scope.stamp).ok()
            } else {
                None
            };
            let pair = permit
                .as_ref()
                .and_then(|_| self.menu_original_frame(&scope));
            if let (Some(permit), Some((browser, frame))) = (permit, pair) {
                let menu = work.request();
                work.finish(self.menu_audit(), || {
                    // Cancel/last Release can synchronously reenter. Recheck the
                    // original native objects and cutpoint again, never focused.
                    if scope.current_native_epoch()
                        && self.menu_original_frame_valid(&scope, &browser, &frame)
                    {
                        if command_id == 130 {
                            match self.start_find(scope.clone(), menu, &browser, &frame) {
                                Ok(_) => scope.note_action(),
                                Err(e) => scope.fail(format!("find operation start: {e:?}")),
                            }
                        } else if matches!(command_id, 200..=204 | 206) {
                            self.perform_spelling(&scope, menu, &browser, &frame, command_id);
                        } else if matches!(command_id, 28440 | 28441) {
                            // Same commands CefSharp routes in its menu handler.
                            let posted = match scope.registry.upgrade() {
                                Some(registry) if command_id == 28440 => {
                                    registry.open_devtools(scope.browser)
                                }
                                Some(registry) => registry.close_devtools(scope.browser),
                                None => Err(EngineError::Closed),
                            };
                            match posted {
                                Ok(()) => scope.note_action(),
                                Err(e) => {
                                    scope.fail(format!("devtools command {command_id}: {e:?}"))
                                }
                            }
                        } else if command_id == 132 {
                            match self.start_source(scope.clone(), menu, &browser, &frame) {
                                Ok(_) => scope.note_action(),
                                Err(e) => scope.fail(format!("source operation start: {e:?}")),
                            }
                        } else if command_id == MENU_CHECKED_ID
                            || command_id == MENU_SUBMENU_CHILD_ID
                        {
                            // Host-managed structural item: there is no CEF
                            // command behind it, so it is recorded, never
                            // forwarded as an unknown edit command.
                            if let Some(registry) = scope.registry.upgrade() {
                                registry.note_host_menu_item(command_id);
                            }
                            scope.note_action();
                        } else {
                            self.menu_audit().record();
                            apply_command(&browser, &frame, command_id);
                            scope.note_action();
                        }
                    } else {
                        scope.fail("original menu frame changed during terminal cancellation");
                    }
                    drop(permit);
                });
                return;
            }
            scope.fail("stale menu selection cancelled before native execution");
        }
        work.cancel(self.menu_audit());
    }
}
impl Scope {
    pub(crate) fn operation_alive(&self) -> bool {
        self.current_native_epoch()
            && self
                .lease
                .snapshot()
                .is_ok_and(|s| !s.retired && !s.faulted)
    }
    pub(crate) fn current_native_epoch(&self) -> bool {
        self.registration.upgrade().is_some_and(|r| {
            !r.retired.load(Ordering::Acquire)
                && r.epoch.load(Ordering::Acquire) == self.epoch
                && r.serial == self.serial
        })
    }
}
#[cfg(feature = "test-support")]
fn prepare_fixture(model: &cef::MenuModel, kind: u8, audit: &crate::executor::NativeAudit) -> bool {
    audit.record();
    if model.clear() == 0 {
        return false;
    }
    let (id, label) = if kind == 1 {
        (cef::MenuId::SELECT_ALL.get_raw(), "Select owned frame")
    } else if kind == 5 {
        (200, "Replace original spelling suggestion")
    } else if kind == 6 {
        (206, "Required dictionary write disabled")
    } else if kind == 4 {
        (130, "Find original selection")
    } else if kind == 3 {
        (132, "View original source")
    } else {
        (131, "Required host Print missing")
    };
    let label = CefString::from(label);
    audit.record();
    model.add_item(id, Some(&label)) != 0
}
#[cfg(test)]
mod tests;
wrap_context_menu_handler! {
    pub(crate) struct ProductionMenuHandler {state:Arc<BrowserClientState>,binding:Arc<Binding>}
    impl ContextMenuHandler {
        fn on_before_context_menu(&self,_browser:Option<&mut Browser>,_frame:Option<&mut Frame>,_params:Option<&mut ContextMenuParams>,model:Option<&mut cef::MenuModel>){
            self.binding.spelling.advance();
            #[cfg(feature="test-support")]
            let spelling_opening=self.binding.spelling.opening();
            #[cfg(feature="test-support")]
            if let Some(registry)=self.state.registry.upgrade(){
                let kind=self.binding.fixture.load(Ordering::Acquire);
                if matches!(kind,5|6)&&currently_on(ThreadId::UI)!=0{if let Some(browser)=_browser.as_deref().filter(|b|registry.menu_native_matches(self.state.browser,b)){let snapshot=crate::spelling_probe::capture_before(spelling_opening,browser,_params.as_deref(),model.as_deref(),registry.menu_audit());self.binding.spelling_probe.begin(snapshot);}}
                if kind!=0{if let Some(model)=model{if !prepare_fixture(model,kind,registry.menu_audit()){self.binding.fail("bridge native fixture model construction failed");}}else{self.binding.fail("bridge fixture model missing");}}
            }
        }
        fn run_context_menu(&self,browser:Option<&mut Browser>,frame:Option<&mut Frame>,params:Option<&mut ContextMenuParams>,model:Option<&mut cef::MenuModel>,callback:Option<&mut RunContextMenuCallback>)->i32{
            let spelling_opening=self.binding.spelling.opening();
            let Some(registry)=self.state.registry.upgrade() else{return 0};
            let binding=self.binding.clone();
            if currently_on(ThreadId::UI)==0{binding.fail("menu capture wrong CEF thread");registry.menus.fault();return 1;}
            let _callback_guard=registry.menus.callback_guard();
            let Some(callback)=callback else{binding.fail("missing native menu continuation");registry.menus.fault();return 1};
            let result=(||{
                let (Some(browser),Some(frame),Some(params),Some(model))=(browser,frame,params,model) else{return Err("missing original native menu context".into())};
                if !registry.menu_native_matches(self.state.browser,browser){return Err("foreign/native menu Browser mapping".into());}
                if binding.retired.load(Ordering::Acquire){return Err("retired menu target".into());}
                let stamp=binding.target.delivery.capture().map_err(|e|format!("projected menu target: {e:?}"))?;
                if !binding.target.lease.is_current(&stamp).map_err(|e|format!("menu lease: {e:?}"))?{return Err("foreign/stale menu target stamp".into());}
                let epoch=binding.epoch.load(Ordering::Acquire);
                let config=registry.menu_bridge.config().ok_or("menu config missing")?;
                let (mut owned,context)=crate::menu_context::copy_cef_request(crate::menu_context::CaptureScope {browser:self.state.browser,binding:stamp.binding(),revision:stamp.revision()},browser,frame,params,model,config.model_limits,config.context_limits).map_err(|e|format!("native menu capture: {e:?}"))?;
                #[cfg(feature="test-support")]
                 if matches!(binding.fixture.load(Ordering::Acquire),5|6){binding.spelling_probe.copied(spelling_opening,&context);}
                 let devtools_configured=registry.devtools.is_configured();
                let menu_caps=crate::menu_bridge::MenuCapabilities{
                    find:binding.find.get().is_some()&&registry.finds.configured(),
                    source:binding.source.get().is_some()&&registry.sources.configured(),
                    devtools:devtools_configured,
                    structure:registry.menu_structure_enabled()};
                crate::menu_bridge::append_capability_items(&mut owned,menu_caps);
                if let Some((id,managed))=unsupported_capabilities(&owned,binding.source.get().is_some()&&registry.sources.configured(),binding.find.get().is_some()&&registry.finds.configured(),devtools_configured,registry.menu_structure_enabled(),|id|binding.spelling.configured(id)){return Err(format!("{} menu command {id}",if managed{"missing required host capability for"}else{"unsupported"}));}
                binding.spelling.validate_model(&context,&owned).map_err(|e|format!("spelling payload rejected: {e:?}"))?;
                 let scope=Arc::new(Scope {browser:self.state.browser,native_id:browser.identifier(),frame_id:context.frame_id.clone(),context:Arc::new(context.clone()),lease:binding.target.lease.clone(),stamp,registry:Arc::downgrade(&registry),registration:Arc::downgrade(&binding),serial:binding.serial,epoch,spelling_opening,delivery:Arc::downgrade(&binding.target.delivery)});
                if !scope.current(){return Err("menu scope changed during capture".into());}
                binding.note(|s|s.trace.push("capture_bound".into()));
                let payload=crate::menu_native::RetainedMenu::capture_bound(&registry.menus,callback,scope.clone());
                let (presentation,displaced)=match registry.menus.open(owned,context,config.context_limits,payload){Ok(v)=>v,Err((e,work))=>{binding.fail(format!("menu opening refused: {e:?}"));work.cancel(registry.menu_audit());return Ok(())}};
                binding.note(|s|s.opened+=1);
                binding.note(|s|{s.trace.push("opened".into());s.trace.push(format!("displaced={}",displaced.is_some()));});
                if let Some(work)=displaced{work.cancel(registry.menu_audit());}
                let request=presentation.request;
                if !scope.current()||!registry.menus.is_pending(request){binding.note(|s|{s.cancel_unpending+=1;s.trace.push("NOT_PENDING".into());});if let Some(work)=registry.menus.take_id(request){work.cancel(registry.menu_audit());}return Ok(());}
                let response=Arc::new(Response {request,scope:scope.clone(),registry:Arc::downgrade(&registry)});
                if let Err(e)=binding.target.delivery.publish(presentation,response,scope.stamp.clone()){
                    binding.fail(format!("menu delivery rejected: {e:?}"));
                    if let Some(work)=registry.menus.take_id(request){work.cancel(registry.menu_audit());}
                }else{binding.note(|s|{s.published+=1;s.trace.push("published".into());});if let Err(e)=registry.menu_wake(){binding.note(|s|s.cancel_wake_refused+=1);binding.fail(format!("menu publication wake refused: {e:?}"));if let Some(work)=registry.menus.take_id(request){work.cancel(registry.menu_audit());}}}
                Ok::<(),String>(())
            })();
            if let Err(e)=result{binding.fail(e);registry.menu_audit().record();callback.cancel();binding.note(|s|s.cancelled+=1);if let Err(e)=registry.menu_wake(){binding.fail(format!("menu rejection wake refused: {e:?}"));}}
            1
        }
        /// Windowless browsers request the quick menu; the base implementation
        /// returns false and CEF cancels the menu at once. Answering here retains
        /// the continuation so the real presenter can own the menu.
        fn run_quick_menu(&self,_browser:Option<&mut Browser>,_frame:Option<&mut Frame>,_location:Option<&Point>,_size:Option<&Size>,_edit_state_flags:QuickMenuEditStateFlags,callback:Option<&mut RunQuickMenuCallback>)->i32{
            let Some(registry)=self.state.registry.upgrade() else{return 0};
            if currently_on(ThreadId::UI)==0{self.binding.fail("quick menu capture wrong CEF thread");registry.menus.fault();return 1;}
            let Some(callback)=callback else{self.binding.fail("missing quick menu continuation");registry.menus.fault();return 1;};
            if !registry.menus.retain_quick(callback){
                self.binding.fail("quick menu continuation refused");
                registry.menus.fault();
                return 1;
            }
            self.binding.note(|s|s.opened+=1);
            1
        }
        fn on_context_menu_dismissed(&self,_browser:Option<&mut Browser>,_frame:Option<&mut Frame>){
            if let Some(registry)=self.state.registry.upgrade(){let _guard=registry.menus.callback_guard();self.binding.note(|s|s.native_dismissed+=1);}
            // No native opening ID: observation only, never take current.
        }
    }
}
