//! Default-deny native capability refusal with opt-in host capability routing.
//!
//! When a host capability service slot in [`rust_cef_core::host_capabilities::HostCapabilities`]
//! is `None`, the handler preserves exact default-deny refusal and emits `PolicyDenied`
//! without ever selecting an unmanaged Chrome/OS modal UI.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]
use crate::{
    client::BrowserClientState,
    host_requests::{DownloadUpdateSnapshot, PermissionReply},
};
use cef::*;
use rust_cef_core::{
    host_capabilities::{
        DownloadRequest, DownloadState, FileDialogKind, FileDialogRequest, JsDialogKind,
        JsDialogRequest, MAX_FILE_DIALOG_PATHS, PermissionKind, PermissionRequest,
    },
    ports::PolicyAction,
};
use std::{path::PathBuf, sync::Arc};

pub(crate) struct PolicyHandlers {
    pub(crate) js: JsdialogHandler,
    pub(crate) file: DialogHandler,
    pub(crate) download: DownloadHandler,
    pub(crate) permission: PermissionHandler,
}
impl PolicyHandlers {
    pub(crate) fn new(state: Arc<BrowserClientState>) -> Self {
        Self {
            js: RuntimeJsDialogs::new(state.clone()),
            file: RuntimeFileDialogs::new(state.clone()),
            download: RuntimeDownloads::new(state.clone()),
            permission: RuntimePermissions::new(state),
        }
    }
}

fn map_file_dialog_kind(mode: FileDialogMode) -> FileDialogKind {
    match mode.get_raw() & 0xff {
        x if x == FileDialogMode::OPEN_MULTIPLE.get_raw() => FileDialogKind::OpenMultiple,
        x if x == FileDialogMode::OPEN_FOLDER.get_raw() => FileDialogKind::OpenFolder,
        x if x == FileDialogMode::SAVE.get_raw() => FileDialogKind::Save,
        _ => FileDialogKind::Open,
    }
}

fn map_js_dialog_kind(dialog_type: JsdialogType) -> JsDialogKind {
    if dialog_type == JsdialogType::CONFIRM {
        JsDialogKind::Confirm
    } else if dialog_type == JsdialogType::PROMPT {
        JsDialogKind::Prompt
    } else {
        JsDialogKind::Alert
    }
}

wrap_jsdialog_handler! {
    struct RuntimeJsDialogs { state: Arc<BrowserClientState>, }
    impl JsdialogHandler {
        fn on_jsdialog(
            &self, _browser: Option<&mut Browser>, origin_url: Option<&CefString>,
            dialog_type: JsdialogType, message_text: Option<&CefString>,
            default_prompt_text: Option<&CefString>, callback: Option<&mut JsdialogCallback>,
            suppress_message: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            if let Some(svc) = self.state.effective_host_capabilities().js_dialog {
                let Some(registry) = self.state.registry.upgrade() else {
                    if let Some(suppress) = suppress_message {
                        *suppress = 1;
                    }
                    return 0;
                };
                let Some(callback) = callback else {
                    if let Some(suppress) = suppress_message {
                        *suppress = 1;
                    }
                    return 0;
                };
                let _guard = registry.host_requests.enter_callback();
                match registry
                    .host_requests
                    .register_js_dialog(self.state.browser, callback.clone())
                {
                    Ok(id) => {
                        svc.on_js_dialog(JsDialogRequest {
                            id,
                            origin_url: origin_url.map(ToString::to_string).unwrap_or_default(),
                            kind: map_js_dialog_kind(dialog_type),
                            message_text: message_text.map(ToString::to_string).unwrap_or_default(),
                            default_prompt_text: default_prompt_text
                                .map(ToString::to_string)
                                .unwrap_or_default(),
                        });
                        return 1;
                    }
                    Err((cb, _)) => {
                        drop(cb);
                        if let Some(suppress) = suppress_message {
                            *suppress = 1;
                        }
                        return 0;
                    }
                }
            }
            self.state.deny(PolicyAction::JsDialog);
            if let Some(suppress) = suppress_message { *suppress = 1; }
            // SDK suppression: false with suppress=true, not an unanswered
            // custom dialog and not the default modal implementation.
            0
        }
        fn on_before_unload_dialog(
            &self, _browser: Option<&mut Browser>, _message_text: Option<&CefString>,
            is_reload: ::std::os::raw::c_int, callback: Option<&mut JsdialogCallback>,
        ) -> ::std::os::raw::c_int {
            if let Some(callback) = callback {
                let callback = callback.clone();
                if let Some(registry) = self.state.registry.upgrade() {
                    registry.before_unload(self.state.browser, is_reload != 0, callback);
                } else {
                    // No retained callback or fallback modal when the owner is
                    // absent. This native UI frame still owns a valid callback.
                    self.state.audit_native();
                    callback.cont(0, None);
                }
            }
            1
        }
        fn on_reset_dialog_state(&self, _browser: Option<&mut Browser>) {
            if let Some(registry) = self.state.registry.upgrade() {
                #[cfg(feature="test-support")]
                registry.note_probe_dialog_reset(self.state.browser);
                registry.reset_dialogs(self.state.browser);
            }
        }
        fn on_dialog_closed(&self, _browser: Option<&mut Browser>) {
            // Fixed implementation may synchronously reenter reset. Tokens
            // have already been removed before any continuation is invoked.
            if let Some(registry) = self.state.registry.upgrade() {
                registry.reset_dialogs(self.state.browser);
            }
        }
    }
}

