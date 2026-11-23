//! Auxiliary Chrome commands and modal/permission refusal. Not a complete
//! navigation/resource/download policy, and not permission to enable Show.
use super::*;
// Fixed SDK cef_id_mappers.h:83. This pinned cef-rs does not generate this
// utility declaration; use the already-linked libcef C ABI, no DLL fallback.
unsafe extern "C" {
    fn cef_id_for_command_id_name(name: *const std::ffi::c_char) -> i32;
}

#[derive(Clone, Copy)]
pub(super) struct CloseCommands {
    window: i32,
    tab: i32,
}
impl CloseCommands {
    pub(super) fn checked(window: i32, tab: i32) -> Result<Self, EngineError> {
        if window <= 0 || tab <= 0 || window == tab {
            return Err(EngineError::InvalidRequest);
        }
        Ok(Self { window, tab })
    }
    pub(super) fn resolve(registry: &UiRegistry) -> Result<Self, EngineError> {
        // Called only after factory CEF UI qualification, outside all locks.
        // Fixed SDK recommends names, not numeric IDs from another build.
        registry.note_auxiliary_native_call();
        let window = unsafe { cef_id_for_command_id_name(c"IDC_CLOSE_WINDOW".as_ptr()) };
        registry.note_auxiliary_native_call();
        let tab = unsafe { cef_id_for_command_id_name(c"IDC_CLOSE_TAB".as_ptr()) };
        Self::checked(window, tab)
    }
    fn contains(self, command: i32) -> bool {
        command == self.window || command == self.tab
    }
}
impl Life {
    fn policy_native_call(&self) {
        if let Some(registry) = self.registry.upgrade() {
            registry.note_auxiliary_native_call();
        } else {
            self.book.fault();
        }
    }
    fn policy_browser(&self, browser: Option<&mut Browser>) -> bool {
        // After BeforeClose do not inspect Browser except the allowed lifecycle
        // methods; here no inspection at all is necessary. No foreign HWND lookup.
        if !self.on_ui()
            || self.closed.load(Ordering::Acquire)
            || self.native.load(Ordering::Acquire) <= 0
            || self.book.faulted()
        {
            return false;
        }
        let Some(browser) = browser else {
            return false;
        };
        let raw = ImplBrowser::get_raw(browser);
        if unsafe { (*raw).is_valid.is_none() || (*raw).get_identifier.is_none() } {
            self.book.fault();
            return false;
        }
        self.policy_native_call();
        if browser.is_valid() == 0 || self.closed.load(Ordering::Acquire) || self.book.faulted() {
            return false;
        }
        self.policy_native_call();
        browser.identifier() == self.native.load(Ordering::Acquire)
            && !self.closed.load(Ordering::Acquire)
            && !self.book.faulted()
    }
    fn command_close(&self, browser: Option<&mut Browser>) {
        if !self.policy_browser(browser) {
            return;
        }
        let native = self.native.load(Ordering::Acquire);
        if self.book.native_command_close(self.id, native).is_err() {
            return;
        }
        // Same bounded explicit close protocol as host retirement. Never execute
        // the Chrome default CLOSE_WINDOW, whose unrelated tab/window effects
        // are not authorized by an auxiliary Browser callback alone.
        if let Some(registry) = self.registry.upgrade() {
            registry.schedule_auxiliary_close(self.id);
        } else {
            self.book.fault();
        }
    }
}
wrap_command_handler! {
    pub(super) struct AuxiliaryCommands {life: Arc<Life>}
    impl CommandHandler {
        fn on_chrome_command(&self, browser: Option<&mut Browser>, command_id: i32,
            disposition: WindowOpenDisposition) -> i32 {
            let life=self.life.clone();
            if life.on_ui() && life.close_commands.contains(command_id)
                && disposition == WindowOpenDisposition::CURRENT_TAB {life.command_close(browser);}
            // Even unknown/missing/late/wrong-thread commands are consumed.
            // In particular NewWindow/Print/Save/Exit never reach Chrome default.
            1
        }
        fn is_chrome_app_menu_item_visible(&self, _browser: Option<&mut Browser>, command_id: i32) -> i32 {
            let life=self.life.clone();
            i32::from(life.on_ui() && !life.closed.load(Ordering::Acquire)
                && life.close_commands.contains(command_id))
        }
        fn is_chrome_app_menu_item_enabled(&self, _browser: Option<&mut Browser>, command_id: i32) -> i32 {
            let life=self.life.clone();
            i32::from(life.on_ui() && !life.closed.load(Ordering::Acquire)
                && life.close_commands.contains(command_id))
        }
        fn is_chrome_page_action_icon_visible(&self, _icon_type: ChromePageActionIconType) -> i32 {
            let life=self.life.clone();let _=life.on_ui();0
        }
        fn is_chrome_toolbar_button_visible(&self, _button_type: ChromeToolbarButtonType) -> i32 {
            let life=self.life.clone();let _=life.on_ui();0
        }
    }
}
/// Allow only Chromium's built-in offline DevTools UI assets (`devtools://devtools/bundled/...`
/// and `devtools://theme/...`). Remote frontend URLs (`devtools://devtools/remote/...` and any
/// `http://`/`https://`) stay denied so managed DevTools cannot bypass `TrustedContentPolicy`.
pub(crate) fn is_internal_devtools_resource(url: &str) -> bool {
    if url.trim() != url || url.chars().any(|c| c.is_ascii_control()) || url.contains('\\') {
        return false;
    }
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "devtools"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
    {
        return false;
    }
    match parsed.host_str() {
        Some("devtools") => parsed.path().starts_with("/bundled/") || parsed.path() == "/bundled",
        Some("theme" | "resources") => parsed.path().starts_with('/'),
        _ => false,
    }
}

