//! Interactive desktop dialogs, OS file picker request tracking, credential
//! redaction helpers, and enhanced download UX helpers.

use std::{
    cell::RefCell,
    collections::BTreeSet,
    path::{Path, PathBuf},
    rc::Rc,
};

use gpui::{
    AnyElement, Context, Entity, EntityInputHandler, FocusHandle, InteractiveElement as _,
    IntoElement, MouseButton, MouseDownEvent, ParentElement as _, PathPromptOptions,
    StatefulInteractiveElement as _, Styled as _, Window, div, hsla, prelude::FluentBuilder as _,
    px, rgb,
};
use rust_cef_gpui::{
    contracts::BrowserId,
    host_capabilities::{
        AuthCredentials, AuthDecision, CertificateErrorRequest, ClientCertificateInfo, DownloadId,
        FileDialogDecision, FileDialogKind, HostCapabilityError, HostRequestId, PermissionKind,
        RedactedSecret, SingleUseLedger, validate_download_destination,
    },
};

use super::{
    model::{
        DownloadInfoView, PendingAuthModal, PendingBeforeUnloadModal, PendingCertKind,
        PendingCertModal, PendingPermissionModal,
    },
    text_input::{
        TextFieldLayoutCache, TextFieldRenderSpec, TextFieldTheme, render_masked_text_field_box,
        render_text_field_box,
    },
};

pub const MAX_FILE_DIALOG_PATHS: usize = 64;
const MAX_LEDGER_CAPACITY: usize = 128;

/// Single-use host-side guard tracking live `FileDialogService` requests
/// against duplicate settlement, async cancellation races, and tab closure.
pub struct FileDialogTracker {
    ledger: SingleUseLedger<HostRequestId>,
    active_order: Vec<HostRequestId>,
    cancelled: BTreeSet<HostRequestId>,
}

impl Default for FileDialogTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl FileDialogTracker {
    pub fn new() -> Self {
        Self {
            ledger: SingleUseLedger::new(MAX_LEDGER_CAPACITY),
            active_order: Vec::new(),
            cancelled: BTreeSet::new(),
        }
    }

    /// Registers a new file dialog request. Returns `false` if this ID was
    /// already cancelled before reaching the UI thread or already consumed.
    pub fn record_request(&mut self, id: HostRequestId) -> bool {
        if self.cancelled.remove(&id) {
            return false;
        }
        if self.ledger.insert(id).is_ok() {
            self.active_order.push(id);
            true
        } else {
            false
        }
    }

    /// Marks `id` as cancelled by CEF. Returns `true` if `id` was still active.
    pub fn cancel_request(&mut self, id: HostRequestId) -> bool {
        if self.ledger.take(&id).is_ok() {
            self.active_order.retain(|x| *x != id);
            true
        } else {
            self.cancelled.insert(id);
            false
        }
    }

    /// Consumes a live single-use `HostRequestId` before resolving CEF. Returns
    /// `false` if cancelled, closed, or already resolved.
    pub fn take_for_resolution(&mut self, id: HostRequestId) -> bool {
        if self.ledger.take(&id).is_ok() {
            self.active_order.retain(|x| *x != id);
            true
        } else {
            false
        }
    }

    /// Drops and invalidates any pending requests owned by `browser`.
    pub fn clear_for_browser(&mut self, browser: BrowserId) -> Vec<HostRequestId> {
        let mut removed = Vec::new();
        let mut kept = Vec::with_capacity(self.active_order.len());
        for id in self.active_order.drain(..) {
            if id.browser == browser {
                let _ = self.ledger.take(&id);
                removed.push(id);
            } else {
                kept.push(id);
            }
        }
        self.active_order = kept;
        self.cancelled.retain(|id| id.browser != browser);
        removed
    }

    #[allow(dead_code)]
    pub fn active_count(&self) -> usize {
        self.ledger.len()
    }
}