wrap_dialog_handler! {
    struct RuntimeFileDialogs { state: Arc<BrowserClientState>, }
    impl DialogHandler {
        fn on_file_dialog(
            &self, _browser: Option<&mut Browser>, mode: FileDialogMode,
            title: Option<&CefString>, default_file_path: Option<&CefString>,
            accept_filters: Option<&mut CefStringList>, _accept_extensions: Option<&mut CefStringList>,
            _accept_descriptions: Option<&mut CefStringList>, callback: Option<&mut FileDialogCallback>,
        ) -> ::std::os::raw::c_int {
            if let Some(svc) = self.state.effective_host_capabilities().file_dialog {
                let (Some(registry), Some(callback)) = (self.state.registry.upgrade(), callback)
                else {
                    return 1;
                };
                let _guard = registry.host_requests.enter_callback();
                let filters = accept_filters
                    .and_then(|list| cef::string_list_copy(Some(list)))
                    .map(|copy| copy.into_iter().take(MAX_FILE_DIALOG_PATHS).collect())
                    .unwrap_or_default();
                match registry
                    .host_requests
                    .register_file_dialog(self.state.browser, callback.clone())
                {
                    Ok(id) => {
                        svc.on_file_dialog(FileDialogRequest {
                            id,
                            mode: map_file_dialog_kind(mode),
                            title: title.map(ToString::to_string).unwrap_or_default(),
                            default_file_path: default_file_path
                                .map(ToString::to_string)
                                .unwrap_or_default(),
                            accept_filters: filters,
                        });
                    }
                    Err((cb, _)) => {
                        self.state.audit_native();
                        cb.cancel();
                    }
                }
                return 1;
            }
            self.state.deny(PolicyAction::FileDialog);
            if let Some(callback) = callback { self.state.audit_native(); callback.cancel(); }
            1
        }
    }
}

fn read_userfree_opt(value: CefStringUserfree) -> String {
    let raw: Option<&cef::sys::_cef_string_utf16_t> = (&value).into();
    if raw.is_some() {
        CefString::from(&value).to_string()
    } else {
        String::new()
    }
}