pub(crate) fn is_devtools_initiator(initiator: Option<&CefString>) -> bool {
    initiator.is_some_and(|s| s.to_string() == "devtools://devtools")
}

/// Top-level DevTools navigations in CEF 156 Chrome runtime arrive at
/// `RequestContextHandler::GetResourceRequestHandler` with `browser == None`
/// and an opaque (`"null"`) or absent browser-process initiator, whereas
/// subresources/workers carry `"devtools://devtools"`.
pub(crate) fn allows_context_devtools_request(
    initiator: Option<&CefString>,
    is_navigation: bool,
) -> bool {
    if is_devtools_initiator(initiator) {
        return true;
    }
    if !is_navigation {
        return false;
    }
    initiator.is_none_or(|s| {
        let text = s.to_string();
        text.is_empty() || text == "null"
    })
}

pub(crate) fn auxiliary_resource_handler() -> ResourceRequestHandler {
    AuxiliaryResourcePolicy::new()
}

wrap_resource_request_handler! {
    struct AuxiliaryResourcePolicy;
    impl ResourceRequestHandler {
        fn on_before_resource_load(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            let allowed = request.is_some_and(|r| {
                is_internal_devtools_resource(&CefString::from(&r.url()).to_string())
            });
            if allowed {
                // cef-rs defaults to CANCEL; explicitly continue for bundled DevTools assets.
                ReturnValue::CONTINUE
            } else {
                ReturnValue::CANCEL
            }
        }

        fn on_resource_redirect(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _response: Option<&mut Response>,
            new_url: Option<&mut CefString>,
        ) {
            if let Some(new_url) = new_url {
                if !is_internal_devtools_resource(&new_url.to_string()) {
                    *new_url = CefString::from("about:blank");
                }
            }
        }

        fn on_protocol_execution(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            allow_os_execution: Option<&mut ::std::os::raw::c_int>,
        ) {
            if let Some(allow) = allow_os_execution {
                *allow = 0;
            }
        }
    }
}

