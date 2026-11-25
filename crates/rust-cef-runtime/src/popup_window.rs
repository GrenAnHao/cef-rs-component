//! Managed native child popup `HWND` lifecycle and CEF `PopupClient`.
//!
//! Reference CefSharp anchors (`D:\System Dir\Downloads\CefSharp`, rev `41919703`):
//! - `CefSharp.Core.Runtime/Internals/ClientAdapter.cpp:136-149`: `CloseAllPopups`
//! - `CefSharp.Core.Runtime/Internals/ClientAdapter.cpp:151-214`: `OnBeforePopup`
//! - `CefSharp.Core.Runtime/Internals/ClientAdapter.cpp:216-246, 265-293`: `_popupBrowsers` track/remove
//! - `CefSharp.Core.Runtime/Internals/ClientAdapter.cpp:386-400`: `OnTitleChange` -> `SetWindowText` for popup `HWND`
//! - `CefSharp.Core.Runtime/Internals/CefBrowserHostWrapper.cpp:172`: `SetAsPopup(NULL, ...)`
//!
//! Unlike raw unmanaged Win32 popups, every popup `Browser` and `PopupClient` created here is
//! tracked by [`PopupLedger`], subject to [`NativePolicy`] (`TrustedContentPolicy`), automatically
//! closed when the parent browser closes or the `Runtime` begins draining, and fenced before
//! `cef::shutdown`.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
    thread::ThreadId as StdThreadId,
};

use cef::*;
use rust_cef_core::{
    contracts::{BrowserId, MAX_NATIVE_BOUND, MAX_NATIVE_COORD},
    ports::{EngineError, PolicyAction},
};

use crate::{policy_native::NativePolicy, registry::UiRegistry};