wrap_download_handler! {
    struct RuntimeDownloads { state: Arc<BrowserClientState>, }
    impl DownloadHandler {
        fn can_download(&self, _browser: Option<&mut Browser>, _url: Option<&CefString>,
            _request_method: Option<&CefString>) -> ::std::os::raw::c_int {
            if self.state.effective_host_capabilities().download.is_some() {
                return 1;
            }
            self.state.deny(PolicyAction::Download);
            0
        }
        fn on_before_download(&self, _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>, suggested_name: Option<&CefString>,
            callback: Option<&mut BeforeDownloadCallback>) -> ::std::os::raw::c_int {
            if let Some(svc) = self.state.effective_host_capabilities().download {
                let (Some(registry), Some(item), Some(callback)) = (
                    self.state.registry.upgrade(),
                    download_item,
                    callback,
                ) else {
                    return 0;
                };
                let _guard = registry.host_requests.enter_callback();
                let cef_id = item.id();
                let url = read_userfree_opt(item.url());
                let original_url = read_userfree_opt(item.original_url());
                let mime_type = read_userfree_opt(item.mime_type());
                let total_bytes = u64::try_from(item.total_bytes()).ok().filter(|&b| b > 0);
                let suggested = suggested_name
                    .map(ToString::to_string)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| read_userfree_opt(item.suggested_file_name()));
                match registry.host_requests.register_download(
                    self.state.browser,
                    cef_id,
                    callback.clone(),
                ) {
                    Ok(id) => {
                        svc.on_before_download(DownloadRequest {
                            id,
                            url,
                            original_url,
                            suggested_name: suggested,
                            mime_type,
                            total_bytes,
                        });
                        return 1;
                    }
                    Err((cb, _)) => {
                        drop(cb);
                        return 0;
                    }
                }
            }
            self.state.deny(PolicyAction::Download);
            // This component fixes RuntimeStyle::ALLOY: the fixed SDK's
            // documented false/default path CANCELS there (Chrome would show
            // a shelf). Returning true without executing the callback would
            // leave it unanswered; Continue could choose a download path.
            0
        }
        fn on_download_updated(&self, _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>, callback: Option<&mut DownloadItemCallback>) {
            let Some(item) = download_item else {
                return;
            };
            let state = if item.is_complete() != 0 {
                DownloadState::Complete
            } else if item.is_canceled() != 0 {
                DownloadState::Canceled
            } else if item.is_interrupted() != 0 {
                DownloadState::Interrupted
            } else {
                DownloadState::InProgress
            };
            if let Some(registry) = self.state.registry.upgrade() {
                let _guard = registry.host_requests.enter_callback();
                let path_str = read_userfree_opt(item.full_path());
                let snapshot = DownloadUpdateSnapshot {
                    cef_id: item.id(),
                    state,
                    received_bytes: u64::try_from(item.received_bytes()).unwrap_or(0),
                    total_bytes: u64::try_from(item.total_bytes()).ok().filter(|&b| b > 0),
                    current_speed: item.current_speed(),
                    percent_complete: item.percent_complete(),
                    full_path: (!path_str.is_empty()).then(|| PathBuf::from(path_str)),
                };
                let caps = self.state.effective_host_capabilities();
                registry.host_requests.on_download_updated(
                    self.state.browser,
                    snapshot,
                    callback.cloned(),
                    registry.audit_ref(),
                    &caps,
                );
            } else if state == DownloadState::InProgress {
                if let Some(callback) = callback {
                    self.state.audit_native();
                    callback.cancel();
                }
            }
        }
    }
}

