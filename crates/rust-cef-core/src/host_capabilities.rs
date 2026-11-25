//! Pure core service ports, single-use tokens, credential/cookie redaction,
//! destination confinement, preference guards, and bounded CDP validation.
//!
//! Structural reference (`D:\System Dir\Downloads\CefSharp`, rev `41919703`):
//! - `IDownloadHandler.cs:23,36,45`
//! - `IDialogHandler.cs:44`
//! - `IJsDialogHandler.cs:25,39`
//! - `IPermissionHandler.cs:30,47`
//! - `ILifeSpanHandler.cs:47`
//! - `IRequestHandler.cs:89,107,121`
//! - `ICookieManager.cs:12,25,37,44`
//! - `ICookieAccessFilter.cs:24`

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::contracts::{BrowserId, BrowserKeyEvent, FocusRequestSource};
use crate::js_bridge::{MAX_JS_JSON_DEPTH, validate_bounded_json};

pub const MAX_CDP_JSON_BYTES: usize = 512 * 1024;
pub const MAX_CDP_METHOD_BYTES: usize = 128;
pub const MAX_FILE_DIALOG_PATHS: usize = 64;
pub const MAX_PREFERENCE_NAME_BYTES: usize = 128;
pub const MAX_PREFERENCE_STRING_BYTES: usize = 4096;
pub const MAX_PREFERENCE_LIST_ITEMS: usize = 64;
pub const MAX_HOST_REQUEST_LEDGER_ENTRIES: usize = 256;
pub const MAX_COOKIE_RECORDS_PER_VISIT: usize = 4096;

#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

/// Redacted credential or cookie secret wrapper. `Debug` always prints
/// `"[REDACTED]"` and never emits raw contents or byte lengths.
#[derive(Clone, PartialEq, Eq)]
pub struct RedactedSecret(String);