/// Builds platform `PathPromptOptions` keeping `prompt: None` as required by the
/// GPUI file dialog backend.
pub fn build_path_prompt_options(kind: FileDialogKind) -> PathPromptOptions {
    match kind {
        FileDialogKind::Open => PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        },
        FileDialogKind::OpenMultiple => PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        },
        FileDialogKind::OpenFolder => PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        },
        FileDialogKind::Save => PathPromptOptions {
            files: false,
            directories: false,
            multiple: false,
            prompt: None,
        },
    }
}

pub fn split_save_default_path(raw: &str, fallback_dir: &Path) -> (PathBuf, Option<String>) {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains('\0') {
        return (fallback_dir.to_path_buf(), None);
    }
    let path = Path::new(trimmed);
    let suggested = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && !s.contains(':') && *s != "." && *s != "..")
        .map(ToOwned::to_owned);
    let dir = path
        .parent()
        .filter(|p| p.is_absolute() && p.is_dir())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| fallback_dir.to_path_buf());
    (dir, suggested)
}

/// Normalizes OS picker path lists to single-item or `MAX_FILE_DIALOG_PATHS`.
pub fn cap_selected_paths(paths: Vec<PathBuf>, kind: FileDialogKind) -> FileDialogDecision {
    if paths.is_empty() {
        return FileDialogDecision::Cancel;
    }
    let limit = match kind {
        FileDialogKind::Open | FileDialogKind::OpenFolder | FileDialogKind::Save => 1,
        FileDialogKind::OpenMultiple => MAX_FILE_DIALOG_PATHS,
    };
    let bounded: Vec<PathBuf> = paths.into_iter().take(limit).collect();
    if bounded.is_empty() {
        FileDialogDecision::Cancel
    } else {
        FileDialogDecision::Accept(bounded)
    }
}

pub fn format_permission_flags(kind: PermissionKind, mask: u32) -> String {
    if mask == 0 {
        return match kind {
            PermissionKind::MediaAccess => "Unspecified media access".into(),
            PermissionKind::PermissionPrompt => "Unspecified site permission".into(),
        };
    }
    let mut parts: Vec<&'static str> = Vec::new();
    match kind {
        PermissionKind::MediaAccess => {
            if mask & (1 << 0) != 0 {
                parts.push("Audio Capture (Microphone)");
            }
            if mask & (1 << 1) != 0 {
                parts.push("Video Capture (Camera)");
            }
            if mask & (1 << 2) != 0 {
                parts.push("Desktop Audio Capture");
            }
            if mask & (1 << 3) != 0 {
                parts.push("Desktop Video Capture");
            }
        }
        PermissionKind::PermissionPrompt => {
            if mask & (1 << 0) != 0 {
                parts.push("Accessibility");
            }
            if mask & (1 << 6) != 0 {
                parts.push("Camera");
            }
            if mask & (1 << 7) != 0 {
                parts.push("Microphone");
            }
            if mask & (1 << 8) != 0 {
                parts.push("Clipboard Read/Write");
            }
            if mask & (1 << 10) != 0 {
                parts.push("Geolocation");
            }
            if mask & (1 << 14) != 0 {
                parts.push("Notifications");
            }
        }
    }
    if parts.is_empty() {
        format!("Permission mask 0x{mask:08x}")
    } else {
        format!("{} (0x{mask:08x})", parts.join(", "))
    }
}

/// Packages HTTP/Proxy authentication inputs into `RedactedSecret` fields and
/// wipes all temporary buffer bytes immediately before returning.
pub fn build_auth_decision_and_wipe(modal: &mut PendingAuthModal, submit: bool) -> AuthDecision {
    if !submit {
        modal.wipe_inputs();
        return AuthDecision::Cancel;
    }
    let creds = AuthCredentials {
        username: RedactedSecret::new(modal.username_input.text.clone()),
        password: RedactedSecret::new(modal.password_input.text.clone()),
    };
    modal.wipe_inputs();
    AuthDecision::Continue(creds)
}

#[allow(dead_code)]
pub fn server_cert_summary(req: &CertificateErrorRequest) -> String {
    format!("URL: {} | Error code: {}", req.request_url, req.cert_error)
}