wrap_permission_handler! {
    struct RuntimePermissions { state: Arc<BrowserClientState>, }
    impl PermissionHandler {
        fn on_request_media_access_permission(&self, _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>, requesting_origin: Option<&CefString>,
            requested_permissions: u32, callback: Option<&mut MediaAccessCallback>) -> ::std::os::raw::c_int {
            if let Some(svc) = self.state.effective_host_capabilities().permission {
                let (Some(registry), Some(callback)) = (self.state.registry.upgrade(), callback)
                else {
                    return 1;
                };
                let _guard = registry.host_requests.enter_callback();
                match registry
                    .host_requests
                    .register_permission(self.state.browser, PermissionReply::Media(callback.clone()))
                {
                    Ok(id) => {
                        svc.on_permission_request(PermissionRequest {
                            id,
                            kind: PermissionKind::MediaAccess,
                            requesting_origin: requesting_origin
                                .map(ToString::to_string)
                                .unwrap_or_default(),
                            requested_permissions,
                        });
                    }
                    Err((PermissionReply::Media(cb), _)) => {
                        self.state.audit_native();
                        cb.cancel();
                    }
                    Err(_) => {}
                }
                return 1;
            }
            self.state.deny(PolicyAction::Permission);
            if let Some(callback) = callback { self.state.audit_native(); callback.cancel(); }
            1
        }
        fn on_show_permission_prompt(&self, _browser: Option<&mut Browser>, _prompt_id: u64,
            requesting_origin: Option<&CefString>, requested_permissions: u32,
            callback: Option<&mut PermissionPromptCallback>) -> ::std::os::raw::c_int {
            if let Some(svc) = self.state.effective_host_capabilities().permission {
                let (Some(registry), Some(callback)) = (self.state.registry.upgrade(), callback)
                else {
                    return 1;
                };
                let _guard = registry.host_requests.enter_callback();
                match registry.host_requests.register_permission(
                    self.state.browser,
                    PermissionReply::Prompt(callback.clone()),
                ) {
                    Ok(id) => {
                        svc.on_permission_request(PermissionRequest {
                            id,
                            kind: PermissionKind::PermissionPrompt,
                            requesting_origin: requesting_origin
                                .map(ToString::to_string)
                                .unwrap_or_default(),
                            requested_permissions,
                        });
                    }
                    Err((PermissionReply::Prompt(cb), _)) => {
                        self.state.audit_native();
                        cb.cont(PermissionRequestResult::DENY);
                    }
                    Err(_) => {}
                }
                return 1;
            }
            self.state.deny(PolicyAction::Permission);
            if let Some(callback) = callback {
                self.state.audit_native(); callback.cont(PermissionRequestResult::DENY);
            }
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{client::ViewportState, policy::CompiledPolicy, policy_native::NativePolicy};
    use rust_cef_core::{
        contracts::{BrowserId, RuntimeGeneration, TrustedContentPolicy, Viewport},
        mailbox::FrameMailbox,
        ports::{
            BrowserEventEnvelope, BrowserEventSink, EngineError, FrameSink, OwnedFrame,
            PublishResult, UiDispatcher,
        },
    };
    use std::sync::{
        Weak,
        atomic::{AtomicU64, Ordering},
    };

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
    fn state() -> Arc<BrowserClientState> {
        let rules = Arc::new(
            CompiledPolicy::compile(&TrustedContentPolicy {
                allowed_origins: vec![],
                allow_embedded_data: false,
            })
            .unwrap(),
        );
        Arc::new(BrowserClientState::new(
            BrowserId::new(RuntimeGeneration::new(1), 1),
            Arc::new(ViewportState::new(Viewport {
                width: 320,
                height: 240,
                scale: 1.0,
            })),
            Arc::new(FrameMailbox::new(BrowserId::new(
                RuntimeGeneration::new(1),
                1,
            ))),
            Arc::new(Sinks),
            Arc::new(Sinks),
            Weak::new(),
            NativePolicy::new(rules, Weak::new()),
            "about:blank",
        ))
    }
    use cef::rc::{ConvertReturnValue, RcImpl};
    // These three fixed bindings are native-only (no wrap_* macros). A tiny
    // Rust-owned C ABI callback checks routing only, not native modal behavior.
    extern "C" fn file_cancel(this: *mut cef::sys::_cef_file_dialog_callback_t) {
        RcImpl::<_, Arc<AtomicU64>>::get(this)
            .interface
            .fetch_add(1, Ordering::SeqCst);
    }
    extern "C" fn media_cancel(this: *mut cef::sys::_cef_media_access_callback_t) {
        RcImpl::<_, Arc<AtomicU64>>::get(this)
            .interface
            .fetch_add(1, Ordering::SeqCst);
    }
    extern "C" fn permission_deny(
        this: *mut cef::sys::_cef_permission_prompt_callback_t,
        result: cef::sys::cef_permission_request_result_t,
    ) {
        let increment = if PermissionRequestResult::from(result) == PermissionRequestResult::DENY {
            1
        } else {
            100
        };
        RcImpl::<_, Arc<AtomicU64>>::get(this)
            .interface
            .fetch_add(increment, Ordering::SeqCst);
    }
    fn cancel_file(calls: Arc<AtomicU64>) -> FileDialogCallback {
        // SAFETY: C ABI fields are nullable; RcImpl installs the full base
        // refcount vtable, and wrap_result consumes exactly its initial ref.
        let mut raw: cef::sys::_cef_file_dialog_callback_t = unsafe { std::mem::zeroed() };
        raw.cancel = Some(file_cancel);
        RcImpl::new(raw, calls)
            .cast::<cef::sys::_cef_file_dialog_callback_t>()
            .wrap_result()
    }
    fn cancel_media(calls: Arc<AtomicU64>) -> MediaAccessCallback {
        // SAFETY: same owned RcImpl/base construction as cancel_file above.
        let mut raw: cef::sys::_cef_media_access_callback_t = unsafe { std::mem::zeroed() };
        raw.cancel = Some(media_cancel);
        RcImpl::new(raw, calls)
            .cast::<cef::sys::_cef_media_access_callback_t>()
            .wrap_result()
    }
    fn deny_permission(calls: Arc<AtomicU64>) -> PermissionPromptCallback {
        // SAFETY: same owned RcImpl/base construction as cancel_file above.
        let mut raw: cef::sys::_cef_permission_prompt_callback_t = unsafe { std::mem::zeroed() };
        raw.cont = Some(permission_deny);
        RcImpl::new(raw, calls)
            .cast::<cef::sys::_cef_permission_prompt_callback_t>()
            .wrap_result()
    }
    #[test]
    fn js_and_download_results_never_choose_a_default_modal_or_unanswered_callback() {
        let state = state();
        let js = RuntimeJsDialogs::new(state.clone());
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
        let downloads = RuntimeDownloads::new(state);
        assert_eq!(downloads.can_download(None, None, None), 0);
        // Runtime fixes ALLOY, whose documented false path cancels.
        assert_eq!(downloads.on_before_download(None, None, None, None), 0);
    }
    #[test]
    fn file_and_permission_callbacks_use_the_explicit_cancel_and_deny_routes() {
        let state = state();
        let count = Arc::new(AtomicU64::new(0));
        let mut file = cancel_file(count.clone());
        assert_eq!(
            RuntimeFileDialogs::new(state.clone()).on_file_dialog(
                None,
                FileDialogMode::OPEN,
                None,
                None,
                None,
                None,
                None,
                Some(&mut file)
            ),
            1
        );
        let permissions = RuntimePermissions::new(state);
        let mut media = cancel_media(count.clone());
        assert_eq!(
            permissions.on_request_media_access_permission(None, None, None, 0, Some(&mut media)),
            1
        );
        let mut prompt = deny_permission(count.clone());
        assert_eq!(
            permissions.on_show_permission_prompt(None, 1, None, 0, Some(&mut prompt)),
            1
        );
        assert_eq!(count.load(Ordering::SeqCst), 3);
        drop((file, media, prompt));
        assert_eq!(Arc::strong_count(&count), 1);
    }

    #[test]
    fn opt_in_download_enables_can_download_while_unset_denies_with_policy_count() {
        use rust_cef_core::host_capabilities::{
            DownloadProgress, DownloadRequest, DownloadService, HostCapabilities,
        };
        struct DummyDl;
        impl DownloadService for DummyDl {
            fn on_before_download(&self, _: DownloadRequest) {}
            fn on_download_updated(&self, _: DownloadProgress) {}
        }
        let s = state();
        let downloads = RuntimeDownloads::new(s.clone());
        assert_eq!(downloads.can_download(None, None, None), 0);
        assert_eq!(s.policy.snapshot().0[PolicyAction::Download as usize], 1);

        s.policy.set_host_capabilities(HostCapabilities {
            download: Some(Arc::new(DummyDl)),
            ..Default::default()
        });
        assert_eq!(downloads.can_download(None, None, None), 1);
        assert_eq!(s.policy.snapshot().0[PolicyAction::Download as usize], 1);
    }
}