wrap_request_handler! {
    pub(super) struct AuxiliaryRequests { life: Arc<Life> }
    impl RequestHandler {
        fn resource_request_handler(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _is_navigation: ::std::os::raw::c_int,
            _is_download: ::std::os::raw::c_int,
            _request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<ResourceRequestHandler> {
            // Runs on CEF IO thread (CefSharp IRequestHandler.cs:63-74): keep
            // stateless (no Arc<Life>) so IO-thread drops never release Life off
            // the CEF UI thread. Returning a non-null handler prevents falling
            // through to RuntimeRequestContextHandler (IRequestContextHandler.cs:23-25),
            // whose TrustedContentPolicy would cancel bundled devtools:// assets.
            Some(auxiliary_resource_handler())
        }
        fn on_open_urlfrom_tab(
            &self, _browser: Option<&mut Browser>, _frame: Option<&mut Frame>,
            _target_url: Option<&CefString>, _disposition: WindowOpenDisposition, _user_gesture: i32,
        ) -> i32 {
            let life = self.life.clone();
            let _ = life.on_ui();
            // CEF gives this callback first refusal on the paths that reach it.
            // The auxiliary has no authority to open a URL in another surface.
            1
        }
    }
}
wrap_dialog_handler! {
    pub(super) struct AuxiliaryFileDialogs {life: Arc<Life>}
    impl DialogHandler {
        fn on_file_dialog(&self, _browser: Option<&mut Browser>, _mode: FileDialogMode,
            _title: Option<&CefString>, _default_file_path: Option<&CefString>,
            _accept_filters: Option<&mut CefStringList>, _accept_extensions: Option<&mut CefStringList>,
            _accept_descriptions: Option<&mut CefStringList>, callback: Option<&mut FileDialogCallback>) -> i32 {
            let life=self.life.clone();
            if life.on_ui() {
                if let Some(callback)=callback {
                    if unsafe {(*ImplFileDialogCallback::get_raw(callback)).cancel.is_some()} {
                        life.policy_native_call();callback.cancel();
                    } else {life.book.fault();}
                } else {life.book.fault();}
            }
            1 // No default OS modal. Cancellation is not download termination.
        }
    }
}
wrap_jsdialog_handler! {
    pub(super) struct AuxiliaryJsDialogs {life: Arc<Life>}
    impl JsdialogHandler {
        fn on_jsdialog(&self, _browser: Option<&mut Browser>, _origin_url: Option<&CefString>,
            _dialog_type: JsdialogType, _message_text: Option<&CefString>,
            _default_prompt_text: Option<&CefString>, callback: Option<&mut JsdialogCallback>,
            suppress_message: Option<&mut i32>) -> i32 {
            let life=self.life.clone();let on_ui=life.on_ui();
            if let Some(suppress)=suppress_message {*suppress=1;return 0;}
            // No writable suppression flag: false would invoke default UI.
            // Fault remains even if this callback can be answered negatively.
            life.book.fault();
            if on_ui {
                if let Some(callback)=callback {
                    if unsafe {(*ImplJsdialogCallback::get_raw(callback)).cont.is_some()} {
                        life.policy_native_call();callback.cont(0,None);
                    }
                }
            }
            1
        }
        fn on_before_unload_dialog(&self, browser: Option<&mut Browser>, _message_text: Option<&CefString>,
            _is_reload: i32, callback: Option<&mut JsdialogCallback>) -> i32 {
            let life=self.life.clone();
            if !life.on_ui() {return 1;}
            let allow=i32::from(life.policy_browser(browser));
            if let Some(callback)=callback {
                if unsafe {(*ImplJsdialogCallback::get_raw(callback)).cont.is_some()} {
                    life.policy_native_call();callback.cont(allow,None);
                } else {life.book.fault();}
            } else {life.book.fault();}
            1 // No default modal; only the original live auxiliary may unload.
        }
    }
}
wrap_permission_handler! {
    pub(super) struct AuxiliaryPermissions {life: Arc<Life>}
    impl PermissionHandler {
        fn on_request_media_access_permission(&self, _browser: Option<&mut Browser>, _frame: Option<&mut Frame>,
            _requesting_origin: Option<&CefString>, _requested_permissions: u32,
            callback: Option<&mut MediaAccessCallback>) -> i32 {
            let life=self.life.clone();
            if life.on_ui() {
                if let Some(callback)=callback {
                    if unsafe {(*ImplMediaAccessCallback::get_raw(callback)).cancel.is_some()} {
                        life.policy_native_call();callback.cancel();
                    } else {life.book.fault();}
                } else {life.book.fault();}
            }
            1
        }
        fn on_show_permission_prompt(&self, _browser: Option<&mut Browser>, _prompt_id: u64,
            _requesting_origin: Option<&CefString>, _requested_permissions: u32,
            callback: Option<&mut PermissionPromptCallback>) -> i32 {
            let life=self.life.clone();
            if life.on_ui() {
                if let Some(callback)=callback {
                    if unsafe {(*ImplPermissionPromptCallback::get_raw(callback)).cont.is_some()} {
                        life.policy_native_call();callback.cont(PermissionRequestResult::DENY);
                    } else {life.book.fault();}
                } else {life.book.fault();}
            }
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{fixture, prepared_fixture};
    use super::*;
    use cef::rc::ConvertReturnValue;
    use std::sync::atomic::{AtomicI32, AtomicUsize};
    // Library-source interfaces have no wrap_* macro. These are typed Rust-owned
    // CABI stubs, not CEF Browsers/callbacks or private-wrapper transmutation.
    #[repr(C)]
    struct Probe<T> {
        raw: T,
        refs: AtomicUsize,
        id: i32,
        calls: Arc<AtomicUsize>,
        value: Arc<AtomicI32>,
    }
    unsafe extern "C" fn add<T>(base: *mut sys::_cef_base_ref_counted_t) {
        unsafe { &*(base as *mut Probe<T>) }
            .refs
            .fetch_add(1, Ordering::SeqCst);
    }
    unsafe extern "C" fn release<T>(base: *mut sys::_cef_base_ref_counted_t) -> i32 {
        if unsafe { &*(base as *mut Probe<T>) }
            .refs
            .fetch_sub(1, Ordering::SeqCst)
            == 1
        {
            drop(unsafe { Box::from_raw(base as *mut Probe<T>) });
            1
        } else {
            0
        }
    }
    unsafe extern "C" fn one<T>(base: *mut sys::_cef_base_ref_counted_t) -> i32 {
        i32::from(
            unsafe { &*(base as *mut Probe<T>) }
                .refs
                .load(Ordering::SeqCst)
                == 1,
        )
    }
    unsafe extern "C" fn any<T>(base: *mut sys::_cef_base_ref_counted_t) -> i32 {
        i32::from(
            unsafe { &*(base as *mut Probe<T>) }
                .refs
                .load(Ordering::SeqCst)
                > 0,
        )
    }
    fn allocate<T>(mut raw: T, id: i32, calls: Arc<AtomicUsize>, value: Arc<AtomicI32>) -> *mut T {
        assert_eq!(std::mem::offset_of!(Probe<T>, raw), 0);
        // All five concrete CAPI headers below assert base offset zero first.
        let base = unsafe { &mut *(&mut raw as *mut T).cast::<sys::_cef_base_ref_counted_t>() };
        base.size = std::mem::size_of::<T>();
        base.add_ref = Some(add::<T>);
        base.release = Some(release::<T>);
        base.has_one_ref = Some(one::<T>);
        base.has_at_least_one_ref = Some(any::<T>);
        Box::into_raw(Box::new(Probe {
            raw,
            refs: AtomicUsize::new(1),
            id,
            calls,
            value,
        }))
        .cast()
    }
    unsafe extern "C" fn valid(raw: *mut sys::_cef_browser_t) -> i32 {
        unsafe { &*(raw as *mut Probe<sys::_cef_browser_t>) }
            .calls
            .fetch_add(1, Ordering::SeqCst);
        1
    }
    unsafe extern "C" fn identifier(raw: *mut sys::_cef_browser_t) -> i32 {
        let p = unsafe { &*(raw as *mut Probe<sys::_cef_browser_t>) };
        p.calls.fetch_add(1, Ordering::SeqCst);
        p.id
    }
    struct ProbeBrowser;
    impl ProbeBrowser {
        fn new(id: i32, calls: Arc<AtomicUsize>) -> Browser {
            assert_eq!(std::mem::offset_of!(sys::_cef_browser_t, base), 0);
            let mut raw: sys::_cef_browser_t = unsafe { std::mem::zeroed() };
            raw.is_valid = Some(valid);
            raw.get_identifier = Some(identifier);
            allocate(raw, id, calls, Arc::new(AtomicI32::new(-1))).wrap_result()
        }
    }
    unsafe extern "C" fn file_cancel(raw: *mut sys::_cef_file_dialog_callback_t) {
        unsafe { &*(raw as *mut Probe<sys::_cef_file_dialog_callback_t>) }
            .calls
            .fetch_add(1, Ordering::SeqCst);
    }
    struct ProbeFile;
    impl ProbeFile {
        fn new(calls: Arc<AtomicUsize>) -> FileDialogCallback {
            assert_eq!(
                std::mem::offset_of!(sys::_cef_file_dialog_callback_t, base),
                0
            );
            let mut raw: sys::_cef_file_dialog_callback_t = unsafe { std::mem::zeroed() };
            raw.cancel = Some(file_cancel);
            allocate(raw, 0, calls, Arc::new(AtomicI32::new(-1))).wrap_result()
        }
    }
    unsafe extern "C" fn media_cancel(raw: *mut sys::_cef_media_access_callback_t) {
        unsafe { &*(raw as *mut Probe<sys::_cef_media_access_callback_t>) }
            .calls
            .fetch_add(1, Ordering::SeqCst);
    }
    struct ProbeMedia;
    impl ProbeMedia {
        fn new(calls: Arc<AtomicUsize>) -> MediaAccessCallback {
            assert_eq!(
                std::mem::offset_of!(sys::_cef_media_access_callback_t, base),
                0
            );
            let mut raw: sys::_cef_media_access_callback_t = unsafe { std::mem::zeroed() };
            raw.cancel = Some(media_cancel);
            allocate(raw, 0, calls, Arc::new(AtomicI32::new(-1))).wrap_result()
        }
    }
    unsafe extern "C" fn permission_deny(
        raw: *mut sys::_cef_permission_prompt_callback_t,
        result: sys::cef_permission_request_result_t,
    ) {
        let result: PermissionRequestResult = result.into();
        assert_eq!(result, PermissionRequestResult::DENY);
        unsafe { &*(raw as *mut Probe<sys::_cef_permission_prompt_callback_t>) }
            .value
            .fetch_add(1, Ordering::SeqCst);
    }
    struct ProbePermission;
    impl ProbePermission {
        fn new(value: Arc<AtomicI32>) -> PermissionPromptCallback {
            assert_eq!(
                std::mem::offset_of!(sys::_cef_permission_prompt_callback_t, base),
                0
            );
            let mut raw: sys::_cef_permission_prompt_callback_t = unsafe { std::mem::zeroed() };
            raw.cont = Some(permission_deny);
            allocate(raw, 0, Arc::new(AtomicUsize::new(0)), value).wrap_result()
        }
    }
    unsafe extern "C" fn js_answer(
        raw: *mut sys::_cef_jsdialog_callback_t,
        success: i32,
        _input: *const sys::cef_string_t,
    ) {
        unsafe { &*(raw as *mut Probe<sys::_cef_jsdialog_callback_t>) }
            .value
            .store(success, Ordering::SeqCst);
    }
    struct ProbeJs;
    impl ProbeJs {
        fn new(value: Arc<AtomicI32>) -> JsdialogCallback {
            assert_eq!(std::mem::offset_of!(sys::_cef_jsdialog_callback_t, base), 0);
            let mut raw: sys::_cef_jsdialog_callback_t = unsafe { std::mem::zeroed() };
            raw.cont = Some(js_answer);
            allocate(raw, 0, Arc::new(AtomicUsize::new(0)), value).wrap_result()
        }
    }
    #[test]
    fn auxiliary_policy_missing_js_suppression_pointer_never_uses_default_modal() {
        let (r, _e, _posted, client, _life) = fixture(true);
        let js = client.jsdialog_handler().unwrap();
        let answer = Arc::new(AtomicI32::new(-1));
        let mut callback = ProbeJs::new(answer.clone());
        assert_eq!(
            js.on_jsdialog(
                None,
                None,
                JsdialogType::ALERT,
                None,
                None,
                Some(&mut callback),
                None
            ),
            1
        );
        assert_eq!(answer.load(Ordering::SeqCst), 0);
        assert!(r.devtools.faulted());
        unsafe {
            (*ImplJsdialogCallback::get_raw(&callback)).cont = None;
        }
        answer.store(-1, Ordering::SeqCst);
        assert_eq!(
            js.on_jsdialog(
                None,
                None,
                JsdialogType::ALERT,
                None,
                None,
                Some(&mut callback),
                None
            ),
            1
        );
        assert_eq!(answer.load(Ordering::SeqCst), -1);
    }
    #[test]
    fn auxiliary_request_handler_refuses_open_url_and_holds_client_retirement() {
        let (registry, client, id, _) = prepared_fixture();
        let handler = client.request_handler().unwrap();
        for disposition in [
            WindowOpenDisposition::CURRENT_TAB,
            WindowOpenDisposition::NEW_WINDOW,
        ] {
            assert_eq!(
                handler.on_open_urlfrom_tab(None, None, None, disposition, 1),
                1
            );
        }
        registry.devtools.abandon_preissue(id).unwrap();
        drop(client);
        assert_eq!(registry.devtools.pending(), 1);
        drop(handler);
        assert_eq!(registry.devtools.pending(), 0);
        assert!(!registry.devtools.faulted());
    }

    #[test]
    fn auxiliary_resource_policy_allows_only_bundled_devtools_and_is_io_thread_safe() {
        for (url, expected) in [
            (
                "devtools://devtools/bundled/entrypoints/devtools_app/devtools_app.js",
                true,
            ),
            (
                "devtools://devtools/bundled/devtools_app.html?targetType=tab",
                true,
            ),
            ("devtools://theme/colors.css?sets=ui,chrome", true),
            ("devtools://resources/css/text_defaults.css", true),
            (
                "devtools://devtools/remote/serve_file/@abc/devtools_app.html",
                false,
            ),
            ("devtools://devtools/custom/foo.js", false),
            ("devtools://evil/bundled/foo.js", false),
            ("devtools://user@devtools/bundled/foo.js", false),
            ("devtools://devtools:80/bundled/foo.js", false),
            (
                "https://chrome-devtools-frontend.appspot.com/serve_file/x",
                false,
            ),
            ("http://127.0.0.1/index.html", false),
            ("file:///C:/Windows/win.ini", false),
            ("data:text/html,hi", false),
            (" devtools://devtools/bundled/a.js", false),
            ("devtools://devtools/bundled\\a.js", false),
        ] {
            assert_eq!(is_internal_devtools_resource(url), expected, "{url:?}");
        }
        assert!(is_devtools_initiator(Some(&CefString::from(
            "devtools://devtools"
        ))));
        assert!(!is_devtools_initiator(Some(&CefString::from(
            "https://example.com"
        ))));
        assert!(!is_devtools_initiator(None));

        // Top-level DevTools navigation in CEF 156 Chrome runtime carries an
        // opaque "null" or absent initiator; subresources require devtools://devtools.
        assert!(allows_context_devtools_request(
            Some(&CefString::from("null")),
            true
        ));
        assert!(allows_context_devtools_request(None, true));
        assert!(!allows_context_devtools_request(
            Some(&CefString::from("null")),
            false
        ));
        assert!(!allows_context_devtools_request(None, false));
        assert!(allows_context_devtools_request(
            Some(&CefString::from("devtools://devtools")),
            false
        ));
        assert!(!allows_context_devtools_request(
            Some(&CefString::from("https://example.com")),
            true
        ));

        // ResourceRequestHandler is invoked and dropped on the CEF IO thread, so
        // it must stay stateless and never pin Life or fault the UI ledger.
        let (registry, client, id, _) = prepared_fixture();
        let req_handler = client.request_handler().unwrap();
        let resource_handler = req_handler
            .resource_request_handler(None, None, None, 0, 0, None, None)
            .expect("DevTools RequestHandler must override RequestContextHandler");
        assert_eq!(
            resource_handler.on_before_resource_load(None, None, None, None),
            ReturnValue::CANCEL
        );
        let mut allow_os = 1;
        resource_handler.on_protocol_execution(None, None, None, Some(&mut allow_os));
        assert_eq!(allow_os, 0);

        registry.devtools.abandon_preissue(id).unwrap();
        drop(client);
        drop(req_handler);
        // Prepared slot retires even while the stateless ResourceRequestHandler
        // still exists and drops on a worker/IO thread.
        assert_eq!(registry.devtools.pending(), 0);
        assert!(!registry.devtools.faulted());
        std::thread::spawn(move || drop(resource_handler))
            .join()
            .unwrap();
        assert!(!registry.devtools.faulted());
    }

    #[test]
    fn auxiliary_policy_all_generated_handler_fields_hold_reserved_gate_until_last_drop() {
        let (r, client, id, _) = prepared_fixture();
        let command = client.command_handler().unwrap();
        let file = client.dialog_handler().unwrap();
        let js = client.jsdialog_handler().unwrap();
        let permission = client.permission_handler().unwrap();
        r.devtools.abandon_preissue(id).unwrap();
        drop(client);
        drop(command);
        drop(file);
        drop(js);
        assert_eq!(r.devtools.pending(), 1);
        drop(permission);
        assert_eq!(r.devtools.pending(), 0);
        assert!(!r.devtools.faulted());
    }
    #[test]
    fn auxiliary_policy_chrome_commands_consume_defaults_and_route_only_original_close() {
        assert!(CloseCommands::checked(-1, 34015).is_err());
        assert!(CloseCommands::checked(1, 1).is_err());
        for command in [34012, 34015] {
            let (r, _e, posted, client, life) = fixture(true);
            let id = life.id;
            life.created(201).unwrap();
            r.devtools.native_adopted(id, 201).unwrap();
            let handler = client.command_handler().unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let mut own = ProbeBrowser::new(201, calls.clone());
            for c in [34000, 34001, 34031, 35003, -1, 0, i32::MAX] {
                assert_eq!(
                    handler.on_chrome_command(
                        Some(&mut own),
                        c,
                        WindowOpenDisposition::CURRENT_TAB
                    ),
                    1
                );
                assert_eq!(handler.is_chrome_app_menu_item_visible(None, c), 0);
            }
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert!(posted.lock().unwrap().is_empty());
            assert_eq!(
                handler.on_chrome_command(
                    Some(&mut own),
                    command,
                    WindowOpenDisposition::NEW_WINDOW
                ),
                1
            );
            let mut other = ProbeBrowser::new(911, calls.clone());
            assert_eq!(
                handler.on_chrome_command(
                    Some(&mut other),
                    command,
                    WindowOpenDisposition::CURRENT_TAB
                ),
                1
            );
            assert!(posted.lock().unwrap().is_empty());
            assert_eq!(
                handler.on_chrome_command(
                    Some(&mut own),
                    command,
                    WindowOpenDisposition::CURRENT_TAB
                ),
                1
            );
            assert_eq!(posted.lock().unwrap().len(), 1);
            assert_eq!(
                handler.on_chrome_command(
                    Some(&mut own),
                    command,
                    WindowOpenDisposition::CURRENT_TAB
                ),
                1
            );
            assert_eq!(posted.lock().unwrap().len(), 1); // no automatic retry/duplicate
            assert_eq!(
                r.devtools.native_command_close(
                    super::super::super::AuxiliaryId {
                        sequence: id.sequence + 1,
                        ..id
                    },
                    201
                ),
                Err(EngineError::InvalidRequest)
            );
            let before = calls.load(Ordering::SeqCst);
            life.before_close(201).unwrap();
            assert_eq!(
                handler.on_chrome_command(
                    Some(&mut own),
                    command,
                    WindowOpenDisposition::CURRENT_TAB
                ),
                1
            );
            assert_eq!(calls.load(Ordering::SeqCst), before);
        }
    }
    #[test]
    fn auxiliary_policy_modal_and_permission_callbacks_deny_without_native_default() {
        let (r, _e, _posted, client, life) = fixture(true);
        life.created(201).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let value = Arc::new(AtomicI32::new(-1));
        let file = client.dialog_handler().unwrap();
        let mut cb = ProbeFile::new(calls.clone());
        assert_eq!(
            file.on_file_dialog(
                None,
                FileDialogMode::OPEN,
                None,
                None,
                None,
                None,
                None,
                Some(&mut cb)
            ),
            1
        );
        let permissions = client.permission_handler().unwrap();
        let mut media = ProbeMedia::new(calls.clone());
        assert_eq!(
            permissions.on_request_media_access_permission(None, None, None, 0, Some(&mut media)),
            1
        );
        let mut permission = ProbePermission::new(value.clone());
        assert_eq!(
            permissions.on_show_permission_prompt(None, 1, None, 0, Some(&mut permission)),
            1
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(value.load(Ordering::SeqCst), 0);
        let js = client.jsdialog_handler().unwrap();
        let mut suppress = 0;
        assert_eq!(
            js.on_jsdialog(
                None,
                None,
                JsdialogType::ALERT,
                None,
                None,
                None,
                Some(&mut suppress)
            ),
            0
        );
        assert_eq!(suppress, 1);
        let mut jscb = ProbeJs::new(value.clone());
        let counters = Arc::new(AtomicUsize::new(0));
        let mut own = ProbeBrowser::new(201, counters.clone());
        assert_eq!(
            js.on_before_unload_dialog(Some(&mut own), None, 0, Some(&mut jscb)),
            1
        );
        assert_eq!(value.load(Ordering::SeqCst), 1);
        assert_eq!(
            js.on_before_unload_dialog(None, None, 0, Some(&mut jscb)),
            1
        );
        assert_eq!(value.load(Ordering::SeqCst), 0);
        assert!(!r.devtools.faulted());
        // Null native cancel slot must fault; SDK's silent no-op is not success.
        unsafe {
            (*ImplFileDialogCallback::get_raw(&cb)).cancel = None;
        }
        assert_eq!(
            file.on_file_dialog(
                None,
                FileDialogMode::OPEN,
                None,
                None,
                None,
                None,
                None,
                Some(&mut cb)
            ),
            1
        );
        assert!(r.devtools.faulted());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
    #[test]
    fn auxiliary_policy_wrong_thread_never_executes_cancel_or_default_command() {
        let (r, _e, posted, client, _life) = fixture(true);
        let file = client.dialog_handler().unwrap();
        let command = client.command_handler().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut callback = ProbeFile::new(calls.clone());
        std::thread::spawn(move || {
            assert_eq!(
                file.on_file_dialog(
                    None,
                    FileDialogMode::OPEN,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(&mut callback)
                ),
                1
            );
            assert_eq!(
                command.on_chrome_command(None, 34012, WindowOpenDisposition::CURRENT_TAB),
                1
            );
        })
        .join()
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(posted.lock().unwrap().is_empty());
        assert!(r.devtools.faulted());
        assert_eq!(r.devtools.pending(), 2);
    }
}