pub(crate) const MAX_CHILD_POPUPS_PER_BROWSER: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PopupSlotId(pub(crate) u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PopupRow {
    slot: PopupSlotId,
    owner: BrowserId,
    popup_id: i32,
    created_native_id: Option<i32>,
    aborted: bool,
    close_requested: bool,
    close_posted: bool,
    native_released: bool,
    client_released: bool,
}

impl PopupRow {
    fn ready_to_retire(&self) -> bool {
        self.client_released && (self.native_released || self.created_native_id.is_none())
    }
}

struct PopupLedgerState {
    next_slot: u64,
    draining: bool,
    rows: BTreeMap<PopupSlotId, PopupRow>,
}

/// Pure-metadata + isolated-native store for runtime-managed native popup `HWND` browsers.
///
/// Metadata and adopted `cef::Browser` wrappers sit behind separate locks, and native CEF wrapper
/// clones/drops ALWAYS occur after releasing every lock and strictly on the CEF UI thread.
pub(crate) struct PopupLedger {
    state: Mutex<PopupLedgerState>,
    natives: Mutex<BTreeMap<PopupSlotId, Browser>>,
}

impl PopupLedger {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PopupLedgerState {
                next_slot: 1,
                draining: false,
                rows: BTreeMap::new(),
            }),
            natives: Mutex::new(BTreeMap::new()),
        })
    }

    pub(crate) fn pending(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .rows
            .len() as u64
    }

    pub(crate) fn is_clear(&self) -> bool {
        let meta_empty = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .rows
            .is_empty();
        let natives_empty = self
            .natives
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty();
        meta_empty && natives_empty
    }

    pub(crate) fn reserve(
        &self,
        owner: BrowserId,
        popup_id: i32,
    ) -> Result<PopupSlotId, EngineError> {
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if s.draining {
            return Err(EngineError::Closing);
        }
        let owner_count = s.rows.values().filter(|r| r.owner == owner).count();
        if owner_count >= MAX_CHILD_POPUPS_PER_BROWSER {
            return Err(EngineError::Backpressure);
        }
        let seq = s.next_slot;
        s.next_slot = seq.checked_add(1).ok_or(EngineError::Backpressure)?;
        let slot = PopupSlotId(seq);
        s.rows.insert(
            slot,
            PopupRow {
                slot,
                owner,
                popup_id,
                created_native_id: None,
                aborted: false,
                close_requested: false,
                close_posted: false,
                native_released: false,
                client_released: false,
            },
        );
        Ok(slot)
    }

    pub(crate) fn mark_aborted_by_popup_id(&self, owner: BrowserId, popup_id: i32) {
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let matching_slot = s
            .rows
            .iter()
            .find_map(|(&slot, row)| {
                (row.owner == owner
                    && row.popup_id == popup_id
                    && row.created_native_id.is_none()
                    && !row.aborted)
                    .then_some(slot)
            });
        if let Some(slot) = matching_slot
            && let Some(row) = s.rows.get_mut(&slot)
        {
            row.aborted = true;
            if row.ready_to_retire() {
                s.rows.remove(&slot);
            }
        }
    }

    fn adopt_created(&self, slot: PopupSlotId, browser: Browser) -> bool {
        let native_id = browser.identifier();
        if native_id <= 0 {
            drop(browser);
            return true;
        }
        {
            let mut natives = self
                .natives
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            natives.insert(slot, browser);
        }
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let draining = s.draining;
        if let Some(row) = s.rows.get_mut(&slot) {
            row.created_native_id = Some(native_id);
            if draining {
                row.close_requested = true;
            }
            if row.close_requested && !row.close_posted {
                row.close_posted = true;
                return true;
            }
            false
        } else {
            // Unexpected missing metadata: close outside locks below.
            true
        }
    }

    fn on_native_closed(&self, slot: PopupSlotId) -> Option<Browser> {
        let native = self
            .natives
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&slot);
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(row) = s.rows.get_mut(&slot) {
            row.native_released = true;
            if row.ready_to_retire() {
                s.rows.remove(&slot);
            }
        }
        native
    }

    pub(crate) fn on_client_released(&self, slot: PopupSlotId) {
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(row) = s.rows.get_mut(&slot) {
            row.client_released = true;
            if row.ready_to_retire() {
                s.rows.remove(&slot);
            }
        }
    }

    pub(crate) fn collect_closes(&self, owner: Option<BrowserId>, stop_admission: bool) -> Vec<PopupSlotId> {
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if stop_admission {
            s.draining = true;
        }
        let mut to_close = Vec::new();
        for row in s.rows.values_mut() {
            if owner.is_none() || Some(row.owner) == owner {
                row.close_requested = true;
                if row.created_native_id.is_some() && !row.native_released && !row.close_posted {
                    row.close_posted = true;
                    to_close.push(row.slot);
                }
            }
        }
        to_close
    }

    pub(crate) fn close_slot_on_ui(&self, registry: &UiRegistry, slot: PopupSlotId) {
        let native = {
            self.natives
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&slot)
                .cloned()
        };
        let Some(native) = native else {
            return;
        };
        registry.audit_native_entry();
        if native.is_valid() == 0 {
            drop(native);
            return;
        }
        registry.audit_native_entry();
        let host = native.host();
        // Release our wrapper clone before `close_browser` which may synchronously
        // trigger `OnBeforeClose`.
        drop(native);
        if let Some(host) = host {
            registry.audit_native_entry();
            host.close_browser(1);
        }
    }

    pub(crate) fn take_all_natives_for_shutdown(&self) -> Vec<Browser> {
        let map = std::mem::take(
            &mut *self
                .natives
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        map.into_values().collect()
    }
}