pub fn client_cert_row_label(info: &ClientCertificateInfo) -> String {
    format!(
        "[{}] {} — Issuer: {}",
        info.index, info.subject, info.issuer
    )
}

pub fn before_unload_copy(is_reload: bool) -> (&'static str, &'static str) {
    if is_reload {
        (
            "Confirm Page Reload",
            "This page asks whether you want to reload. Unsaved changes may be lost.",
        )
    } else {
        (
            "Confirm Leave Tab",
            "This page asks whether you want to leave. Unsaved changes may be lost.",
        )
    }
}

pub fn sanitize_download_filename(suggested: &str) -> String {
    Path::new(suggested)
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::trim)
        .filter(|s| {
            !s.is_empty() && !s.contains('\0') && !s.contains(':') && *s != "." && *s != ".."
        })
        .unwrap_or("download.bin")
        .to_owned()
}

pub fn resolve_download_destination(
    base_dir: &Path,
    seq: u64,
    suggested_name: &str,
    user_chosen_path: Option<PathBuf>,
    forbidden_roots: &[PathBuf],
) -> Result<PathBuf, HostCapabilityError> {
    let candidate = match user_chosen_path {
        Some(chosen) => chosen,
        None => {
            let clean = sanitize_download_filename(suggested_name);
            base_dir.join(format!("{seq}-{clean}"))
        }
    };
    validate_download_destination(&candidate, forbidden_roots)
}

pub fn cancel_active_download_on_close(
    dl: &mut DownloadInfoView,
    browser_id: BrowserId,
) -> Option<DownloadId> {
    if dl.browser_id != Some(browser_id) || dl.is_complete || dl.is_cancelled {
        return None;
    }
    let id = dl.download_id?;
    dl.is_in_progress = false;
    dl.is_cancelled = true;
    dl.cancelled_reason = Some("Tab closed".into());
    dl.last_action = "Cancelled".into();
    Some(id)
}

fn dialog_btn<V: EntityInputHandler>(
    id: &'static str,
    label: &'static str,
    primary: bool,
    on_click: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    cx: &mut Context<V>,
) -> AnyElement {
    let bg = if primary {
        rgb(0x5e35b1)
    } else {
        rgb(0xeeeeee)
    };
    let fg = if primary {
        rgb(0xffffff)
    } else {
        rgb(0x212121)
    };
    let border = if primary {
        rgb(0x5e35b1)
    } else {
        rgb(0xbdbdbd)
    };
    let hover_bg = if primary {
        rgb(0x512da8)
    } else {
        rgb(0xe0e0e0)
    };
    div()
        .id(id)
        .px(px(14.0))
        .py(px(5.0))
        .bg(bg)
        .text_color(fg)
        .border_1()
        .border_color(border)
        .rounded(px(3.0))
        .text_size(px(12.0))
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                on_click(this, window, cx);
            }),
        )
        .child(label)
        .into_any_element()
}