impl RedactedSecret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn empty() -> Self {
        Self(String::new())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for RedactedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostRequestId {
    pub browser: BrowserId,
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DownloadId {
    pub browser: BrowserId,
    pub cef_id: u32,
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CookieRequestId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PdfExportId {
    pub browser: BrowserId,
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CdpMethodId {
    pub browser: BrowserId,
    pub message_id: i32,
}

#[derive(Clone, PartialEq, Eq)]
pub enum HostCapabilityError {
    WrongRuntime,
    BrowserClosed,
    NotReady,
    CapabilityDisabled,
    QueueFull,
    UnknownOrStaleToken,
    AlreadyConsumed,
    InvalidDestination,
    ForbiddenDestination,
    PreferenceBlocked,
    InvalidPreferenceValue,
    InvalidCdpPayload,
    CdpPayloadTooLarge,
    PostFailed,
    OperationFailed,
    Cancelled,
}

impl fmt::Debug for HostCapabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongRuntime => f.write_str("WrongRuntime"),
            Self::BrowserClosed => f.write_str("BrowserClosed"),
            Self::NotReady => f.write_str("NotReady"),
            Self::CapabilityDisabled => f.write_str("CapabilityDisabled"),
            Self::QueueFull => f.write_str("QueueFull"),
            Self::UnknownOrStaleToken => f.write_str("UnknownOrStaleToken"),
            Self::AlreadyConsumed => f.write_str("AlreadyConsumed"),
            Self::InvalidDestination => f.write_str("InvalidDestination"),
            Self::ForbiddenDestination => f.write_str("ForbiddenDestination"),
            Self::PreferenceBlocked => f.write_str("PreferenceBlocked"),
            Self::InvalidPreferenceValue => f.write_str("InvalidPreferenceValue"),
            Self::InvalidCdpPayload => f.write_str("InvalidCdpPayload"),
            Self::CdpPayloadTooLarge => f.write_str("CdpPayloadTooLarge"),
            Self::PostFailed => f.write_str("PostFailed"),
            Self::OperationFailed => f.write_str("OperationFailed"),
            Self::Cancelled => f.write_str("Cancelled"),
        }
    }
}

impl fmt::Display for HostCapabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for HostCapabilityError {}

/// Bounded, single-use request-token ledger guarding against duplicate
/// consumption or stale replies.
#[derive(Debug)]
pub struct SingleUseLedger<K: Ord + Copy> {
    capacity: usize,
    active: BTreeSet<K>,
    consumed: BTreeSet<K>,
}

impl<K: Ord + Copy> SingleUseLedger<K> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.clamp(1, MAX_HOST_REQUEST_LEDGER_ENTRIES),
            active: BTreeSet::new(),
            consumed: BTreeSet::new(),
        }
    }

    pub fn insert(&mut self, key: K) -> Result<(), HostCapabilityError> {
        if self.active.contains(&key) || self.consumed.contains(&key) {
            return Err(HostCapabilityError::AlreadyConsumed);
        }
        if self.active.len() >= self.capacity {
            return Err(HostCapabilityError::QueueFull);
        }
        self.active.insert(key);
        Ok(())
    }

    pub fn take(&mut self, key: &K) -> Result<(), HostCapabilityError> {
        if self.active.remove(key) {
            if self.consumed.len() >= 1024 {
                if let Some(oldest) = self.consumed.first().copied() {
                    self.consumed.remove(&oldest);
                }
            }
            self.consumed.insert(*key);
            return Ok(());
        }
        if self.consumed.contains(key) {
            Err(HostCapabilityError::AlreadyConsumed)
        } else {
            Err(HostCapabilityError::UnknownOrStaleToken)
        }
    }

    pub fn contains(&self, key: &K) -> bool {
        self.active.contains(key)
    }

    pub fn len(&self) -> usize {
        self.active.len()
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    pub fn clear(&mut self) -> Vec<K> {
        let keys: Vec<K> = std::mem::take(&mut self.active).into_iter().collect();
        for &k in &keys {
            if self.consumed.len() >= 1024 {
                if let Some(oldest) = self.consumed.first().copied() {
                    self.consumed.remove(&oldest);
                }
            }
            self.consumed.insert(k);
        }
        keys
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadRequest {
    pub id: DownloadId,
    pub url: String,
    pub original_url: String,
    pub suggested_name: String,
    pub mime_type: String,
    pub total_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadState {
    InProgress,
    Complete,
    Canceled,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadProgress {
    pub id: DownloadId,
    pub state: DownloadState,
    pub received_bytes: u64,
    pub total_bytes: Option<u64>,
    pub current_speed: i64,
    pub percent_complete: i32,
    pub full_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileDialogKind {
    Open,
    OpenMultiple,
    OpenFolder,
    Save,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDialogRequest {
    pub id: HostRequestId,
    pub mode: FileDialogKind,
    pub title: String,
    pub default_file_path: String,
    pub accept_filters: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileDialogDecision {
    Accept(Vec<PathBuf>),
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsDialogKind {
    Alert,
    Confirm,
    Prompt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsDialogRequest {
    pub id: HostRequestId,
    pub origin_url: String,
    pub kind: JsDialogKind,
    pub message_text: String,
    pub default_prompt_text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JsDialogDecision {
    Accept { prompt_input: Option<String> },
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionKind {
    MediaAccess,
    PermissionPrompt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionRequest {
    pub id: HostRequestId,
    pub kind: PermissionKind,
    pub requesting_origin: String,
    pub requested_permissions: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow { media_mask: u32 },
    Deny,
    Dismiss,
    Ignore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopupDisposition {
    CurrentTab,
    SingletonTab,
    NewForegroundTab,
    NewBackgroundTab,
    NewPopup,
    NewWindow,
    SaveToDisk,
    OffTheRecord,
    IgnoreAction,
    Unknown(i32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PopupRequest {
    pub browser: BrowserId,
    pub target_url: String,
    pub target_frame_name: String,
    pub disposition: PopupDisposition,
    pub user_gesture: bool,
    pub policy_allowed: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum PopupPolicy {
    /// Headless unattached `Runtime` defaults to `Deny`; attaching to a UI view
    /// (`Runtime::attach_browser` -> `CefBrowser` / `NativeCefBrowser`) upgrades
    /// `Auto` to `AllowNativePopup` (CefSharp default).
    #[default]
    Auto = 0,
    /// Cancel native popup creation (`OnBeforePopup` returns `1` / `true`).
    Deny = 1,
    /// Open a managed native popup `HWND` (`SetAsPopup`, tracked & closed with parent/Runtime).
    AllowNativePopup = 2,
    /// Cancel native popup `HWND` creation and navigate the opener browser's main frame to `target_url`.
    NavigateCurrentTab = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopupDecision {
    Cancel,
    AllowNativePopup { no_javascript_access: bool },
    NavigateCurrentTab,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavigationDecision {
    Allow,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NavigationRequest {
    pub browser: BrowserId,
    pub url: String,
    pub method: String,
    pub is_main_frame: bool,
    pub is_redirect: bool,
    pub user_gesture: bool,
    pub transition_type: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DragEnterInfo {
    pub browser: BrowserId,
    pub is_link: bool,
    pub is_fragment: bool,
    pub is_file: bool,
    pub link_url: String,
    pub mask: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragEnterDecision {
    Allow,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusDecision {
    Allow,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreKeyDecision {
    PassThrough,
    PassAsShortcut,
    Handled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyEventDecision {
    PassThrough,
    Handled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolExecutionDecision {
    Deny,
    AllowOsExecution,
}

pub type PopupFilter = Arc<dyn Fn(&PopupRequest) -> PopupDecision + Send + Sync>;
pub type NavigationFilter = Arc<dyn Fn(&NavigationRequest) -> NavigationDecision + Send + Sync>;
pub type DragEnterFilter = Arc<dyn Fn(&DragEnterInfo) -> DragEnterDecision + Send + Sync>;
pub type SetFocusFilter = Arc<dyn Fn(BrowserId, FocusRequestSource) -> FocusDecision + Send + Sync>;
pub type PreKeyFilter = Arc<dyn Fn(BrowserId, &BrowserKeyEvent) -> PreKeyDecision + Send + Sync>;
pub type KeyEventFilter = Arc<dyn Fn(BrowserId, &BrowserKeyEvent) -> KeyEventDecision + Send + Sync>;
pub type ProtocolExecutionFilter =
    Arc<dyn Fn(BrowserId, &str) -> ProtocolExecutionDecision + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthRequest {
    pub id: HostRequestId,
    pub origin_url: String,
    pub is_proxy: bool,
    pub host: String,
    pub port: i32,
    pub realm: String,
    pub scheme: String,
}

#[derive(Clone, PartialEq, Eq)]
pub struct AuthCredentials {
    pub username: RedactedSecret,
    pub password: RedactedSecret,
}

impl fmt::Debug for AuthCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthCredentials")
            .field("username", &self.username)
            .field("password", &self.password)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthDecision {
    Continue(AuthCredentials),
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertificateErrorRequest {
    pub id: HostRequestId,
    pub cert_error: i32,
    pub request_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientCertificateInfo {
    pub index: usize,
    pub subject: String,
    pub issuer: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientCertificateRequest {
    pub id: HostRequestId,
    pub is_proxy: bool,
    pub host: String,
    pub port: i32,
    pub certificates: Vec<ClientCertificateInfo>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertificateDecision {
    /// Allow the invalid server certificate, or select certificate at `Some(idx)` / none for client cert.
    Accept {
        certificate_index: Option<usize>,
    },
    Cancel,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CookieSameSiteKind {
    #[default]
    Unspecified,
    NoRestriction,
    Lax,
    Strict,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CookiePriorityKind {
    Low,
    #[default]
    Medium,
    High,
}

#[derive(Clone, PartialEq, Eq)]
pub struct CookieRecord {
    pub name: String,
    pub value: RedactedSecret,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: CookieSameSiteKind,
    pub priority: CookiePriorityKind,
}

impl fmt::Debug for CookieRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CookieRecord")
            .field("name", &self.name)
            .field("value", &self.value)
            .field("domain", &self.domain)
            .field("path", &self.path)
            .field("secure", &self.secure)
            .field("http_only", &self.http_only)
            .finish()
    }
}

#[derive(Clone)]
pub struct CookieFilterRequest {
    pub browser: Option<BrowserId>,
    pub url: String,
    pub cookie_name: String,
    pub cookie_domain: String,
    pub cookie_path: String,
    pub is_save: bool,
}

impl fmt::Debug for CookieFilterRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CookieFilterRequest")
            .field("browser", &self.browser)
            .field("url", &self.url)
            .field("cookie_name", &self.cookie_name)
            .field("is_save", &self.is_save)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CookieFilterDecision {
    Allow,
    Block,
}

#[derive(Clone, PartialEq)]
pub enum PreferenceValue {
    Null,
    Bool(bool),
    Int(i32),
    Double(f64),
    String(String),
    StringList(Vec<String>),
}

impl fmt::Debug for PreferenceValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("Null"),
            Self::Bool(v) => f.debug_tuple("Bool").field(v).finish(),
            Self::Int(v) => f.debug_tuple("Int").field(v).finish(),
            Self::Double(v) => f.debug_tuple("Double").field(v).finish(),
            Self::String(s) => f.debug_struct("String").field("bytes", &s.len()).finish(),
            Self::StringList(v) => f.debug_struct("StringList").field("len", &v.len()).finish(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PdfExportOptions {
    pub landscape: bool,
    pub print_background: bool,
    pub scale: f64,
    pub paper_width: f64,
    pub paper_height: f64,
    pub prefer_css_page_size: bool,
    pub page_ranges: String,
    pub generate_document_outline: bool,
}

impl Default for PdfExportOptions {
    fn default() -> Self {
        Self {
            landscape: false,
            print_background: true,
            scale: 0.0,
            paper_width: 0.0,
            paper_height: 0.0,
            prefer_css_page_size: true,
            page_ranges: String::new(),
            generate_document_outline: false,
        }
    }
}

impl PdfExportOptions {
    pub fn validate(&self) -> Result<(), HostCapabilityError> {
        if !self.scale.is_finite()
            || !self.paper_width.is_finite()
            || !self.paper_height.is_finite()
            || self.scale < 0.0
            || self.paper_width < 0.0
            || self.paper_height < 0.0
            || self.page_ranges.len() > 256
            || self.page_ranges.bytes().any(|b| b < 0x20 || b == 0x7f)
        {
            return Err(HostCapabilityError::InvalidPreferenceValue);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PdfExportOutcome {
    pub id: PdfExportId,
    pub ok: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CdpMethodResult {
    pub id: CdpMethodId,
    pub success: bool,
    pub result_json: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CdpEvent {
    pub browser: BrowserId,
    pub method: String,
    pub params_json: String,
}

pub trait DownloadService: Send + Sync {
    fn on_before_download(&self, request: DownloadRequest);
    fn on_download_updated(&self, progress: DownloadProgress);
}

pub trait FileDialogService: Send + Sync {
    fn on_file_dialog(&self, request: FileDialogRequest);
    fn on_file_dialog_cancelled(&self, _id: HostRequestId) {}
}

pub trait JsDialogService: Send + Sync {
    fn on_js_dialog(&self, request: JsDialogRequest);
    fn on_js_dialog_cancelled(&self, _id: HostRequestId) {}
}

pub trait PermissionService: Send + Sync {
    fn on_permission_request(&self, request: PermissionRequest);
    fn on_permission_cancelled(&self, _id: HostRequestId) {}
}

pub trait PopupService: Send + Sync {
    fn on_popup_request(&self, request: PopupRequest) {
        let _ = request;
    }

    fn decide_popup(&self, request: PopupRequest) -> PopupDecision {
        self.on_popup_request(request);
        PopupDecision::Cancel
    }
}

pub trait AuthService: Send + Sync {
    fn on_auth_credentials(&self, request: AuthRequest);
    fn on_auth_cancelled(&self, _id: HostRequestId) {}
}

pub trait CertificateService: Send + Sync {
    fn on_certificate_error(&self, _request: CertificateErrorRequest) {}
    fn on_select_client_certificate(&self, _request: ClientCertificateRequest) {}
    fn on_certificate_cancelled(&self, _id: HostRequestId) {}
}

pub trait CookieAccessPolicy: Send + Sync {
    fn can_send_cookie(&self, request: &CookieFilterRequest) -> CookieFilterDecision;
    fn can_save_cookie(&self, request: &CookieFilterRequest) -> CookieFilterDecision;
}

pub trait CdpObserver: Send + Sync {
    fn on_method_result(&self, result: CdpMethodResult);
    fn on_event(&self, event: CdpEvent);
    fn on_attached(&self, _browser: BrowserId) {}
    fn on_detached(&self, _browser: BrowserId) {}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioStreamConfig {
    pub channel_layout: i32,
    pub sample_rate: i32,
    pub frames_per_buffer: i32,
}

/// Optional host audio PCM capture service mirroring CefSharp `IAudioHandler`
/// (`CefSharp/Handler/IAudioHandler.cs:14-72`). When absent in `HostCapabilities`,
/// CEF uses normal OS speaker output without attaching `CefAudioHandler`.
pub trait AudioStreamService: Send + Sync {
    fn configure_stream(
        &self,
        _browser: BrowserId,
        default_config: AudioStreamConfig,
    ) -> Option<AudioStreamConfig> {
        Some(default_config)
    }

    fn on_stream_started(&self, _browser: BrowserId, _config: AudioStreamConfig, _channels: usize) {}

    fn on_stream_packet(&self, _browser: BrowserId, _channels: &[Vec<f32>], _pts: i64) {}

    fn on_stream_stopped(&self, _browser: BrowserId) {}

    fn on_stream_error(&self, _browser: BrowserId, _message: String) {}
}

#[derive(Clone, Default)]
pub struct HostCapabilities {
    pub download: Option<Arc<dyn DownloadService>>,
    pub file_dialog: Option<Arc<dyn FileDialogService>>,
    pub js_dialog: Option<Arc<dyn JsDialogService>>,
    pub permission: Option<Arc<dyn PermissionService>>,
    pub popup: Option<Arc<dyn PopupService>>,
    pub auth: Option<Arc<dyn AuthService>>,
    pub certificate: Option<Arc<dyn CertificateService>>,
    pub cookie_access: Option<Arc<dyn CookieAccessPolicy>>,
    pub cdp_observer: Option<Arc<dyn CdpObserver>>,
    pub audio: Option<Arc<dyn AudioStreamService>>,
}

impl HostCapabilities {
    #[must_use]
    pub fn merge_fallback(&self, fallback: &Self) -> Self {
        Self {
            download: self.download.clone().or_else(|| fallback.download.clone()),
            file_dialog: self
                .file_dialog
                .clone()
                .or_else(|| fallback.file_dialog.clone()),
            js_dialog: self.js_dialog.clone().or_else(|| fallback.js_dialog.clone()),
            permission: self
                .permission
                .clone()
                .or_else(|| fallback.permission.clone()),
            popup: self.popup.clone().or_else(|| fallback.popup.clone()),
            auth: self.auth.clone().or_else(|| fallback.auth.clone()),
            certificate: self
                .certificate
                .clone()
                .or_else(|| fallback.certificate.clone()),
            cookie_access: self
                .cookie_access
                .clone()
                .or_else(|| fallback.cookie_access.clone()),
            cdp_observer: self
                .cdp_observer
                .clone()
                .or_else(|| fallback.cdp_observer.clone()),
            audio: self.audio.clone().or_else(|| fallback.audio.clone()),
        }
    }
}

impl fmt::Debug for HostCapabilities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostCapabilities")
            .field("download", &self.download.is_some())
            .field("file_dialog", &self.file_dialog.is_some())
            .field("js_dialog", &self.js_dialog.is_some())
            .field("permission", &self.permission.is_some())
            .field("popup", &self.popup.is_some())
            .field("auth", &self.auth.is_some())
            .field("certificate", &self.certificate.is_some())
            .field("cookie_access", &self.cookie_access.is_some())
            .field("cdp_observer", &self.cdp_observer.is_some())
            .field("audio", &self.audio.is_some())
            .finish()
    }
}

fn is_reparse_or_symlink(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
            return true;
        }
    }
    false
}

fn comparable_path_string(path: &Path) -> String {
    let s = path.to_string_lossy();
    let stripped = s.strip_prefix(r"\\?\").unwrap_or(&s);
    if cfg!(windows) {
        stripped.to_ascii_lowercase()
    } else {
        stripped.to_owned()
    }
}

fn path_within_root(candidate: &Path, root: &Path) -> bool {
    let cand_str = comparable_path_string(candidate);
    let root_str = comparable_path_string(root);
    let cand_trimmed = cand_str.trim_end_matches(['\\', '/']);
    let root_trimmed = root_str.trim_end_matches(['\\', '/']);
    if cand_trimmed == root_trimmed {
        return true;
    }
    let sep = if cfg!(windows) { '\\' } else { '/' };
    cand_trimmed
        .strip_prefix(root_trimmed)
        .is_some_and(|rest| rest.starts_with(sep))
}

fn validate_confined_output_file(
    path: &Path,
    forbidden_roots: &[PathBuf],
    require_pdf_ext: bool,
) -> Result<PathBuf, HostCapabilityError> {
    let raw = path.to_string_lossy();
    if raw.is_empty() || raw.contains('\0') || !path.is_absolute() {
        return Err(HostCapabilityError::InvalidDestination);
    }
    for component in path.components() {
        match component {
            Component::ParentDir | Component::CurDir => {
                return Err(HostCapabilityError::InvalidDestination);
            }
            Component::Normal(part) => {
                if part.to_string_lossy().contains(':') {
                    return Err(HostCapabilityError::InvalidDestination);
                }
            }
            Component::Prefix(_) | Component::RootDir => {}
        }
    }
    let Some(file_name) = path.file_name().filter(|n| !n.is_empty()) else {
        return Err(HostCapabilityError::InvalidDestination);
    };
    if require_pdf_ext {
        let is_pdf = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"));
        if !is_pdf {
            return Err(HostCapabilityError::InvalidDestination);
        }
    }
    let Some(parent) = path.parent() else {
        return Err(HostCapabilityError::InvalidDestination);
    };
    let parent_meta =
        fs::symlink_metadata(parent).map_err(|_| HostCapabilityError::InvalidDestination)?;
    if !parent_meta.is_dir() || is_reparse_or_symlink(&parent_meta) {
        return Err(HostCapabilityError::ForbiddenDestination);
    }
    let canon_parent =
        fs::canonicalize(parent).map_err(|_| HostCapabilityError::InvalidDestination)?;
    let canon_meta =
        fs::symlink_metadata(&canon_parent).map_err(|_| HostCapabilityError::InvalidDestination)?;
    if !canon_meta.is_dir() || is_reparse_or_symlink(&canon_meta) {
        return Err(HostCapabilityError::ForbiddenDestination);
    }
    let target = canon_parent.join(file_name);
    if let Ok(existing_meta) = fs::symlink_metadata(&target) {
        if existing_meta.is_dir() || is_reparse_or_symlink(&existing_meta) {
            return Err(HostCapabilityError::ForbiddenDestination);
        }
    }
    for root in forbidden_roots {
        let resolved_root = fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        if path_within_root(&canon_parent, &resolved_root)
            || path_within_root(&target, &resolved_root)
        {
            return Err(HostCapabilityError::ForbiddenDestination);
        }
    }
    Ok(target)
}

pub fn validate_download_destination(
    path: &Path,
    forbidden_roots: &[PathBuf],
) -> Result<PathBuf, HostCapabilityError> {
    validate_confined_output_file(path, forbidden_roots, false)
}

pub fn validate_pdf_destination(
    path: &Path,
    forbidden_roots: &[PathBuf],
) -> Result<PathBuf, HostCapabilityError> {
    validate_confined_output_file(path, forbidden_roots, true)
}

pub fn validate_preference_name(name: &str) -> Result<(), HostCapabilityError> {
    if name.is_empty()
        || name.len() > MAX_PREFERENCE_NAME_BYTES
        || !name.is_ascii()
        || name.starts_with('.')
        || name.ends_with('.')
        || name.contains("..")
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
    {
        return Err(HostCapabilityError::PreferenceBlocked);
    }
    let lower = name.to_ascii_lowercase();
    for blocked in [
        "browser.",
        "profile.",
        "download.",
        "safebrowsing.",
        "ssl.",
        "proxy.",
        "net.",
        "plugins.",
        "auth.",
        "credentials_enable",
        "autofill.",
        "dns_prefetching.",
    ] {
        if lower == blocked || lower.starts_with(blocked) {
            return Err(HostCapabilityError::PreferenceBlocked);
        }
    }
    for allowed in [
        "spellcheck.",
        "intl.",
        "webkit.webprefs.",
        "accessibility.",
        "enable_do_not_track",
    ] {
        if lower == allowed || (allowed.ends_with('.') && lower.starts_with(allowed)) {
            return Ok(());
        }
    }
    Err(HostCapabilityError::PreferenceBlocked)
}

pub fn validate_preference_update(
    name: &str,
    value: &PreferenceValue,
) -> Result<(), HostCapabilityError> {
    validate_preference_name(name)?;
    match value {
        PreferenceValue::Null | PreferenceValue::Bool(_) | PreferenceValue::Int(_) => Ok(()),
        PreferenceValue::Double(d) => d
            .is_finite()
            .then_some(())
            .ok_or(HostCapabilityError::InvalidPreferenceValue),
        PreferenceValue::String(s) => {
            if s.len() > MAX_PREFERENCE_STRING_BYTES
                || s.bytes().any(|b| matches!(b, 0 | b'\r' | b'\n'))
            {
                Err(HostCapabilityError::InvalidPreferenceValue)
            } else {
                Ok(())
            }
        }
        PreferenceValue::StringList(items) => {
            if items.len() > MAX_PREFERENCE_LIST_ITEMS
                || items.iter().any(|s| {
                    s.len() > MAX_PREFERENCE_STRING_BYTES
                        || s.bytes().any(|b| matches!(b, 0 | b'\r' | b'\n'))
                })
            {
                Err(HostCapabilityError::InvalidPreferenceValue)
            } else {
                Ok(())
            }
        }
    }
}

pub fn validate_cdp_method_name(method: &str) -> Result<(), HostCapabilityError> {
    if method.is_empty()
        || method.len() > MAX_CDP_METHOD_BYTES
        || !method.is_ascii()
        || !method
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_')
    {
        return Err(HostCapabilityError::InvalidCdpPayload);
    }
    Ok(())
}

pub fn validate_cdp_json(payload: &str) -> Result<(), HostCapabilityError> {
    if payload.len() > MAX_CDP_JSON_BYTES {
        return Err(HostCapabilityError::CdpPayloadTooLarge);
    }
    validate_bounded_json(payload, MAX_CDP_JSON_BYTES, MAX_JS_JSON_DEPTH).map_err(|err| {
        if matches!(err, crate::js_bridge::EvalError::PayloadTooLarge) {
            HostCapabilityError::CdpPayloadTooLarge
        } else {
            HostCapabilityError::InvalidCdpPayload
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::RuntimeGeneration;

    fn bid(local: u64) -> BrowserId {
        BrowserId::new(RuntimeGeneration::new(1), local)
    }

    #[test]
    fn single_use_ledger_rejects_duplicates_unknown_and_queue_overflow() {
        let mut ledger = SingleUseLedger::new(2);
        let k1 = HostRequestId {
            browser: bid(1),
            sequence: 1,
        };
        let k2 = HostRequestId {
            browser: bid(1),
            sequence: 2,
        };
        let k3 = HostRequestId {
            browser: bid(1),
            sequence: 3,
        };
        ledger.insert(k1).unwrap();
        assert_eq!(ledger.insert(k1), Err(HostCapabilityError::AlreadyConsumed));
        ledger.insert(k2).unwrap();
        assert_eq!(ledger.insert(k3), Err(HostCapabilityError::QueueFull));

        ledger.take(&k1).unwrap();
        assert_eq!(ledger.take(&k1), Err(HostCapabilityError::AlreadyConsumed));
        assert_eq!(
            ledger.take(&k3),
            Err(HostCapabilityError::UnknownOrStaleToken)
        );
        let cleared = ledger.clear();
        assert_eq!(cleared, vec![k2]);
        assert!(ledger.is_empty());
        assert_eq!(ledger.take(&k2), Err(HostCapabilityError::AlreadyConsumed));
    }

    #[test]
    fn secrets_and_cookies_are_redacted_in_debug_and_capabilities() {
        let secret = RedactedSecret::new("super-secret-password-value");
        let creds = AuthCredentials {
            username: RedactedSecret::new("alice"),
            password: secret,
        };
        let dbg = format!("{creds:?}");
        assert!(!dbg.contains("alice"));
        assert!(!dbg.contains("super-secret"));
        assert!(dbg.contains("[REDACTED]"));

        let cookie = CookieRecord {
            name: "session".into(),
            value: RedactedSecret::new("top-secret-token-xyz"),
            domain: "example.local".into(),
            path: "/".into(),
            secure: true,
            http_only: true,
            same_site: CookieSameSiteKind::Lax,
            priority: CookiePriorityKind::Medium,
        };
        let cdbg = format!("{cookie:?}");
        assert!(cdbg.contains("session"));
        assert!(!cdbg.contains("top-secret-token-xyz"));
        assert!(cdbg.contains("[REDACTED]"));
    }

    #[test]
    fn destination_validators_reject_traversal_ads_and_forbidden_roots() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("gpui-cef-hostcap-dest-{nonce}"));
        let allowed = base.join("downloads");
        let cache = base.join("cache");
        fs::create_dir_all(&allowed).unwrap();
        fs::create_dir_all(&cache).unwrap();

        let ok_dl = validate_download_destination(&allowed.join("report.bin"), &[cache.clone()]);
        assert!(ok_dl.is_ok());

        let ok_pdf = validate_pdf_destination(&allowed.join("export.PDF"), &[cache.clone()]);
        assert!(ok_pdf.is_ok());
        assert_eq!(
            validate_pdf_destination(&allowed.join("export.txt"), &[cache.clone()]),
            Err(HostCapabilityError::InvalidDestination)
        );

        assert_eq!(
            validate_download_destination(Path::new("relative/out.bin"), &[cache.clone()]),
            Err(HostCapabilityError::InvalidDestination)
        );
        assert_eq!(
            validate_download_destination(
                &allowed.join("..").join("cache").join("evil.bin"),
                &[cache.clone()]
            ),
            Err(HostCapabilityError::InvalidDestination)
        );
        assert_eq!(
            validate_download_destination(&allowed.join("out.bin:ads"), &[cache.clone()]),
            Err(HostCapabilityError::InvalidDestination)
        );
        assert_eq!(
            validate_download_destination(&cache.join("cookies.db"), &[cache.clone()]),
            Err(HostCapabilityError::ForbiddenDestination)
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn preference_guard_blocks_storage_keys_and_permits_safe_allowlist() {
        assert!(
            validate_preference_update(
                "intl.accept_languages",
                &PreferenceValue::String("en-US".into())
            )
            .is_ok()
        );
        assert!(
            validate_preference_update(
                "spellcheck.dictionaries",
                &PreferenceValue::StringList(vec!["en-US".into()])
            )
            .is_ok()
        );
        assert!(
            validate_preference_update("enable_do_not_track", &PreferenceValue::Bool(true)).is_ok()
        );

        for blocked in [
            "browser.cache_path",
            "profile.default_content_settings.cookies",
            "download.default_directory",
            "safebrowsing.enabled",
            "ssl.version_min",
            "unknown.arbitrary_key",
            "",
            "intl..bad",
        ] {
            assert_eq!(
                validate_preference_update(blocked, &PreferenceValue::Bool(false)),
                Err(HostCapabilityError::PreferenceBlocked),
                "{blocked:?} must be refused"
            );
        }

        assert_eq!(
            validate_preference_update("intl.accept_languages", &PreferenceValue::Double(f64::NAN)),
            Err(HostCapabilityError::InvalidPreferenceValue)
        );
    }

    #[test]
    fn cdp_json_validator_enforces_size_and_syntax() {
        assert!(
            validate_cdp_json(r#"{"method":"Runtime.evaluate","params":{"expression":"1+1"}}"#)
                .is_ok()
        );
        assert_eq!(
            validate_cdp_json("{"),
            Err(HostCapabilityError::InvalidCdpPayload)
        );
        let huge = format!("\"{}\"", "a".repeat(MAX_CDP_JSON_BYTES + 16));
        assert_eq!(
            validate_cdp_json(&huge),
            Err(HostCapabilityError::CdpPayloadTooLarge)
        );
    }

    #[test]
    fn popup_service_default_decide_popup_delegates_to_on_popup_request_and_cancels() {
        use std::sync::Mutex;
        struct RecordingService(Mutex<Vec<PopupRequest>>);
        impl PopupService for RecordingService {
            fn on_popup_request(&self, request: PopupRequest) {
                self.0.lock().unwrap().push(request);
            }
        }
        let svc = RecordingService(Mutex::new(Vec::new()));
        let req = PopupRequest {
            browser: bid(7),
            target_url: "https://example.com/next".into(),
            target_frame_name: "_blank".into(),
            disposition: PopupDisposition::NewForegroundTab,
            user_gesture: true,
            policy_allowed: true,
        };
        assert_eq!(svc.decide_popup(req.clone()), PopupDecision::Cancel);
        assert_eq!(svc.0.lock().unwrap().as_slice(), &[req]);
        assert_eq!(PopupPolicy::default(), PopupPolicy::Auto);
    }

    #[test]
    fn host_capabilities_merge_fallback_prefers_browser_slot_and_falls_back_to_runtime() {
        struct GlobalAudio;
        impl AudioStreamService for GlobalAudio {}
        struct BrowserPopup;
        impl PopupService for BrowserPopup {}

        let runtime = HostCapabilities {
            audio: Some(Arc::new(GlobalAudio)),
            popup: Some(Arc::new(BrowserPopup)),
            ..Default::default()
        };
        struct CustomPopup;
        impl PopupService for CustomPopup {}
        let browser_override = HostCapabilities {
            popup: Some(Arc::new(CustomPopup)),
            ..Default::default()
        };
        let merged = browser_override.merge_fallback(&runtime);
        assert!(merged.audio.is_some());
        assert!(merged.popup.is_some());
        assert!(Arc::ptr_eq(
            merged.popup.as_ref().unwrap(),
            browser_override.popup.as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(
            merged.audio.as_ref().unwrap(),
            runtime.audio.as_ref().unwrap()
        ));
    }
}