impl Drop for PopupLedger {
    fn drop(&mut self) {
        let leftover = std::mem::take(
            self.natives
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        // Never Release CEF Browser wrappers on an arbitrary teardown thread if
        // barrier invariants were bypassed; leak safely rather than UB.
        for (_, browser) in leftover {
            std::mem::forget(browser);
        }
    }
}

pub(crate) fn build_popup_window_info(
    popup_features: Option<&PopupFeatures>,
    title: &str,
) -> WindowInfo {
    let clean_title: String = title
        .chars()
        .filter(|&c| c != '\0')
        .take(128)
        .collect();
    let title = if clean_title.is_empty() {
        "GPUI CEF Popup"
    } else {
        clean_title.as_str()
    };
    let mut info = WindowInfo::default().set_as_popup(cef::sys::HWND(std::ptr::null_mut()), title);
    info.parent_window = cef::sys::HWND(std::ptr::null_mut());
    info.windowless_rendering_enabled = 0;
    info.shared_texture_enabled = 0;
    info.external_begin_frame_enabled = 0;
    info.runtime_style = RuntimeStyle::ALLOY;

    if let Some(features) = popup_features {
        if features.x_set != 0 && (-MAX_NATIVE_COORD..=MAX_NATIVE_COORD).contains(&features.x) {
            info.bounds.x = features.x;
        }
        if features.y_set != 0 && (-MAX_NATIVE_COORD..=MAX_NATIVE_COORD).contains(&features.y) {
            info.bounds.y = features.y;
        }
        if features.width_set != 0
            && features.width > 0
            && features.width <= MAX_NATIVE_BOUND as i32
        {
            info.bounds.width = features.width;
        }
        if features.height_set != 0
            && features.height > 0
            && features.height <= MAX_NATIVE_BOUND as i32
        {
            info.bounds.height = features.height;
        }
    }
    info
}

pub(crate) fn build_popup_client(
    registry: &Arc<UiRegistry>,
    owner: BrowserId,
    popup_id: i32,
    policy: Arc<NativePolicy>,
) -> Result<Client, EngineError> {
    if currently_on(ThreadId::UI) == 0 {
        return Err(EngineError::WrongThread);
    }
    let slot = registry.popups.reserve(owner, popup_id)?;
    let life = Arc::new(PopupLife {
        slot,
        owner,
        policy,
        registry: Arc::downgrade(registry),
        ui_thread: std::thread::current().id(),
        native_id: AtomicI32::new(0),
        closed: AtomicBool::new(false),
    });
    Ok(PopupClient::new(life))
}

struct PopupLife {
    slot: PopupSlotId,
    owner: BrowserId,
    policy: Arc<NativePolicy>,
    registry: Weak<UiRegistry>,
    ui_thread: StdThreadId,
    native_id: AtomicI32,
    closed: AtomicBool,
}

impl PopupLife {
    fn on_ui(&self) -> bool {
        self.ui_thread == std::thread::current().id()
    }
}

impl Drop for PopupLife {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            registry.post_popup_client_released(self.slot);
        }
    }
}

fn is_exact_about_blank(url: &str) -> bool {
    url == "about:blank"
}