fn modal_shell<V: EntityInputHandler>(
    backdrop_id: &'static str,
    title: impl Into<String>,
    width_px: f32,
    body: AnyElement,
    footer_buttons: Vec<AnyElement>,
    cx: &mut Context<V>,
) -> AnyElement {
    div()
        .id(backdrop_id)
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .bottom_0()
        .bg(hsla(0.0, 0.0, 0.0, 0.40))
        .flex()
        .items_center()
        .justify_center()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|_, _, _, cx| cx.stop_propagation()),
        )
        .child(
            div()
                .w(px(width_px))
                .bg(rgb(0xffffff))
                .border_1()
                .border_color(rgb(0xbdbdbd))
                .rounded(px(4.0))
                .shadow_lg()
                .overflow_hidden()
                .flex()
                .flex_col()
                .child(
                    div()
                        .px(px(14.0))
                        .py(px(8.0))
                        .bg(rgb(0x5e35b1))
                        .text_color(rgb(0xffffff))
                        .text_size(px(13.0))
                        .child(title.into()),
                )
                .child(div().p(px(14.0)).child(body))
                .child(
                    div()
                        .px(px(14.0))
                        .pb(px(12.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .children(footer_buttons),
                ),
        )
        .into_any_element()
}

pub(crate) fn render_permission_modal<V: EntityInputHandler>(
    modal: &PendingPermissionModal,
    on_allow: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    on_deny: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    cx: &mut Context<V>,
) -> AnyElement {
    let kind_label = match modal.request.kind {
        PermissionKind::MediaAccess => "Media Access Request",
        PermissionKind::PermissionPrompt => "Site Permission Request",
    };
    let flags = format_permission_flags(modal.request.kind, modal.request.requested_permissions);
    let body = div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .text_size(px(12.0))
        .text_color(rgb(0x212121))
        .child(format!("Origin: {}", modal.request.requesting_origin))
        .child(format!("Requested: {flags}"))
        .into_any_element();
    modal_shell(
        "permission-modal-backdrop",
        kind_label,
        460.0,
        body,
        vec![
            dialog_btn("perm-deny-btn", "Deny", false, on_deny, cx),
            dialog_btn("perm-allow-btn", "Allow", true, on_allow, cx),
        ],
        cx,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_auth_modal<V: EntityInputHandler>(
    modal: &PendingAuthModal,
    user_focused: bool,
    pass_focused: bool,
    host: Entity<V>,
    focus_handle: FocusHandle,
    user_cache: Rc<RefCell<TextFieldLayoutCache>>,
    pass_cache: Rc<RefCell<TextFieldLayoutCache>>,
    on_focus_user: impl Fn(&mut V, &MouseDownEvent, &mut Window, &mut Context<V>) + 'static,
    on_focus_pass: impl Fn(&mut V, &MouseDownEvent, &mut Window, &mut Context<V>) + 'static,
    on_submit: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    on_cancel: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    cx: &mut Context<V>,
) -> AnyElement {
    let scope_kind = if modal.request.is_proxy {
        "Proxy"
    } else {
        "Origin Server"
    };
    let title = format!("Authentication Required ({scope_kind})");
    let body = div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .text_size(px(12.0))
        .text_color(rgb(0x212121))
        .child(format!(
            "{} {}:{}  (scheme: {}, realm: {})",
            scope_kind,
            modal.request.host,
            modal.request.port,
            modal.request.scheme,
            if modal.request.realm.is_empty() {
                "<default>"
            } else {
                &modal.request.realm
            }
        ))
        .child(div().text_color(rgb(0x424242)).child("Username:"))
        .child(render_text_field_box(
            TextFieldRenderSpec {
                id: "auth-username-input",
                host: host.clone(),
                focus_handle: focus_handle.clone(),
                buffer: &modal.username_input,
                cache: user_cache,
                placeholder: "Username",
                focused: user_focused,
                multiline: false,
                font_size_px: 12.0,
                pad_x_px: 8.0,
                pad_y_px: 4.0,
                height_px: Some(28.0),
                rounded_px: 3.0,
                flex_1: false,
                theme: TextFieldTheme::example_light(),
            },
            on_focus_user,
            cx,
        ))
        .child(div().text_color(rgb(0x424242)).child("Password:"))
        .child(render_masked_text_field_box(
            TextFieldRenderSpec {
                id: "auth-password-input",
                host,
                focus_handle,
                buffer: &modal.password_input,
                cache: pass_cache,
                placeholder: "Password",
                focused: pass_focused,
                multiline: false,
                font_size_px: 12.0,
                pad_x_px: 8.0,
                pad_y_px: 4.0,
                height_px: Some(28.0),
                rounded_px: 3.0,
                flex_1: false,
                theme: TextFieldTheme::example_light(),
            },
            on_focus_pass,
            cx,
        ))
        .into_any_element();

    modal_shell(
        "auth-modal-backdrop",
        title,
        460.0,
        body,
        vec![
            dialog_btn("auth-cancel-btn", "Cancel", false, on_cancel, cx),
            dialog_btn("auth-submit-btn", "Sign In", true, on_submit, cx),
        ],
        cx,
    )
}

pub(crate) fn render_certificate_modal<V: EntityInputHandler>(
    modal: &PendingCertModal,
    selected_index: Option<usize>,
    on_select_cert: impl Fn(&mut V, usize, &mut Window, &mut Context<V>) + Clone + 'static,
    on_accept: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    on_cancel: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    cx: &mut Context<V>,
) -> AnyElement {
    match &modal.kind {
        PendingCertKind::ServerError(err) => {
            let body = div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .text_size(px(12.0))
                .text_color(rgb(0x212121))
                .child(format!("Request URL: {}", err.request_url))
                .child(format!("Certificate error code: {}", err.cert_error))
                .into_any_element();
            modal_shell(
                "cert-modal-backdrop",
                "TLS Certificate Warning",
                500.0,
                body,
                vec![
                    dialog_btn("cert-cancel-btn", "Cancel", false, on_cancel, cx),
                    dialog_btn("cert-proceed-btn", "Proceed", true, on_accept, cx),
                ],
                cx,
            )
        }
        PendingCertKind::ClientSelect(req) => {
            let active_idx = selected_index
                .filter(|idx| req.certificates.iter().any(|c| c.index == *idx))
                .or_else(|| req.certificates.first().map(|c| c.index));
            let rows = req.certificates.iter().enumerate().map(|(i, cert)| {
                let idx = cert.index;
                let selected = active_idx == Some(idx);
                let cb = on_select_cert.clone();
                div()
                    .id(("client-cert-row", i))
                    .p(px(6.0))
                    .rounded(px(3.0))
                    .border_1()
                    .border_color(if selected {
                        rgb(0x5e35b1)
                    } else {
                        rgb(0xd6d6d6)
                    })
                    .bg(if selected {
                        rgb(0xede7f6)
                    } else {
                        rgb(0xfafafa)
                    })
                    .text_size(px(11.0))
                    .text_color(rgb(0x212121))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            cb(this, idx, window, cx);
                        }),
                    )
                    .child(client_cert_row_label(cert))
            });
            let body = div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .text_size(px(12.0))
                .text_color(rgb(0x212121))
                .child(format!(
                    "{} {}:{}",
                    if req.is_proxy { "Proxy" } else { "Origin" },
                    req.host,
                    req.port
                ))
                .child(
                    div()
                        .id("client-cert-list")
                        .max_h(px(180.0))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .when(req.certificates.is_empty(), |d| {
                            d.child("No client certificates available.")
                        })
                        .children(rows),
                )
                .into_any_element();
            modal_shell(
                "cert-modal-backdrop",
                "Select Client Certificate",
                520.0,
                body,
                vec![
                    dialog_btn("cert-cancel-btn", "Cancel", false, on_cancel, cx),
                    dialog_btn("cert-accept-btn", "Select", true, on_accept, cx),
                ],
                cx,
            )
        }
    }
}

