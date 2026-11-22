use std::path::PathBuf;

use crate::custom_schemes::{
    CustomSchemeConfig, CustomSchemeValidationError, validate_custom_schemes,
};
use crate::js_bridge::JsBridgeConfig;
use crate::keymap::{
    EVENTFLAG_ALT_DOWN, EVENTFLAG_COMMAND_DOWN, EVENTFLAG_CONTROL_DOWN, EVENTFLAG_SHIFT_DOWN,
};

pub const MAX_TOOLTIP_BYTES: usize = 4096;
pub const MAX_STATUS_MESSAGE_BYTES: usize = 2048;
pub const MAX_FAVICON_URLS: usize = 16;
pub const MAX_FAVICON_URL_BYTES: usize = 2048;
pub const MAX_CONSOLE_MESSAGE_BYTES: usize = 4096;
pub const DEFAULT_CONSOLE_MESSAGE_BYTES: usize = 2048;
pub const MAX_SHORTCUT_RULES: usize = 64;
pub const MAX_ALLOWLISTED_SWITCHES: usize = 32;
pub const MAX_SWITCH_NAME_BYTES: usize = 64;
pub const MAX_SWITCH_VALUE_BYTES: usize = 256;
pub const MAX_SETTING_TEXT_BYTES: usize = 512;
pub const MAX_ENCODING_BYTES: usize = 64;
pub const MIN_WINDOWLESS_FRAME_RATE: u16 = 1;
pub const MAX_WINDOWLESS_FRAME_RATE: u16 = 240;
pub const MIN_ZOOM_LEVEL: f64 = -9.0;
pub const MAX_ZOOM_LEVEL: f64 = 9.0;
pub const MAX_NATIVE_BOUND: u32 = 16384;
pub const MAX_NATIVE_COORD: i32 = 1_000_000;

const ALLOWLISTED_SWITCH_NAMES: &[&str] = &[
    "autoplay-policy",
    "disable-background-timer-throttling",
    "disable-renderer-backgrounding",
    "disable-smooth-scrolling",
    "enable-begin-frame-scheduling",
    "enable-experimental-web-platform-features",
    "force-color-profile",
    "force-prefers-reduced-motion",
    "high-dpi-support",
    "lang",
    "mute-audio",
];

const FORBIDDEN_SWITCH_NAMES: &[&str] = &[
    "browser-subprocess-path",
    "cache-path",
    "disable-gpu",
    "disable-gpu-compositing",
    "disable-web-security",
    "enable-spelling",
    "in-process-gpu",
    "incognito",
    "locales-dir-path",
    "no-sandbox",
    "no-startup-window",
    "proxy-server",
    "remote-allow-origins",
    "remote-debugging-port",
    "resources-dir-path",
    "root-cache-path",
    "rust-cef-custom-schemes",
    "single-process",
    "user-data-dir",
];

pub fn truncate_utf8_bytes(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    input[..end].to_owned()
}

pub fn zoom_level_to_milli(level: f64) -> Result<i32, crate::ports::EngineError> {
    if !level.is_finite() || !(MIN_ZOOM_LEVEL..=MAX_ZOOM_LEVEL).contains(&level) {
        return Err(crate::ports::EngineError::InvalidRequest);
    }
    Ok((level * 1000.0).round() as i32)
}