wrap_life_span_handler! {
    struct PopupLifeSpan {
        life: Arc<PopupLife>,
    }
    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            popup_id: ::std::os::raw::c_int,
            target_url: Option<&CefString>,
            target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: ::std::os::raw::c_int,
            popup_features: Option<&PopupFeatures>,
            window_info: Option<&mut WindowInfo>,
            client: Option<&mut Option<Client>>,
            settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            if !self.life.on_ui() || self.life.closed.load(Ordering::Acquire) {
                return 1;
            }
            let url = target_url.map(ToString::to_string).unwrap_or_default();
            let policy_allowed = self.life.policy.rules.allows_page_navigation(&url)
                || self.life.policy.rules.allows_open_web_blob(&url)
                || self.life.policy.rules.allows_open_web_data(&url)
                || self.life.policy.rules.allows_open_web_blank(&url, false, true);
            if !policy_allowed {
                let external = url::Url::parse(&url).is_ok_and(|parsed| {
                    !matches!(
                        parsed.scheme(),
                        "http" | "https" | "data" | "about" | "file" | "javascript" | "blob"
                    ) && !self.life.policy.rules.is_custom_scheme(parsed.scheme())
                });
                self.life.policy.deny(
                    Some(self.life.owner),
                    if external {
                        PolicyAction::ExternalProtocol
                    } else {
                        PolicyAction::Navigation
                    },
                );
                return 1;
            }
            let (Some(registry), Some(win_info), Some(client_slot)) =
                (self.life.registry.upgrade(), window_info, client)
            else {
                return 1;
            };
            let title = target_frame_name
                .map(ToString::to_string)
                .filter(|s| !s.is_empty() && !s.starts_with('_'))
                .unwrap_or_else(|| {
                    let clean_url = url.trim();
                    if !clean_url.is_empty() {
                        clean_url.to_owned()
                    } else {
                        "Popup".to_owned()
                    }
                });
            let Ok(popup_client) = registry.open_managed_popup_client(
                self.life.owner,
                popup_id,
                self.life.policy.clone(),
            ) else {
                return 1;
            };
            *win_info = build_popup_window_info(popup_features, &title);
            if let Some(s) = settings {
                s.windowless_frame_rate = 0;
                if s.background_color == 0 {
                    s.background_color = 0xFFFF_FFFF;
                }
            }
            *client_slot = Some(popup_client);
            0
        }

        fn on_before_popup_aborted(
            &self,
            _browser: Option<&mut Browser>,
            popup_id: ::std::os::raw::c_int,
        ) {
            if self.life.on_ui()
                && let Some(registry) = self.life.registry.upgrade()
            {
                registry
                    .popups
                    .mark_aborted_by_popup_id(self.life.owner, popup_id);
            }
        }

        fn on_after_created(&self, browser: Option<&mut Browser>) {
            if !self.life.on_ui() {
                return;
            }
            let (Some(browser), Some(registry)) = (browser, self.life.registry.upgrade()) else {
                return;
            };
            registry.audit_native_entry();
            let id = browser.identifier();
            self.life.native_id.store(id, Ordering::Release);
            let cloned = browser.clone();
            if registry.popups.adopt_created(self.life.slot, cloned) {
                registry.post_popup_close(self.life.slot);
            }
        }

        fn do_close(&self, _browser: Option<&mut Browser>) -> ::std::os::raw::c_int {
            0
        }

        fn on_before_close(&self, browser: Option<&mut Browser>) {
            if !self.life.on_ui() {
                return;
            }
            self.life.closed.store(true, Ordering::Release);
            let _ = browser;
            if let Some(registry) = self.life.registry.upgrade() {
                registry.audit_native_entry();
                let dropped_browser = registry.popups.on_native_closed(self.life.slot);
                drop(dropped_browser);
            }
        }
    }
}