pub(crate) fn render_before_unload_modal<V: EntityInputHandler>(
    modal: &PendingBeforeUnloadModal,
    on_stay: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    on_leave: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    cx: &mut Context<V>,
) -> AnyElement {
    let (title, msg) = before_unload_copy(modal.is_reload);
    let body = div()
        .text_size(px(12.0))
        .text_color(rgb(0x212121))
        .child(msg)
        .into_any_element();
    modal_shell(
        "before-unload-modal-backdrop",
        title,
        440.0,
        body,
        vec![
            dialog_btn("before-unload-stay-btn", "Stay", false, on_stay, cx),
            dialog_btn("before-unload-leave-btn", "Leave", true, on_leave, cx),
        ],
        cx,
    )
}

#[cfg(test)]
mod tests {
    use super::super::model::TextBuffer;
    use super::*;
    use rust_cef_gpui::{
        contracts::RuntimeGeneration,
        host_capabilities::{AuthRequest, ClientCertificateRequest},
    };

    fn browser(local: u64) -> BrowserId {
        BrowserId::new(RuntimeGeneration::new(1), local)
    }

    mod file_dialog_tests {
        use super::*;

        #[test]
        fn file_dialog_tracker_enforces_single_use_cancellation_and_tab_cleanup() {
            let b1 = browser(1);
            let b2 = browser(2);
            let r1 = HostRequestId {
                browser: b1,
                sequence: 1,
            };
            let r2 = HostRequestId {
                browser: b1,
                sequence: 2,
            };
            let r3 = HostRequestId {
                browser: b2,
                sequence: 3,
            };

            let mut tracker = FileDialogTracker::new();
            assert!(tracker.record_request(r1));
            assert!(!tracker.record_request(r1));
            assert!(tracker.take_for_resolution(r1));
            // Single-use: second resolution attempt fails.
            assert!(!tracker.take_for_resolution(r1));

            // Stale cancel blocks delayed picker resolution; pre-cancelled id refuses late arrival.
            assert!(tracker.record_request(r2));
            assert!(tracker.cancel_request(r2));
            assert!(!tracker.take_for_resolution(r2));

            let r_pre_cancel = HostRequestId {
                browser: b1,
                sequence: 99,
            };
            assert!(!tracker.cancel_request(r_pre_cancel));
            assert!(!tracker.record_request(r_pre_cancel));

            // Closing browser b2 invalidates remaining active requests for b2.
            assert!(tracker.record_request(r3));
            assert_eq!(tracker.clear_for_browser(b2), vec![r3]);
            assert!(!tracker.take_for_resolution(r3));
            assert_eq!(tracker.active_count(), 0);
        }

