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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TargetMenuContext {
    pub link_url: Option<String>,
    pub source_url: Option<String>,
    pub is_image: bool,
    pub has_image_contents: bool,
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
    pub target: TargetMenuContext,
    pub synthesized_target: bool,
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
    /// Host-managed standard page items on non-editable/blank OSR areas.
    pub navigation: bool,
    /// Host DownloadService wired; unlocks `Save link/image/audio/video as...` (`IDC_CONTENT_CONTEXT_SAVE*`).
    pub download: bool,
    /// Host PopupService wired; unlocks synthesized Alloy `Open link` / `Open image` (`50100` / `50123`).
    pub popup: bool,
}

pub(crate) fn is_chromium_native_context_command(id: i32, download_enabled: bool) -> bool {
    match id {
        // Links: Open link in new tab, Copy link address/text, Copy link to highlight
        50100 | 50104 | 50107 | 50158 => true,
        // Images: Copy image address/image, Open image in new tab/original, Load image
        50121..=50123 | 50125..=50126 => true,
        // Audio/Video: Copy location, Copy video frame, Open in new tab, PiP, Loop, Controls, Rotate
        50132..=50133 | 50136..=50137 | 50140..=50143 => true,
        // Edit/Frame: IDC_CONTENT_CONTEXT_COPY..PASTE_AND_MATCH_STYLE, RELOADFRAME
        50150..=50157 | 50180 => true,
        // Save Link As / Save Image As / Save Audio/Video As / Save Video Frame As
        50103 | 50120 | 50130 | 50131 => download_enabled,
        _ => false,
    }
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

fn model_command_ids(model: &MenuModel) -> Vec<i32> {
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
    present
}

fn plain_command(command_id: i32, label: &str, enabled: bool) -> MenuItem {
    MenuItem::Command(MenuCommand {
        command_id,
        label: label.to_string(),
        enabled,
        visible: true,
        checked: None,
        radio_group: None,
    })
}

fn command_supported(id: i32, caps: MenuCapabilities, spelling: &impl Fn(i32) -> bool) -> bool {
    match id {
        100..=104 => true,
        _ if edit_command(id).is_some() => true,
        130 => caps.find,
        132 => caps.source,
        200..=204 | 206 => spelling(id),
        28440..=28441 => caps.devtools,
        MENU_CHECKED_ID | MENU_SUBMENU_CHILD_ID => caps.structure,
        _ if caps.navigation && is_chromium_native_context_command(id, caps.download) => true,
        _ => false,
    }
}

fn normalize_separators(entries: &mut Vec<MenuItem>) {
    let mut normalized = Vec::with_capacity(entries.len());
    for item in entries.drain(..) {
        let is_sep = matches!(item, MenuItem::Separator { .. });
        if is_sep
            && (normalized.is_empty()
                || matches!(normalized.last(), Some(MenuItem::Separator { .. })))
        {
            continue;
        }
        normalized.push(item);
    }
    while matches!(normalized.last(), Some(MenuItem::Separator { .. })) {
        normalized.pop();
    }
    *entries = normalized;
}

fn normalize_child_separators(entries: &mut Vec<MenuChild>) {
    let mut normalized = Vec::with_capacity(entries.len());
    for item in entries.drain(..) {
        let is_sep = matches!(item, MenuChild::Separator { .. });
        if is_sep
            && (normalized.is_empty()
                || matches!(normalized.last(), Some(MenuChild::Separator { .. })))
        {
            continue;
        }
        normalized.push(item);
    }
    while matches!(normalized.last(), Some(MenuChild::Separator { .. })) {
        normalized.pop();
    }
    *entries = normalized;
}

/// Strip unconfigured host capabilities (such as physical print `131`,
/// `NO_SPELLING_SUGGESTIONS (205)`, or unsupported Chromium `IDC_*` commands) from
/// raw CEF models when `menu_navigation` is enabled, keeping separator hygiene valid.
pub(crate) fn filter_supported_items(
    model: &mut MenuModel,
    caps: MenuCapabilities,
    spelling: impl Fn(i32) -> bool,
) {
    let entries = std::mem::take(&mut model.entries);
    for item in entries {
        match item {
            MenuItem::Command(c) if command_supported(c.command_id, caps, &spelling) => {
                model.entries.push(MenuItem::Command(c));
            }
            MenuItem::Command(_) => {}
            MenuItem::Submenu(mut sub) => {
                sub.entries.retain(|child| match child {
                    MenuChild::Command(c) => command_supported(c.command_id, caps, &spelling),
                    MenuChild::Separator { .. } => true,
                });
                normalize_child_separators(&mut sub.entries);
                let has_cmd = sub
                    .entries
                    .iter()
                    .any(|c| matches!(c, MenuChild::Command(_)));
                if has_cmd {
                    model.entries.push(MenuItem::Submenu(sub));
                }
            }
            sep @ MenuItem::Separator { .. } => model.entries.push(sep),
        }
    }
    normalize_separators(&mut model.entries);
}

/// Synthesize link and image target menu items for Alloy-style windowless OSR
/// browsers where CEF's `CefMenuCreator` omits `50100..=50143` from `MenuModel`
/// even when `ContextMenuParams` carries `link_url` / `source_url` / `CM_MEDIATYPE_IMAGE`.
pub(crate) fn prepend_target_items(
    model: &mut MenuModel,
    target: &TargetMenuContext,
    caps: MenuCapabilities,
) -> bool {
    let has_target_commands = model.entries.iter().any(|item| {
        matches!(
            item,
            MenuItem::Command(c) if matches!(c.command_id, 50100..=50143 | 50158)
        )
    });
    if has_target_commands {
        return false;
    }
    let link_url = target
        .link_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let image_url = target
        .is_image
        .then(|| target.source_url.as_deref().map(str::trim))
        .flatten()
        .filter(|s| !s.is_empty());
    let mut target_entries = Vec::new();
    if let Some(url) = link_url {
        if caps.popup {
            target_entries.push(plain_command(50100, "Open link", true));
        }
        if caps.download && (url.starts_with("http://") || url.starts_with("https://")) {
            target_entries.push(plain_command(50103, "Save link as...", true));
        }
        target_entries.push(plain_command(50104, "Copy link address", true));
    }
    if image_url.is_some() {
        if !target_entries.is_empty() {
            target_entries.push(MenuItem::Separator { visible: true });
        }
        if caps.popup {
            target_entries.push(plain_command(50123, "Open image", true));
        }
        if caps.download {
            target_entries.push(plain_command(50120, "Save image as...", true));
        }
        if target.has_image_contents {
            target_entries.push(plain_command(50122, "Copy image", true));
        }
        target_entries.push(plain_command(50121, "Copy image address", true));
    }
    if target_entries.is_empty() {
        return false;
    }
    model.entries.retain(|item| {
        !matches!(
            item,
            MenuItem::Command(c) if matches!(c.command_id, 100..=104)
        )
    });
    if !model.entries.is_empty() {
        target_entries.push(MenuItem::Separator { visible: true });
    }
    model.entries.splice(0..0, target_entries);
    normalize_separators(&mut model.entries);
    true
}

/// Prepend standard browser navigation & page selection commands when right-clicking
/// on a non-editable area without edit commands (`110..=116`).
pub(crate) fn prepend_navigation_items(
    model: &mut MenuModel,
    is_editable: bool,
    can_go_back: bool,
    can_go_forward: bool,
    has_selection: bool,
) {
    let has_edit_commands = model.entries.iter().any(|item| {
        matches!(
            item,
            MenuItem::Command(c)
                if matches!(c.command_id, 110 | 111 | 112 | 114 | 115 | 116 | 50151..=50155 | 50157)
        )
    });
    if is_editable || has_edit_commands {
        return;
    }
    let has_target_commands = model.entries.iter().any(|item| {
        matches!(
            item,
            MenuItem::Command(c) if matches!(c.command_id, 50100..=50143 | 50158)
        )
    });
    if has_target_commands {
        return;
    }
    let present = model_command_ids(model);
    let mut missing_nav = Vec::new();
    if !present.contains(&100) {
        missing_nav.push(plain_command(100, "Back", can_go_back));
    }
    if !present.contains(&101) {
        missing_nav.push(plain_command(101, "Forward", can_go_forward));
    }
    if !present.contains(&102) {
        missing_nav.push(plain_command(102, "Reload", true));
    }
    if !missing_nav.is_empty() {
        if !model.entries.is_empty() {
            missing_nav.push(MenuItem::Separator { visible: true });
        }
        model.entries.splice(0..0, missing_nav);
    }

    let present = model_command_ids(model);
    let add_copy = has_selection && !present.contains(&113) && !present.contains(&50150);
    let add_select_all = !present.contains(&117) && !present.contains(&50156);
    if add_copy || add_select_all {
        let insert_at = model
            .entries
            .iter()
            .position(|item| match item {
                MenuItem::Command(c) => !matches!(c.command_id, 100..=104 | 110..=117),
                MenuItem::Submenu(_) => true,
                MenuItem::Separator { .. } => false,
            })
            .unwrap_or(model.entries.len());
        let mut selection_group = vec![MenuItem::Separator { visible: true }];
        if add_copy {
            selection_group.push(plain_command(113, "Copy", true));
        }
        if add_select_all {
            selection_group.push(plain_command(117, "Select All", true));
        }
        selection_group.push(MenuItem::Separator { visible: true });
        model.entries.splice(insert_at..insert_at, selection_group);
    }
    normalize_separators(&mut model.entries);
}

/// Append the opt-in capability items. DevTools keeps CefSharp's exact labels,
/// ids and order; Find/ViewSource use the fixed CEF menu ids this component
/// already routes.
pub(crate) fn append_capability_items(model: &mut MenuModel, caps: MenuCapabilities) {
    // Never duplicate an id the model already offers: a full Chromium model can
    // carry ViewSource/Find itself, and a duplicate would make the whole model
    // invalid (Hierarchy(DuplicateCommandId)) and refuse an otherwise fine menu.
    let present = model_command_ids(model);
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

#[cfg(test)]
pub(crate) fn unsupported_capabilities(
    model: &MenuModel,
    source: bool,
    find: bool,
    devtools: bool,
    structure: bool,
    spelling: impl Fn(i32) -> bool,
) -> Option<(i32, bool)> {
    unsupported_capabilities_with_caps(
        model,
        MenuCapabilities {
            find,
            source,
            devtools,
            structure,
            navigation: false,
            download: false,
            popup: false,
        },
        spelling,
    )
}

pub(crate) fn unsupported_capabilities_with_caps(
    model: &MenuModel,
    caps: MenuCapabilities,
    spelling: impl Fn(i32) -> bool,
) -> Option<(i32, bool)> {
    let command = |id: i32| match id {
        100..=104 => None,
        _ if edit_command(id).is_some() => None,
        132 if caps.source => None,
        130 if caps.find => None,
        200..=204 | 206 if spelling(id) => None,
        28440..=28441 if caps.devtools => None,
        MENU_CHECKED_ID | MENU_SUBMENU_CHILD_ID if caps.structure => None,
        _ if caps.navigation && is_chromium_native_context_command(id, caps.download) => None,
        130..=132 | 200..=204 | 206 | 28440..=28441 => Some((id, true)),
        _ if id == MENU_CHECKED_ID || id == MENU_SUBMENU_CHILD_ID => Some((id, true)),
        _ => Some((id, false)),
    };
    for item in &model.entries {
        match item {
            MenuItem::Command(c) => {
                if let Some(e) = command(c.command_id) {
                    return Some(e);
                }
            }
            MenuItem::Submenu(g) => {
                for c in &g.entries {
                    if let MenuChild::Command(c) = c {
                        if let Some(e) = command(c.command_id) {
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
mod win32_clipboard {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::ptr;

    const GMEM_MOVEABLE: u32 = 0x0002;
    const CF_DIB: u32 = 8;
    const CF_UNICODETEXT: u32 = 13;
    const BI_RGB: u32 = 0;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct BitmapInfoHeader {
        bi_size: u32,
        bi_width: i32,
        bi_height: i32,
        bi_planes: u16,
        bi_bit_count: u16,
        bi_compression: u32,
        bi_size_image: u32,
        bi_x_pels_per_meter: i32,
        bi_y_pels_per_meter: i32,
        bi_clr_used: u32,
        bi_clr_important: u32,
    }

    const _: () = assert!(size_of::<BitmapInfoHeader>() == 40);

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
        fn GlobalLock(mem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(mem: *mut c_void) -> i32;
        fn GlobalFree(mem: *mut c_void) -> *mut c_void;
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn OpenClipboard(hwnd_new_owner: *mut c_void) -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(format: u32, mem: *mut c_void) -> *mut c_void;
        fn CloseClipboard() -> i32;
    }

    fn commit_clipboard_hglobal(format: u32, write: impl FnOnce(*mut u8), byte_len: usize) -> bool {
        if byte_len == 0 {
            return false;
        }
        let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_len) };
        if hglobal.is_null() {
            return false;
        }
        let locked = unsafe { GlobalLock(hglobal) };
        if locked.is_null() {
            unsafe { GlobalFree(hglobal) };
            return false;
        }
        write(locked.cast::<u8>());
        unsafe { GlobalUnlock(hglobal) };
        if unsafe { OpenClipboard(ptr::null_mut()) } == 0 {
            unsafe { GlobalFree(hglobal) };
            return false;
        }
        let committed = unsafe { EmptyClipboard() } != 0
            && !unsafe { SetClipboardData(format, hglobal) }.is_null();
        unsafe { CloseClipboard() };
        if !committed {
            unsafe { GlobalFree(hglobal) };
        }
        committed
    }

    pub(super) fn copy_unicode_text(text: &str) -> bool {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let Some(byte_len) = wide.len().checked_mul(size_of::<u16>()) else {
            return false;
        };
        commit_clipboard_hglobal(
            CF_UNICODETEXT,
            |dst| unsafe {
                ptr::copy_nonoverlapping(wide.as_ptr().cast::<u8>(), dst, byte_len);
            },
            byte_len,
        )
    }

    pub(super) fn copy_bgra_image(width: u32, height: u32, bgra_top_down: &[u8]) -> bool {
        let Some(stride) = (width as usize).checked_mul(4) else {
            return false;
        };
        let Some(pixel_bytes) = stride.checked_mul(height as usize) else {
            return false;
        };
        if pixel_bytes == 0 || bgra_top_down.len() != pixel_bytes {
            return false;
        }
        let Some(total_bytes) = size_of::<BitmapInfoHeader>().checked_add(pixel_bytes) else {
            return false;
        };
        let header = BitmapInfoHeader {
            bi_size: size_of::<BitmapInfoHeader>() as u32,
            bi_width: width as i32,
            bi_height: height as i32,
            bi_planes: 1,
            bi_bit_count: 32,
            bi_compression: BI_RGB,
            bi_size_image: u32::try_from(pixel_bytes).unwrap_or(0),
            ..BitmapInfoHeader::default()
        };
        commit_clipboard_hglobal(
            CF_DIB,
            |dst| unsafe {
                ptr::copy_nonoverlapping(
                    (&raw const header).cast::<u8>(),
                    dst,
                    size_of::<BitmapInfoHeader>(),
                );
                let pixel_dst = dst.add(size_of::<BitmapInfoHeader>());
                for y in 0..height as usize {
                    let src_row = bgra_top_down
                        .as_ptr()
                        .add((height as usize - 1 - y) * stride);
                    let dst_row = pixel_dst.add(y * stride);
                    ptr::copy_nonoverlapping(src_row, dst_row, stride);
                }
            },
            total_bytes,
        )
    }
}

wrap_download_image_callback! {
    struct CopyImageDownloadCallback;
    impl DownloadImageCallback {
        fn on_download_image_finished(
            &self,
            _image_url: Option<&CefString>,
            _http_status_code: ::std::os::raw::c_int,
            image: Option<&mut Image>,
        ) {
            let Some(image) = image else {
                return;
            };
            if image.is_empty() != 0 {
                return;
            }
            let mut pixel_width = 0i32;
            let mut pixel_height = 0i32;
            let Some(binary) = image.as_bitmap(
                1.0,
                ColorType::BGRA_8888,
                AlphaType::POSTMULTIPLIED,
                Some(&mut pixel_width),
                Some(&mut pixel_height),
            ) else {
                return;
            };
            if pixel_width <= 0 || pixel_height <= 0 {
                return;
            }
            let (width, height) = (pixel_width as u32, pixel_height as u32);
            let Some(expected) = (width as usize)
                .checked_mul(height as usize)
                .and_then(|n| n.checked_mul(4))
            else {
                return;
            };
            if expected == 0
                || expected > rust_cef_core::outbound_drag::MAX_DRAG_IMAGE_BYTES
                || binary.size() != expected
            {
                return;
            }
            let mut bgra = vec![0u8; expected];
            if binary.data(Some(&mut bgra), 0) != expected {
                return;
            }
            let _ = win32_clipboard::copy_bgra_image(width, height, &bgra);
        }
    }
}

impl UiRegistry {
    fn perform_synthesized_target_command(
        &self,
        scope: &Scope,
        browser: &Browser,
        command_id: i32,
    ) {
        let client_state = self.browser_client_state(scope.browser);
        let url_allowed = |url: &str| {
            url.starts_with("data:")
                || client_state
                    .as_ref()
                    .is_some_and(|state| state.allows_resource(url, false))
        };
        match command_id {
            50100 | 50123 => {
                let target_url = if command_id == 50100 {
                    scope.target.link_url.as_deref()
                } else {
                    scope.target.source_url.as_deref()
                };
                match (target_url, client_state.as_ref()) {
                    (Some(url), Some(state)) => {
                        let url_cef = CefString::from(url);
                        state.handle_popup_request(
                            Some(&url_cef),
                            None,
                            WindowOpenDisposition::NEW_FOREGROUND_TAB,
                            1,
                        );
                        scope.note_action();
                    }
                    _ => scope.fail(format!("synthesized menu command {command_id} unavailable")),
                }
            }
            50104 | 50121 => {
                let text = if command_id == 50104 {
                    scope.target.link_url.as_deref()
                } else {
                    scope.target.source_url.as_deref()
                };
                if text.is_some_and(win32_clipboard::copy_unicode_text) {
                    scope.note_action();
                } else {
                    scope.fail(format!(
                        "clipboard copy failed for menu command {command_id}"
                    ));
                }
            }
            50103 | 50120 => {
                let url = if command_id == 50103 {
                    scope.target.link_url.as_deref()
                } else {
                    scope.target.source_url.as_deref()
                };
                if let (Some(url), Some(host)) = (url.filter(|u| url_allowed(u)), browser.host()) {
                    self.menu_audit().record();
                    host.start_download(Some(&CefString::from(url)));
                    scope.note_action();
                } else {
                    scope.fail(format!("download disallowed for menu command {command_id}"));
                }
            }
            50122 => {
                if let (Some(url), Some(host)) = (
                    scope
                        .target
                        .source_url
                        .as_deref()
                        .filter(|u| u.starts_with("data:image/") || url_allowed(u)),
                    browser.host(),
                ) {
                    self.menu_audit().record();
                    let mut callback = CopyImageDownloadCallback::new();
                    host.download_image(Some(&CefString::from(url)), 0, 0, 0, Some(&mut callback));
                    scope.note_action();
                } else {
                    scope.fail("copy image disallowed or image source unavailable");
                }
            }
            _ => scope.fail(format!("unhandled synthesized menu command {command_id}")),
        }
    }

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
                if is_chromium_native_context_command(command_id, true) && !scope.synthesized_target
                {
                    work.continue_native(self.menu_audit(), command_id, || {
                        let valid = scope.current_native_epoch()
                            && self.menu_original_frame_valid(&scope, &browser, &frame);
                        if !valid {
                            scope.fail("original menu frame changed during native execution");
                        }
                        drop(permit);
                        valid
                    });
                    return;
                }
                work.finish(self.menu_audit(), || {
                    // Cancel/last Release can synchronously reenter. Recheck the
                    // original native objects and cutpoint again, never focused.
                    if scope.current_native_epoch()
                        && self.menu_original_frame_valid(&scope, &browser, &frame)
                    {
                        if is_chromium_native_context_command(command_id, true) {
                            self.perform_synthesized_target_command(&scope, &browser, command_id);
                        } else if command_id == 130 {
                            match self.start_find(scope.clone(), menu, &browser, &frame) {
                                Ok(_) => scope.note_action(),
                                Err(e) => scope.fail(format!("find operation start: {e:?}")),
                            }
                        } else if matches!(command_id, 200..=204 | 206) {
                            self.perform_spelling(&scope, menu, &browser, &frame, command_id);
                        } else if matches!(command_id, 28440 | 28441) {
                            // Same commands CefSharp routes in its menu handler.
                            let posted = match scope.registry.upgrade() {
                                Some(registry) if command_id == 28440 => registry.open_devtools_at(
                                    scope.browser,
                                    Some((scope.context.x, scope.context.y)),
                                ),
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
                 let host_caps=registry.host_capabilities();
                 let download_configured=host_caps.download.is_some();
                 let popup_configured=host_caps.popup.is_some();
                 let target=TargetMenuContext {
                     link_url: crate::menu_model::read_userfree(&params.link_url(), 4096).ok().filter(|s|!s.is_empty()),
                     source_url: crate::menu_model::read_userfree(&params.source_url(), 4096).ok().filter(|s|!s.is_empty()),
                     is_image: params.media_type().get_raw() == ContextMenuMediaType::IMAGE.get_raw(),
                     has_image_contents: params.has_image_contents() != 0,
                 };
                let menu_caps=crate::menu_bridge::MenuCapabilities{
                    find:binding.find.get().is_some()&&registry.finds.configured(),
                    source:binding.source.get().is_some()&&registry.sources.configured(),
                    devtools:devtools_configured,
                    structure:registry.menu_structure_enabled(),
                    navigation:registry.menu_navigation_enabled(),
                    download:download_configured,
                    popup:popup_configured};
                let synthesized_target=if menu_caps.navigation{
                    crate::menu_bridge::filter_supported_items(&mut owned,menu_caps,|id|binding.spelling.configured(id));
                    let syn=crate::menu_bridge::prepend_target_items(&mut owned,&target,menu_caps);
                    crate::menu_bridge::prepend_navigation_items(&mut owned,params.is_editable()!=0,browser.can_go_back()!=0,browser.can_go_forward()!=0,!context.selection.is_empty());
                    syn
                }else{false};
                crate::menu_bridge::append_capability_items(&mut owned,menu_caps);
                if let Some((id,managed))=unsupported_capabilities_with_caps(&owned,menu_caps,|id|binding.spelling.configured(id)){return Err(format!("{} menu command {id}",if managed{"missing required host capability for"}else{"unsupported"}));}
                binding.spelling.validate_model(&context,&owned).map_err(|e|format!("spelling payload rejected: {e:?}"))?;
                 let scope=Arc::new(Scope {browser:self.state.browser,native_id:browser.identifier(),frame_id:context.frame_id.clone(),context:Arc::new(context.clone()),lease:binding.target.lease.clone(),stamp,registry:Arc::downgrade(&registry),registration:Arc::downgrade(&binding),serial:binding.serial,epoch,spelling_opening,delivery:Arc::downgrade(&binding.target.delivery),target,synthesized_target});
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