wrap_request_handler! {
    struct PopupRequests {
        owner: BrowserId,
        policy: Arc<NativePolicy>,
    }
    impl RequestHandler {
        fn on_before_browse(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _user_gesture: ::std::os::raw::c_int,
            _is_redirect: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let Some(frame) = frame else {
                self.policy.deny(Some(self.owner), PolicyAction::Navigation);
                return 1;
            };
            let Some(request) = request else {
                self.policy.deny(Some(self.owner), PolicyAction::Navigation);
                return 1;
            };
            let url = CefString::from(&request.url()).to_string();
            let main = frame.is_main() != 0;
            if self.policy.rules.allows_page_navigation(&url)
                || self.policy.allows_chrome_internal_url(&url)
                || self.policy.rules.allows_open_web_blob(&url)
                || self.policy.rules.allows_open_web_data(&url)
                || self.policy.rules.allows_open_web_blank(&url, !main, false)
                || (main && is_exact_about_blank(&url))
            {
                0
            } else {
                let external = url::Url::parse(&url).is_ok_and(|parsed| {
                    !matches!(
                        parsed.scheme(),
                        "http" | "https" | "data" | "about" | "file" | "javascript" | "blob"
                    ) && !self.policy.rules.is_custom_scheme(parsed.scheme())
                });
                self.policy.deny(
                    Some(self.owner),
                    if external {
                        PolicyAction::ExternalProtocol
                    } else {
                        PolicyAction::Navigation
                    },
                );
                1
            }
        }

        fn on_open_urlfrom_tab(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            target_url: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let url = target_url.map(ToString::to_string).unwrap_or_default();
            if self.policy.rules.allows_page_navigation(&url) {
                0
            } else {
                self.policy.deny(Some(self.owner), PolicyAction::Navigation);
                1
            }
        }

        fn resource_request_handler(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            is_navigation: ::std::os::raw::c_int,
            _is_download: ::std::os::raw::c_int,
            _request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<ResourceRequestHandler> {
            // Runs on CEF IO thread; stateless with respect to PopupLife so IO drops
            // never retain or release PopupLife off the UI thread.
            let main = frame.is_some_and(|f| f.is_main() != 0);
            Some(crate::resource_policy::popup_handler(
                self.policy.clone(),
                self.owner,
                is_navigation != 0 && main,
            ))
        }

        fn auth_credentials(
            &self,
            _browser: Option<&mut Browser>,
            _origin_url: Option<&CefString>,
            _is_proxy: ::std::os::raw::c_int,
            _host: Option<&CefString>,
            _port: ::std::os::raw::c_int,
            _realm: Option<&CefString>,
            _scheme: Option<&CefString>,
            _callback: Option<&mut AuthCallback>,
        ) -> ::std::os::raw::c_int {
            self.policy
                .deny(Some(self.owner), PolicyAction::Authentication);
            0
        }

        fn on_certificate_error(
            &self,
            _browser: Option<&mut Browser>,
            _cert_error: Errorcode,
            _request_url: Option<&CefString>,
            _ssl_info: Option<&mut Sslinfo>,
            _callback: Option<&mut Callback>,
        ) -> ::std::os::raw::c_int {
            self.policy.deny(Some(self.owner), PolicyAction::Certificate);
            0
        }
    }
}

wrap_dialog_handler! {
    struct PopupFileDialogs {
        owner: BrowserId,
        policy: Arc<NativePolicy>,
    }
    impl DialogHandler {
        fn on_file_dialog(
            &self,
            _browser: Option<&mut Browser>,
            _mode: FileDialogMode,
            _title: Option<&CefString>,
            _default_file_path: Option<&CefString>,
            _accept_filters: Option<&mut CefStringList>,
            _accept_extensions: Option<&mut CefStringList>,
            _accept_descriptions: Option<&mut CefStringList>,
            callback: Option<&mut FileDialogCallback>,
        ) -> ::std::os::raw::c_int {
            self.policy.deny(Some(self.owner), PolicyAction::FileDialog);
            if let Some(cb) = callback {
                cb.cancel();
            }
            1
        }
    }
}

wrap_jsdialog_handler! {
    struct PopupJsDialogs {
        owner: BrowserId,
        policy: Arc<NativePolicy>,
    }
    impl JsdialogHandler {
        fn on_jsdialog(
            &self,
            _browser: Option<&mut Browser>,
            _origin_url: Option<&CefString>,
            _dialog_type: JsdialogType,
            _message_text: Option<&CefString>,
            _default_prompt_text: Option<&CefString>,
            _callback: Option<&mut JsdialogCallback>,
            suppress_message: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            self.policy.deny(Some(self.owner), PolicyAction::JsDialog);
            if let Some(suppress) = suppress_message {
                *suppress = 1;
            }
            0
        }

        fn on_before_unload_dialog(
            &self,
            _browser: Option<&mut Browser>,
            _message_text: Option<&CefString>,
            _is_reload: ::std::os::raw::c_int,
            callback: Option<&mut JsdialogCallback>,
        ) -> ::std::os::raw::c_int {
            if let Some(cb) = callback {
                cb.cont(1, None);
            }
            1
        }
    }
}

wrap_download_handler! {
    struct PopupDownloads {
        owner: BrowserId,
        policy: Arc<NativePolicy>,
    }
    impl DownloadHandler {
        fn can_download(
            &self,
            _browser: Option<&mut Browser>,
            _url: Option<&CefString>,
            _request_method: Option<&CefString>,
        ) -> ::std::os::raw::c_int {
            self.policy.deny(Some(self.owner), PolicyAction::Download);
            0
        }

        fn on_before_download(
            &self,
            _browser: Option<&mut Browser>,
            _download_item: Option<&mut DownloadItem>,
            _suggested_name: Option<&CefString>,
            _callback: Option<&mut BeforeDownloadCallback>,
        ) -> ::std::os::raw::c_int {
            self.policy.deny(Some(self.owner), PolicyAction::Download);
            0
        }
    }
}

wrap_permission_handler! {
    struct PopupPermissions {
        owner: BrowserId,
        policy: Arc<NativePolicy>,
    }
    impl PermissionHandler {
        fn on_request_media_access_permission(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _requesting_origin: Option<&CefString>,
            _requested_permissions: u32,
            callback: Option<&mut MediaAccessCallback>,
        ) -> ::std::os::raw::c_int {
            self.policy.deny(Some(self.owner), PolicyAction::Permission);
            if let Some(cb) = callback {
                cb.cancel();
            }
            1
        }

        fn on_show_permission_prompt(
            &self,
            _browser: Option<&mut Browser>,
            _prompt_id: u64,
            _requesting_origin: Option<&CefString>,
            _requested_permissions: u32,
            callback: Option<&mut PermissionPromptCallback>,
        ) -> ::std::os::raw::c_int {
            self.policy.deny(Some(self.owner), PolicyAction::Permission);
            if let Some(cb) = callback {
                cb.cont(PermissionRequestResult::DENY);
            }
            1
        }
    }
}

#[allow(clashing_extern_declarations)]
#[link(name = "user32")]
unsafe extern "system" {
    fn IsWindow(hwnd: isize) -> i32;
    fn SetWindowTextW(hwnd: isize, text: *const u16) -> i32;
}

wrap_display_handler! {
    struct PopupDisplay;
    impl DisplayHandler {
        fn on_title_change(&self, browser: Option<&mut Browser>, title: Option<&CefString>) {
            if currently_on(ThreadId::UI) == 0 {
                return;
            }
            let Some(host) = browser.and_then(|b| b.host()) else {
                return;
            };
            let hwnd = host.window_handle().0 as isize;
            if hwnd == 0 || unsafe { IsWindow(hwnd) } == 0 {
                return;
            }
            let clean: String = title
                .map(ToString::to_string)
                .unwrap_or_default()
                .chars()
                .filter(|&c| c != '\0')
                .take(256)
                .collect();
            if clean.trim().is_empty() {
                return;
            }
            let wide: Vec<u16> = clean.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: `hwnd` passed `IsWindow` on the CEF UI thread and `wide` is NUL-terminated UTF-16.
            unsafe {
                let _ = SetWindowTextW(hwnd, wide.as_ptr());
            }
        }
    }
}

wrap_client! {
    struct PopupClient {
        life: Arc<PopupLife>,
    }
    impl Client {
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(PopupDisplay::new())
        }
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(PopupLifeSpan::new(self.life.clone()))
        }
        fn request_handler(&self) -> Option<RequestHandler> {
            Some(PopupRequests::new(
                self.life.owner,
                self.life.policy.clone(),
            ))
        }
        fn dialog_handler(&self) -> Option<DialogHandler> {
            Some(PopupFileDialogs::new(
                self.life.owner,
                self.life.policy.clone(),
            ))
        }
        fn jsdialog_handler(&self) -> Option<JsdialogHandler> {
            Some(PopupJsDialogs::new(
                self.life.owner,
                self.life.policy.clone(),
            ))
        }
        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(PopupDownloads::new(
                self.life.owner,
                self.life.policy.clone(),
            ))
        }
        fn permission_handler(&self) -> Option<PermissionHandler> {
            Some(PopupPermissions::new(
                self.life.owner,
                self.life.policy.clone(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::contracts::RuntimeGeneration;

    fn bid(local: u64) -> BrowserId {
        BrowserId::new(RuntimeGeneration::new(1), local)
    }

    #[test]
    fn build_popup_window_info_clears_windowless_flags_and_applies_bounds() {
        let mut features = PopupFeatures::default();
        features.x = 120;
        features.x_set = 1;
        features.y = 80;
        features.y_set = 1;
        features.width = 960;
        features.width_set = 1;
        features.height = 720;
        features.height_set = 1;

        let info = build_popup_window_info(Some(&features), "Child Window\0Hidden");
        assert_eq!(info.windowless_rendering_enabled, 0);
        assert_eq!(info.shared_texture_enabled, 0);
        assert_eq!(info.external_begin_frame_enabled, 0);
        assert!(matches!(info.runtime_style, RuntimeStyle::ALLOY));
        assert!(info.parent_window.0.is_null());
        assert_eq!(info.bounds.x, 120);
        assert_eq!(info.bounds.y, 80);
        assert_eq!(info.bounds.width, 960);
        assert_eq!(info.bounds.height, 720);
        assert_eq!(info.window_name.to_string(), "Child WindowHidden");
    }

    #[test]
    fn popup_ledger_enforces_per_browser_cap_and_waits_for_abort_and_client_release() {
        let ledger = PopupLedger::new();
        let owner = bid(1);
        let mut slots = Vec::new();
        for i in 0..MAX_CHILD_POPUPS_PER_BROWSER {
            slots.push(ledger.reserve(owner, i as i32).unwrap());
        }
        assert_eq!(
            ledger.reserve(owner, 99),
            Err(EngineError::Backpressure)
        );
        assert_eq!(ledger.pending(), MAX_CHILD_POPUPS_PER_BROWSER as u64);

        // Aborting popup creation does not drop pending until client_released is observed.
        ledger.mark_aborted_by_popup_id(owner, 0);
        assert_eq!(ledger.pending(), MAX_CHILD_POPUPS_PER_BROWSER as u64);
        ledger.on_client_released(slots[0]);
        assert_eq!(ledger.pending(), (MAX_CHILD_POPUPS_PER_BROWSER - 1) as u64);

        for slot in slots.into_iter().skip(1) {
            ledger.on_client_released(slot);
        }
        assert_eq!(ledger.pending(), 0);
        assert!(ledger.is_clear());
    }

    #[test]
    fn popup_ledger_requires_both_native_and_client_release_after_adoption() {
        let ledger = PopupLedger::new();
        let owner = bid(2);
        let slot = ledger.reserve(owner, 5).unwrap();
        {
            let mut s = ledger.state.lock().unwrap();
            let row = s.rows.get_mut(&slot).unwrap();
            row.created_native_id = Some(201);
        }
        assert_eq!(ledger.collect_closes(Some(owner), false), vec![slot]);
        // Repeated collect_closes does not re-post.
        assert!(ledger.collect_closes(Some(owner), false).is_empty());

        // Client release before OnBeforeClose keeps slot pending.
        ledger.on_client_released(slot);
        assert_eq!(ledger.pending(), 1);

        // Native release retires once client was already released.
        assert!(ledger.on_native_closed(slot).is_none());
        assert_eq!(ledger.pending(), 0);

        // Reverse order: OnBeforeClose before client release.
        let slot2 = ledger.reserve(owner, 6).unwrap();
        {
            let mut s = ledger.state.lock().unwrap();
            s.rows.get_mut(&slot2).unwrap().created_native_id = Some(202);
        }
        assert!(ledger.on_native_closed(slot2).is_none());
        assert_eq!(ledger.pending(), 1);
        ledger.on_client_released(slot2);
        assert_eq!(ledger.pending(), 0);
    }

    #[test]
    fn popup_ledger_stop_admission_rejects_new_reserves() {
        let ledger = PopupLedger::new();
        assert!(ledger.collect_closes(None, true).is_empty());
        assert_eq!(ledger.reserve(bid(3), 1), Err(EngineError::Closing));
    }
}