        #[test]
        fn path_prompt_options_keep_prompt_none_and_cap_paths() {
            for kind in [
                FileDialogKind::Open,
                FileDialogKind::OpenMultiple,
                FileDialogKind::OpenFolder,
                FileDialogKind::Save,
            ] {
                let opts = build_path_prompt_options(kind);
                assert!(opts.prompt.is_none());
            }

            let over: Vec<PathBuf> = (0..(MAX_FILE_DIALOG_PATHS + 10))
                .map(|i| PathBuf::from(format!("C:\\temp\\file_{i}.txt")))
                .collect();
            assert_eq!(
                cap_selected_paths(Vec::new(), FileDialogKind::Open),
                FileDialogDecision::Cancel
            );
            match cap_selected_paths(over.clone(), FileDialogKind::Open) {
                FileDialogDecision::Accept(paths) => assert_eq!(paths.len(), 1),
                FileDialogDecision::Cancel => panic!("expected Accept"),
            }
            match cap_selected_paths(over, FileDialogKind::OpenMultiple) {
                FileDialogDecision::Accept(paths) => assert_eq!(paths.len(), MAX_FILE_DIALOG_PATHS),
                FileDialogDecision::Cancel => panic!("expected Accept"),
            }
        }
    }

    mod dialogs_tests {
        use super::*;

        #[test]
        fn auth_decision_wraps_redacted_secrets_and_wipes_buffers() {
            let b = browser(4);
            let mut modal = PendingAuthModal {
                tab_id: 1,
                browser_id: b,
                request: AuthRequest {
                    id: HostRequestId {
                        browser: b,
                        sequence: 7,
                    },
                    origin_url: "https://internal.test".into(),
                    is_proxy: true,
                    host: "proxy.test".into(),
                    port: 8080,
                    realm: "Corp".into(),
                    scheme: "basic".into(),
                },
                username_input: TextBuffer::new("secret_user"),
                password_input: TextBuffer::new("hunter2_secret"),
            };

            let dbg = format!("{modal:?}");
            assert!(!dbg.contains("secret_user") && !dbg.contains("hunter2_secret"));

            let decision = build_auth_decision_and_wipe(&mut modal, true);
            assert!(modal.username_input.text.is_empty());
            assert!(modal.password_input.text.is_empty());

            let decision_dbg = format!("{decision:?}");
            assert!(
                !decision_dbg.contains("secret_user") && !decision_dbg.contains("hunter2_secret")
            );
            match decision {
                AuthDecision::Continue(creds) => {
                    assert_eq!(creds.username.expose(), "secret_user");
                    assert_eq!(creds.password.expose(), "hunter2_secret");
                }
                AuthDecision::Cancel => panic!("expected Continue"),
            }

            let mut cancel_modal = PendingAuthModal {
                tab_id: 1,
                browser_id: b,
                request: modal.request.clone(),
                username_input: TextBuffer::new("u2"),
                password_input: TextBuffer::new("p2"),
            };
            assert_eq!(
                build_auth_decision_and_wipe(&mut cancel_modal, false),
                AuthDecision::Cancel
            );
            assert!(cancel_modal.username_input.text.is_empty());
            assert!(cancel_modal.password_input.text.is_empty());
        }