pub fn progress_to_per_mille(progress: f64) -> u16 {
    if !progress.is_finite() {
        return 0;
    }
    (progress.clamp(0.0, 1.0) * 1000.0).round() as u16
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum BrowserCursor {
    #[default]
    Arrow,
    IBeam,
    Crosshair,
    ClosedHand,
    OpenHand,
    PointingHand,
    ResizeLeft,
    ResizeRight,
    ResizeLeftRight,
    ResizeUp,
    ResizeDown,
    ResizeUpDown,
    ResizeUpLeftDownRight,
    ResizeUpRightDownLeft,
    ResizeColumn,
    ResizeRow,
    IBeamVertical,
    OperationNotAllowed,
    DragLink,
    DragCopy,
    ContextualMenu,
    CustomUnsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConsoleSeverity {
    Default,
    Verbose,
    Info,
    Warning,
    Error,
    Fatal,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CefFeatureState {
    #[default]
    Default,
    Enabled,
    Disabled,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BrowserFeatureStates {
    pub javascript: CefFeatureState,
    pub local_storage: CefFeatureState,
    pub image_loading: CefFeatureState,
    pub tab_to_links: CefFeatureState,
    pub javascript_access_clipboard: CefFeatureState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConsolePolicy {
    pub enabled: bool,
    pub max_message_bytes: usize,
}

impl Default for ConsolePolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            max_message_bytes: DEFAULT_CONSOLE_MESSAGE_BYTES,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShortcutId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ShortcutRule {
    pub id: ShortcutId,
    pub windows_key_code: i32,
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub platform: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShortcutFilter {
    rules: Vec<ShortcutRule>,
}

impl ShortcutFilter {
    pub fn new(rules: Vec<ShortcutRule>) -> Result<Self, ConfigError> {
        if rules.len() > MAX_SHORTCUT_RULES || rules.iter().any(|r| r.windows_key_code <= 0) {
            return Err(ConfigError::InvalidShortcutFilter);
        }
        Ok(Self { rules })
    }

    pub fn rules(&self) -> &[ShortcutRule] {
        &self.rules
    }

    pub fn is_valid(&self) -> bool {
        self.rules.len() <= MAX_SHORTCUT_RULES && self.rules.iter().all(|r| r.windows_key_code > 0)
    }

    pub fn match_key(&self, windows_key_code: i32, cef_modifiers: u32) -> Option<ShortcutId> {
        const MASK: u32 = EVENTFLAG_SHIFT_DOWN
            | EVENTFLAG_CONTROL_DOWN
            | EVENTFLAG_ALT_DOWN
            | EVENTFLAG_COMMAND_DOWN;
        let mods = cef_modifiers & MASK;
        self.rules.iter().find_map(|rule| {
            let expected = (if rule.shift { EVENTFLAG_SHIFT_DOWN } else { 0 })
                | (if rule.control {
                    EVENTFLAG_CONTROL_DOWN
                } else {
                    0
                })
                | (if rule.alt { EVENTFLAG_ALT_DOWN } else { 0 })
                | (if rule.platform {
                    EVENTFLAG_COMMAND_DOWN
                } else {
                    0
                });
            (rule.windows_key_code == windows_key_code && mods == expected).then_some(rule.id)
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliSwitch {
    pub name: String,
    pub value: Option<String>,
}

impl CliSwitch {
    pub fn flag(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: None,
        }
    }

    pub fn with_value(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: Some(value.into()),
        }
    }
}

pub fn validate_allowlisted_switch(switch: &CliSwitch) -> Result<(), ConfigError> {
    let name = switch.name.as_str();
    if name.is_empty()
        || name.len() > MAX_SWITCH_NAME_BYTES
        || name.starts_with('-')
        || name.contains('=')
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(ConfigError::ForbiddenSwitch);
    }
    if FORBIDDEN_SWITCH_NAMES.contains(&name) || !ALLOWLISTED_SWITCH_NAMES.contains(&name) {
        return Err(ConfigError::ForbiddenSwitch);
    }
    if let Some(value) = &switch.value {
        if value.is_empty()
            || value.len() > MAX_SWITCH_VALUE_BYTES
            || value.bytes().any(|b| b == 0 || b == b'\r' || b == b'\n')
        {
            return Err(ConfigError::ForbiddenSwitch);
        }
    }
    Ok(())
}

fn validate_setting_text(value: &Option<String>, error: ConfigError) -> Result<(), ConfigError> {
    if let Some(text) = value {
        if text.trim().is_empty()
            || text.len() > MAX_SETTING_TEXT_BYTES
            || text.bytes().any(|b| b == 0 || b == b'\r' || b == b'\n')
        {
            return Err(error);
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeSettings {
    pub user_agent: Option<String>,
    pub user_agent_product: Option<String>,
    pub locale: Option<String>,
    pub accept_language_list: Option<String>,
    pub background_color: Option<u32>,
    pub allowlisted_switches: Vec<CliSwitch>,
    pub custom_schemes: Vec<CustomSchemeConfig>,
    pub windowless_rendering_enabled: bool,
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            user_agent: None,
            user_agent_product: None,
            locale: None,
            accept_language_list: None,
            background_color: None,
            allowlisted_switches: Vec::new(),
            custom_schemes: Vec::new(),
            windowless_rendering_enabled: true,
        }
    }
}

impl RuntimeSettings {
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_setting_text(&self.user_agent, ConfigError::InvalidUserAgent)?;
        validate_setting_text(
            &self.user_agent_product,
            ConfigError::InvalidUserAgentProduct,
        )?;
        validate_setting_text(&self.locale, ConfigError::InvalidLocale)?;
        validate_setting_text(
            &self.accept_language_list,
            ConfigError::InvalidAcceptLanguageList,
        )?;
        if self.allowlisted_switches.len() > MAX_ALLOWLISTED_SWITCHES {
            return Err(ConfigError::TooManySwitches);
        }
        for switch in &self.allowlisted_switches {
            validate_allowlisted_switch(switch)?;
        }
        validate_custom_schemes(&self.custom_schemes).map_err(ConfigError::InvalidCustomSchemes)?;
        Ok(())
    }
}

/// Process-lifetime identity supplied by the runtime owner, never a native pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RuntimeGeneration(u64);

impl RuntimeGeneration {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BrowserId {
    runtime: RuntimeGeneration,
    local: u64,
}

impl BrowserId {
    pub fn new(runtime: RuntimeGeneration, local: u64) -> Self {
        Self { runtime, local }
    }

    pub fn runtime(self) -> RuntimeGeneration {
        self.runtime
    }

    pub fn local(self) -> u64 {
        self.local
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProfileHandle {
    runtime: RuntimeGeneration,
    local: u64,
}

impl ProfileHandle {
    pub fn new(runtime: RuntimeGeneration, local: u64) -> Self {
        Self { runtime, local }
    }

    pub fn runtime(self) -> RuntimeGeneration {
        self.runtime
    }

    pub fn local(self) -> u64 {
        self.local
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadMode {
    Integrated,
    Threaded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeFiles {
    pub directory: PathBuf,
    pub helper_executable: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileStorage {
    Memory,
    Persistent { directory: PathBuf },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileConfig {
    pub name: String,
    pub storage: ProfileStorage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedContentPolicy {
    /// Exact canonical HTTP(S) origins, not host suffixes or URL prefixes.
    pub allowed_origins: Vec<String>,
    /// Host approval of complete data URI content, local to each Browser's
    /// lifetime. Identical approved content may be revisited; no network bypass.
    pub allow_embedded_data: bool,
}

/// Complete host choices. Native paths and capabilities are checked by the adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub thread_mode: ThreadMode,
    pub files: RuntimeFiles,
    pub root_cache_path: PathBuf,
    pub default_profile: ProfileStorage,
    pub content_policy: TrustedContentPolicy,
    pub settings: RuntimeSettings,
}

impl RuntimeConfig {
    pub fn builder() -> RuntimeConfigBuilder {
        RuntimeConfigBuilder::default()
    }
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeConfigBuilder {
    thread_mode: Option<ThreadMode>,
    files: Option<RuntimeFiles>,
    root_cache_path: Option<PathBuf>,
    default_profile: Option<ProfileStorage>,
    content_policy: Option<TrustedContentPolicy>,
    settings: RuntimeSettings,
}

impl RuntimeConfigBuilder {
    pub fn thread_mode(mut self, value: ThreadMode) -> Self {
        self.thread_mode = Some(value);
        self
    }

    pub fn files(mut self, value: RuntimeFiles) -> Self {
        self.files = Some(value);
        self
    }

    pub fn root_cache_path(mut self, value: PathBuf) -> Self {
        self.root_cache_path = Some(value);
        self
    }

    pub fn default_profile(mut self, value: ProfileStorage) -> Self {
        self.default_profile = Some(value);
        self
    }

    pub fn content_policy(mut self, value: TrustedContentPolicy) -> Self {
        self.content_policy = Some(value);
        self
    }

    pub fn settings(mut self, value: RuntimeSettings) -> Self {
        self.settings = value;
        self
    }

    pub fn user_agent(mut self, value: impl Into<String>) -> Self {
        self.settings.user_agent = Some(value.into());
        self
    }

    pub fn user_agent_product(mut self, value: impl Into<String>) -> Self {
        self.settings.user_agent_product = Some(value.into());
        self
    }

    pub fn locale(mut self, value: impl Into<String>) -> Self {
        self.settings.locale = Some(value.into());
        self
    }

    pub fn accept_language_list(mut self, value: impl Into<String>) -> Self {
        self.settings.accept_language_list = Some(value.into());
        self
    }

    pub fn background_color(mut self, value: u32) -> Self {
        self.settings.background_color = Some(value);
        self
    }

    pub fn allowlisted_switch(mut self, name: impl Into<String>, value: Option<String>) -> Self {
        self.settings.allowlisted_switches.push(CliSwitch {
            name: name.into(),
            value,
        });
        self
    }

    pub fn custom_scheme(mut self, scheme: CustomSchemeConfig) -> Self {
        self.settings.custom_schemes.push(scheme);
        self
    }

    pub fn custom_schemes(mut self, schemes: Vec<CustomSchemeConfig>) -> Self {
        self.settings.custom_schemes.extend(schemes);
        self
    }

    pub fn windowless_rendering_enabled(mut self, enabled: bool) -> Self {
        self.settings.windowless_rendering_enabled = enabled;
        self
    }

    pub fn build(self) -> Result<RuntimeConfig, ConfigError> {
        self.settings.validate()?;
        Ok(RuntimeConfig {
            thread_mode: self.thread_mode.ok_or(ConfigError::MissingThreadMode)?,
            files: self.files.ok_or(ConfigError::MissingRuntimeFiles)?,
            root_cache_path: self
                .root_cache_path
                .ok_or(ConfigError::MissingRootCachePath)?,
            default_profile: self
                .default_profile
                .ok_or(ConfigError::MissingDefaultProfile)?,
            content_policy: self
                .content_policy
                .ok_or(ConfigError::MissingContentPolicy)?,
            settings: self.settings,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    MissingThreadMode,
    MissingRuntimeFiles,
    MissingRootCachePath,
    MissingDefaultProfile,
    MissingContentPolicy,
    InvalidUserAgent,
    InvalidUserAgentProduct,
    InvalidLocale,
    InvalidAcceptLanguageList,
    ForbiddenSwitch,
    TooManySwitches,
    InvalidShortcutFilter,
    InvalidFrameRate,
    InvalidDefaultEncoding,
    InvalidConsolePolicy,
    InvalidCustomSchemes(CustomSchemeValidationError),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid runtime configuration: {self:?}")
    }
}

impl std::error::Error for ConfigError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderPreference {
    SoftwareRequired,
    D3d11Required,
    Auto,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub width: i32,
    pub height: i32,
    pub scale: f32,
}

/// Bounded physical-pixel rectangle for opt-in Win32 child `HWND` hosting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PixelRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl PixelRect {
    pub fn is_valid(&self) -> bool {
        (-MAX_NATIVE_COORD..=MAX_NATIVE_COORD).contains(&self.x)
            && (-MAX_NATIVE_COORD..=MAX_NATIVE_COORD).contains(&self.y)
            && self.width <= MAX_NATIVE_BOUND
            && self.height <= MAX_NATIVE_BOUND
            && i32::try_from(self.width)
                .ok()
                .and_then(|w| self.x.checked_add(w))
                .is_some()
            && i32::try_from(self.height)
                .ok()
                .and_then(|h| self.y.checked_add(h))
                .is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub fn try_from_i32(
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<Self, crate::ports::EngineError> {
        let width = u32::try_from(width).map_err(|_| crate::ports::EngineError::InvalidRequest)?;
        let height =
            u32::try_from(height).map_err(|_| crate::ports::EngineError::InvalidRequest)?;
        let rect = Self {
            x,
            y,
            width,
            height,
        };
        if !rect.is_valid() {
            return Err(crate::ports::EngineError::InvalidRequest);
        }
        Ok(rect)
    }

    pub fn validate(&self) -> Result<(), CreateError> {
        if self.is_valid() {
            Ok(())
        } else {
            Err(CreateError::InvalidOptions)
        }
    }
}

/// Configuration for opt-in native Win32 child `HWND` browser hosting.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct NativeChildHwndConfig {
    pub parent_hwnd: std::num::NonZeroUsize,
    pub initial_bounds: PixelRect,
    pub activate_on_creation: bool,
}

impl std::fmt::Debug for NativeChildHwndConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeChildHwndConfig")
            .field(
                "parent_hwnd",
                &format_args!("0x{:x}", self.parent_hwnd.get()),
            )
            .field("initial_bounds", &self.initial_bounds)
            .field("activate_on_creation", &self.activate_on_creation)
            .finish()
    }
}

impl NativeChildHwndConfig {
    pub fn validate(&self) -> Result<(), CreateError> {
        self.initial_bounds.validate()
    }
}

/// Selects between the default offscreen compositor surface and opt-in native child `HWND`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BrowserSurfaceMode {
    #[default]
    Offscreen,
    NativeChildHwnd(NativeChildHwndConfig),
}

impl BrowserSurfaceMode {
    pub fn is_native_child_hwnd(&self) -> bool {
        matches!(self, Self::NativeChildHwnd(_))
    }

    pub fn native_child_hwnd(&self) -> Option<NativeChildHwndConfig> {
        match self {
            Self::Offscreen => None,
            Self::NativeChildHwnd(cfg) => Some(*cfg),
        }
    }

    pub fn validate(&self) -> Result<(), CreateError> {
        match self {
            Self::Offscreen => Ok(()),
            Self::NativeChildHwnd(cfg) => cfg.validate(),
        }
    }

    pub fn validate_for_runtime(
        &self,
        windowless_rendering_enabled: bool,
    ) -> Result<(), CreateError> {
        match self {
            Self::Offscreen if !windowless_rendering_enabled => Err(CreateError::InvalidOptions),
            Self::Offscreen => Ok(()),
            Self::NativeChildHwnd(cfg) => cfg.validate(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BrowserOptions {
    pub windowless_frame_rate: Option<u16>,
    pub background_color: Option<u32>,
    pub default_encoding: Option<String>,
    pub enable_tooltip_events: bool,
    pub console_policy: ConsolePolicy,
    pub shortcut_filter: ShortcutFilter,
    pub feature_states: BrowserFeatureStates,
    pub js_bridge: JsBridgeConfig,
    pub headless_cdp_enabled: bool,
    pub outbound_drag_policy: crate::outbound_drag::OutboundDragPolicy,
    pub accessibility_state: CefFeatureState,
    pub surface_mode: BrowserSurfaceMode,
}

impl BrowserOptions {
    pub fn with_frame_rate(mut self, fps: u16) -> Self {
        self.windowless_frame_rate = Some(fps);
        self
    }

    pub fn with_background_color(mut self, color: u32) -> Self {
        self.background_color = Some(color);
        self
    }

    pub fn with_default_encoding(mut self, encoding: impl Into<String>) -> Self {
        self.default_encoding = Some(encoding.into());
        self
    }

    pub fn with_tooltip_events(mut self, enabled: bool) -> Self {
        self.enable_tooltip_events = enabled;
        self
    }

    pub fn with_console_policy(mut self, policy: ConsolePolicy) -> Self {
        self.console_policy = policy;
        self
    }

    pub fn with_shortcut_filter(mut self, filter: ShortcutFilter) -> Self {
        self.shortcut_filter = filter;
        self
    }

    pub fn with_feature_states(mut self, states: BrowserFeatureStates) -> Self {
        self.feature_states = states;
        self
    }

    pub fn with_js_bridge(mut self, config: JsBridgeConfig) -> Self {
        self.js_bridge = config;
        self
    }

    pub fn with_headless_cdp(mut self, enabled: bool) -> Self {
        self.headless_cdp_enabled = enabled;
        self
    }

    pub fn with_outbound_drag_policy(
        mut self,
        policy: crate::outbound_drag::OutboundDragPolicy,
    ) -> Self {
        self.outbound_drag_policy = policy;
        self
    }

    pub fn with_accessibility_state(mut self, state: CefFeatureState) -> Self {
        self.accessibility_state = state;
        self
    }

    pub fn with_surface_mode(mut self, mode: BrowserSurfaceMode) -> Self {
        self.surface_mode = mode;
        self
    }

    pub fn validate(&self) -> Result<(), CreateError> {
        if let Some(fps) = self.windowless_frame_rate {
            if !(MIN_WINDOWLESS_FRAME_RATE..=MAX_WINDOWLESS_FRAME_RATE).contains(&fps) {
                return Err(CreateError::InvalidOptions);
            }
        }
        if let Some(encoding) = &self.default_encoding {
            if encoding.trim().is_empty()
                || encoding.len() > MAX_ENCODING_BYTES
                || !encoding.is_ascii()
                || encoding.bytes().any(|b| b < 0x20 || b == 0x7f)
            {
                return Err(CreateError::InvalidOptions);
            }
        }
        if self.console_policy.max_message_bytes > MAX_CONSOLE_MESSAGE_BYTES
            || (self.console_policy.enabled && self.console_policy.max_message_bytes == 0)
        {
            return Err(CreateError::InvalidOptions);
        }
        if !self.shortcut_filter.is_valid() {
            return Err(CreateError::InvalidOptions);
        }
        self.js_bridge.validate()?;
        self.surface_mode.validate()?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BrowserConfig {
    pub initial_url: String,
    /// None selects the runtime's shared default context, not a new context.
    pub profile: Option<ProfileHandle>,
    pub render_preference: RenderPreference,
    pub viewport: Viewport,
    pub options: BrowserOptions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateError {
    WrongRuntime,
    NotReady,
    Closing,
    InvalidViewport,
    InvalidOptions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Creating,
    Alive,
    Closing,
    Closed,
    CreateFailed,
}

/// Latest owned page state, independent of native creation and frame delivery.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NavigationSnapshot {
    pub address: String,
    pub title: String,
    pub is_loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub last_main_frame_error: Option<i32>,
    pub last_sub_frame_error: Option<i32>,
    pub main_http_status: Option<i32>,
    pub cursor: BrowserCursor,
    pub tooltip: String,
    pub status_message: String,
    pub favicon_urls: Vec<String>,
    pub fullscreen: bool,
    pub loading_progress_per_mille: u16,
    pub zoom_level_milli: i32,
    pub audio_muted: bool,
}

impl NavigationSnapshot {
    pub fn loading_progress(&self) -> f32 {
        self.loading_progress_per_mille as f32 / 1000.0
    }

    pub fn zoom_level(&self) -> f64 {
        self.zoom_level_milli as f64 / 1000.0
    }
}

/// Native renderer observations, never inferred from frame frequency.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RendererState {
    #[default]
    Starting,
    Running,
    Unresponsive,
    Terminated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RendererTermination {
    /// Adapter's raw termination category, not a process identifier.
    pub native_status: i32,
    /// Platform/CEF termination error; not a network error or PID.
    pub code: Option<i32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RendererSnapshot {
    pub state: RendererState,
    /// Retained after a new render view becomes ready for diagnostics.
    pub last_termination: Option<RendererTermination>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserSnapshot {
    pub browser: BrowserId,
    pub creation_accepted: bool,
    pub state: SessionState,
    pub navigation: NavigationSnapshot,
    pub renderer: RendererSnapshot,
}

impl BrowserSnapshot {
    /// Native Browser lifetime only; inspect renderer/navigation separately.
    pub fn is_ready(&self) -> bool {
        self.state == SessionState::Alive
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeState {
    Starting,
    Ready,
    Draining,
    ShutdownBlocked,
    Stopped,
    ProcessExitRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitializeOutcome {
    Accepted,
    ProcessExitRequired { exit_code: Option<i32> },
}