        #[test]
        fn permission_certificate_and_before_unload_helpers_format_exposed_fields() {
            let media = format_permission_flags(PermissionKind::MediaAccess, 0b11);
            assert!(media.contains("Audio Capture") && media.contains("Video Capture"));

            let err_summary = server_cert_summary(&CertificateErrorRequest {
                id: HostRequestId {
                    browser: browser(1),
                    sequence: 1,
                },
                cert_error: -202,
                request_url: "https://self-signed.example".into(),
            });
            assert!(
                err_summary.contains("https://self-signed.example") && err_summary.contains("-202")
            );

            let client_req = ClientCertificateRequest {
                id: HostRequestId {
                    browser: browser(1),
                    sequence: 2,
                },
                is_proxy: false,
                host: "mtls.example".into(),
                port: 443,
                certificates: vec![ClientCertificateInfo {
                    index: 0,
                    subject: "CN=Alice".into(),
                    issuer: "CN=ExampleCA".into(),
                }],
            };
            let row = client_cert_row_label(&client_req.certificates[0]);
            assert!(
                row.contains("[0]") && row.contains("CN=Alice") && row.contains("CN=ExampleCA")
            );

            let (reload_title, _) = before_unload_copy(true);
            let (leave_title, _) = before_unload_copy(false);
            assert!(reload_title.contains("Reload"));
            assert!(leave_title.contains("Leave"));
        }
    }

    mod download_tests {
        use super::*;

        #[test]
        fn download_destination_validates_bounds_and_cancels_on_tab_close() {
            let temp_root = std::env::temp_dir().join("gpui_cef_example_dl_test");
            std::fs::create_dir_all(&temp_root).unwrap();
            let forbidden = temp_root.join("forbidden");
            std::fs::create_dir_all(&forbidden).unwrap();

            // Traversal/colon characters are stripped from default suggestions.
            let auto_dest = resolve_download_destination(
                &temp_root,
                5,
                "../../evil:stream.txt",
                None,
                std::slice::from_ref(&forbidden),
            )
            .unwrap();
            assert!(auto_dest.ends_with("5-download.bin"));

            let ok_dest = resolve_download_destination(
                &temp_root,
                6,
                "report.txt",
                None,
                std::slice::from_ref(&forbidden),
            )
            .unwrap();
            assert!(ok_dest.ends_with("6-report.txt"));

            // Forbidden destination via Ask Save As is rejected.
            assert!(
                resolve_download_destination(
                    &temp_root,
                    7,
                    "x.txt",
                    Some(forbidden.join("secret.txt")),
                    std::slice::from_ref(&forbidden),
                )
                .is_err()
            );

            // Deterministic cancellation on tab close for active downloads only.
            let b = browser(8);
            let dl_id = DownloadId {
                browser: b,
                cef_id: 1,
                sequence: 11,
            };
            let mut active = DownloadInfoView {
                browser_id: Some(b),
                download_id: Some(dl_id),
                id: Some(11),
                is_in_progress: true,
                is_complete: false,
                is_cancelled: false,
                ..DownloadInfoView::default()
            };
            assert_eq!(cancel_active_download_on_close(&mut active, b), Some(dl_id));
            assert!(active.is_cancelled && !active.is_in_progress);
            assert_eq!(active.cancelled_reason.as_deref(), Some("Tab closed"));
            // Already cancelled download does not trigger duplicate cancel.
            assert_eq!(cancel_active_download_on_close(&mut active, b), None);

            let _ = std::fs::remove_dir_all(&temp_root);
        }
    }
}
