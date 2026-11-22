//! Pure contracts, validation, and subprocess wire encoding for custom URL
//! schemes, confined profile asset sources, and bounded request interception.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

pub const MAX_CUSTOM_SCHEMES: usize = 16;
pub const MAX_SCHEME_NAME_BYTES: usize = 32;
pub const MAX_SCHEME_DOMAIN_BYTES: usize = 253;
pub const MAX_SCHEME_MEMORY_ASSETS: usize = 256;
pub const MAX_SCHEME_ASSET_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_SCHEME_TOTAL_MEMORY_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_INTERCEPT_HEADERS: usize = 16;
pub const MAX_HEADER_NAME_BYTES: usize = 128;
pub const MAX_HEADER_VALUE_BYTES: usize = 2048;

pub const CUSTOM_SCHEMES_SWITCH: &str = "rust-cef-custom-schemes";

pub const RESERVED_SCHEMES: &[&str] = &[
    "http",
    "https",
    "file",
    "data",
    "javascript",
    "blob",
    "about",
    "chrome",
    "devtools",
    "ftp",
    "ws",
    "wss",
    "view-source",
    "chrome-extension",
    "chrome-untrusted",
    "filesystem",
];

// Exact CEF 146 `cef_scheme_options_t` bit values.
pub const CEF_SCHEME_OPTION_STANDARD: i32 = 1 << 0;
pub const CEF_SCHEME_OPTION_LOCAL: i32 = 1 << 1;
pub const CEF_SCHEME_OPTION_DISPLAY_ISOLATED: i32 = 1 << 2;
pub const CEF_SCHEME_OPTION_SECURE: i32 = 1 << 3;
pub const CEF_SCHEME_OPTION_CORS_ENABLED: i32 = 1 << 4;
pub const CEF_SCHEME_OPTION_CSP_BYPASSING: i32 = 1 << 5;
pub const CEF_SCHEME_OPTION_FETCH_ENABLED: i32 = 1 << 6;
const ALL_SCHEME_OPTION_BITS: i32 = CEF_SCHEME_OPTION_STANDARD
    | CEF_SCHEME_OPTION_LOCAL
    | CEF_SCHEME_OPTION_DISPLAY_ISOLATED
    | CEF_SCHEME_OPTION_SECURE
    | CEF_SCHEME_OPTION_CORS_ENABLED
    | CEF_SCHEME_OPTION_CSP_BYPASSING
    | CEF_SCHEME_OPTION_FETCH_ENABLED;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CustomSchemeValidationError {
    EmptyScheme,
    InvalidSchemeSyntax,
    ReservedScheme,
    DuplicateScheme,
    TooManySchemes,
    InvalidDomain,
    InvalidAssetPath,
    TooManyAssets,
    AssetTooLarge,
    TotalAssetsTooLarge,
    InvalidMimeType,
    InvalidStatusCode,
    InvalidHeader,
    InvalidDirectoryRoot,
    InvalidDefaultPage,
    InvalidSerializedSwitch,
}

impl fmt::Display for CustomSchemeValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "custom scheme validation failed: {self:?}")
    }
}

impl std::error::Error for CustomSchemeValidationError {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CustomSchemeOptions {
    pub is_standard: bool,
    pub is_local: bool,
    pub is_display_isolated: bool,
    pub is_secure: bool,
    pub is_cors_enabled: bool,
    pub is_csp_bypassing: bool,
    pub is_fetch_enabled: bool,
}

impl CustomSchemeOptions {
    /// Standard secure app scheme supporting Fetch API and same-origin CORS.
    pub fn standard_app() -> Self {
        Self {
            is_standard: true,
            is_local: false,
            is_display_isolated: false,
            is_secure: true,
            is_cors_enabled: true,
            is_csp_bypassing: false,
            is_fetch_enabled: true,
        }
    }

    pub fn to_cef_flags(&self) -> i32 {
        let mut flags = 0i32;
        if self.is_standard {
            flags |= CEF_SCHEME_OPTION_STANDARD;
        }
        if self.is_local {
            flags |= CEF_SCHEME_OPTION_LOCAL;
        }
        if self.is_display_isolated {
            flags |= CEF_SCHEME_OPTION_DISPLAY_ISOLATED;
        }
        if self.is_secure {
            flags |= CEF_SCHEME_OPTION_SECURE;
        }
        if self.is_cors_enabled {
            flags |= CEF_SCHEME_OPTION_CORS_ENABLED;
        }
        if self.is_csp_bypassing {
            flags |= CEF_SCHEME_OPTION_CSP_BYPASSING;
        }
        if self.is_fetch_enabled {
            flags |= CEF_SCHEME_OPTION_FETCH_ENABLED;
        }
        flags
    }

    pub fn from_cef_flags(flags: i32) -> Result<Self, CustomSchemeValidationError> {
        if flags < 0 || (flags & !ALL_SCHEME_OPTION_BITS) != 0 {
            return Err(CustomSchemeValidationError::InvalidSerializedSwitch);
        }
        Ok(Self {
            is_standard: flags & CEF_SCHEME_OPTION_STANDARD != 0,
            is_local: flags & CEF_SCHEME_OPTION_LOCAL != 0,
            is_display_isolated: flags & CEF_SCHEME_OPTION_DISPLAY_ISOLATED != 0,
            is_secure: flags & CEF_SCHEME_OPTION_SECURE != 0,
            is_cors_enabled: flags & CEF_SCHEME_OPTION_CORS_ENABLED != 0,
            is_csp_bypassing: flags & CEF_SCHEME_OPTION_CSP_BYPASSING != 0,
            is_fetch_enabled: flags & CEF_SCHEME_OPTION_FETCH_ENABLED != 0,
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct MemoryAsset {
    pub status_code: u16,
    pub mime_type: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl fmt::Debug for MemoryAsset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryAsset")
            .field("status_code", &self.status_code)
            .field("mime_type", &self.mime_type)
            .field("header_count", &self.headers.len())
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

impl MemoryAsset {
    pub fn html(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status_code: 200,
            mime_type: "text/html".into(),
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub fn json(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status_code: 200,
            mime_type: "application/json".into(),
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub fn bytes(mime_type: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status_code: 200,
            mime_type: mime_type.into(),
            headers: Vec::new(),
            body: body.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemeAssetSource {
    Memory(BTreeMap<String, MemoryAsset>),
    Directory { root: PathBuf, default_page: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomSchemeConfig {
    pub scheme: String,
    pub options: CustomSchemeOptions,
    pub domain: Option<String>,
    pub asset_source: Option<SchemeAssetSource>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileSchemeBinding {
    pub scheme: String,
    pub domain: Option<String>,
    pub source: SchemeAssetSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterceptRequest {
    pub url: String,
    pub method: String,
    pub is_main_frame: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub enum InterceptAction {
    Continue,
    Cancel,
    AddHeaders(Vec<(String, String)>),
    Respond {
        status_code: u16,
        mime_type: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
}

impl fmt::Debug for InterceptAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Continue => f.write_str("Continue"),
            Self::Cancel => f.write_str("Cancel"),
            Self::AddHeaders(h) => f
                .debug_struct("AddHeaders")
                .field("count", &h.len())
                .finish(),
            Self::Respond {
                status_code,
                mime_type,
                headers,
                body,
            } => f
                .debug_struct("Respond")
                .field("status_code", status_code)
                .field("mime_type", mime_type)
                .field("header_count", &headers.len())
                .field("body_bytes", &body.len())
                .finish(),
        }
    }
}

/// Optional host request interceptor. Invoked strictly on IO callbacks AFTER
/// `TrustedContentPolicy` has already approved the request URL.
pub trait ResourceInterceptor: Send + Sync {
    fn intercept(&self, request: &InterceptRequest) -> InterceptAction;
}

pub fn validate_intercept_header(name: &str, value: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.len() != name.len()
        || name.len() > MAX_HEADER_NAME_BYTES
        || value.len() > MAX_HEADER_VALUE_BYTES
    {
        return false;
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return false;
    }
    if value.bytes().any(|b| matches!(b, 0 | b'\r' | b'\n')) {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "upgrade"
            | "cookie"
            | "set-cookie"
            | "origin"
            | "referer"
            | "authorization"
            | "proxy-authorization"
            | "te"
            | "trailer"
    ) || lower.starts_with("proxy-")
        || lower.starts_with("sec-")
    {
        return false;
    }
    true
}

pub fn canonicalize_scheme_name(raw: &str) -> Result<String, CustomSchemeValidationError> {
    if raw.is_empty() {
        return Err(CustomSchemeValidationError::EmptyScheme);
    }
    if raw.len() > MAX_SCHEME_NAME_BYTES {
        return Err(CustomSchemeValidationError::InvalidSchemeSyntax);
    }
    let mut bytes = raw.bytes();
    let Some(first) = bytes.next() else {
        return Err(CustomSchemeValidationError::EmptyScheme);
    };
    if !first.is_ascii_alphabetic()
        || !bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    {
        return Err(CustomSchemeValidationError::InvalidSchemeSyntax);
    }
    let lower = raw.to_ascii_lowercase();
    if RESERVED_SCHEMES.contains(&lower.as_str()) {
        return Err(CustomSchemeValidationError::ReservedScheme);
    }
    Ok(lower)
}

pub fn validate_domain_name(domain: &str) -> Result<String, CustomSchemeValidationError> {
    if domain.is_empty()
        || domain.len() > MAX_SCHEME_DOMAIN_BYTES
        || domain.trim() != domain
        || domain
            .bytes()
            .any(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'[' | b']' | b':')))
    {
        return Err(CustomSchemeValidationError::InvalidDomain);
    }
    Ok(domain.to_ascii_lowercase())
}

pub fn validate_mime_type(mime: &str) -> bool {
    let trimmed = mime.trim();
    if trimmed.is_empty()
        || trimmed.len() != mime.len()
        || mime.len() > 128
        || mime.bytes().any(|b| b <= 0x20 || b == 0x7f)
    {
        return false;
    }
    let Some((major, minor)) = mime.split_once('/') else {
        return false;
    };
    !major.is_empty() && !minor.is_empty()
}

pub fn normalize_memory_asset_path(path: &str) -> Result<String, CustomSchemeValidationError> {
    if path.is_empty()
        || path.len() > 1024
        || path.contains('\0')
        || path.contains('\\')
        || path.contains(':')
        || path.contains('?')
        || path.contains('#')
    {
        return Err(CustomSchemeValidationError::InvalidAssetPath);
    }
    let normalized = path.strip_prefix('/').unwrap_or(path);
    if normalized.is_empty() {
        return Err(CustomSchemeValidationError::InvalidAssetPath);
    }
    for segment in normalized.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(CustomSchemeValidationError::InvalidAssetPath);
        }
    }
    Ok(normalized.to_owned())
}

pub fn validate_scheme_asset_source(
    source: &SchemeAssetSource,
) -> Result<(), CustomSchemeValidationError> {
    match source {
        SchemeAssetSource::Memory(map) => {
            if map.len() > MAX_SCHEME_MEMORY_ASSETS {
                return Err(CustomSchemeValidationError::TooManyAssets);
            }
            let mut total_bytes = 0usize;
            for (path, asset) in map {
                normalize_memory_asset_path(path)?;
                if !(100..=599).contains(&asset.status_code) {
                    return Err(CustomSchemeValidationError::InvalidStatusCode);
                }
                if !validate_mime_type(&asset.mime_type) {
                    return Err(CustomSchemeValidationError::InvalidMimeType);
                }
                if asset.headers.len() > MAX_INTERCEPT_HEADERS {
                    return Err(CustomSchemeValidationError::InvalidHeader);
                }
                for (name, value) in &asset.headers {
                    if !validate_intercept_header(name, value) {
                        return Err(CustomSchemeValidationError::InvalidHeader);
                    }
                }
                if asset.body.len() > MAX_SCHEME_ASSET_BYTES {
                    return Err(CustomSchemeValidationError::AssetTooLarge);
                }
                total_bytes = total_bytes
                    .checked_add(asset.body.len())
                    .ok_or(CustomSchemeValidationError::TotalAssetsTooLarge)?;
                if total_bytes > MAX_SCHEME_TOTAL_MEMORY_BYTES {
                    return Err(CustomSchemeValidationError::TotalAssetsTooLarge);
                }
            }
            Ok(())
        }
        SchemeAssetSource::Directory { root, default_page } => {
            if !root.is_absolute()
                || root.as_os_str().is_empty()
                || root.to_string_lossy().contains('\0')
            {
                return Err(CustomSchemeValidationError::InvalidDirectoryRoot);
            }
            if default_page.is_empty()
                || default_page.len() > 256
                || default_page.contains('/')
                || default_page.contains('\\')
                || default_page.contains(':')
                || default_page.contains('\0')
                || default_page == "."
                || default_page == ".."
            {
                return Err(CustomSchemeValidationError::InvalidDefaultPage);
            }
            Ok(())
        }
    }
}

pub fn validate_profile_scheme_binding(
    binding: &ProfileSchemeBinding,
) -> Result<(), CustomSchemeValidationError> {
    canonicalize_scheme_name(&binding.scheme)?;
    if let Some(domain) = &binding.domain {
        validate_domain_name(domain)?;
    }
    validate_scheme_asset_source(&binding.source)
}

pub fn validate_custom_schemes(
    schemes: &[CustomSchemeConfig],
) -> Result<(), CustomSchemeValidationError> {
    if schemes.len() > MAX_CUSTOM_SCHEMES {
        return Err(CustomSchemeValidationError::TooManySchemes);
    }
    let mut seen = BTreeSet::new();
    for entry in schemes {
        let canonical = canonicalize_scheme_name(&entry.scheme)?;
        if !seen.insert(canonical) {
            return Err(CustomSchemeValidationError::DuplicateScheme);
        }
        if let Some(domain) = &entry.domain {
            validate_domain_name(domain)?;
        }
        if let Some(source) = &entry.asset_source {
            validate_scheme_asset_source(source)?;
        }
    }
    Ok(())
}

pub fn encode_custom_schemes_switch(
    schemes: &[CustomSchemeConfig],
) -> Result<String, CustomSchemeValidationError> {
    validate_custom_schemes(schemes)?;
    let mut parts = Vec::with_capacity(schemes.len());
    for entry in schemes {
        let name = canonicalize_scheme_name(&entry.scheme)?;
        parts.push(format!("{name}:{}", entry.options.to_cef_flags()));
    }
    Ok(parts.join(","))
}

pub fn decode_custom_schemes_switch(
    raw: &str,
) -> Result<Vec<CustomSchemeConfig>, CustomSchemeValidationError> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    if raw.len() > 1024 {
        return Err(CustomSchemeValidationError::InvalidSerializedSwitch);
    }
    let mut out = Vec::new();
    for item in raw.split(',') {
        let Some((scheme_raw, flags_raw)) = item.split_once(':') else {
            return Err(CustomSchemeValidationError::InvalidSerializedSwitch);
        };
        let scheme = canonicalize_scheme_name(scheme_raw)?;
        let flags: i32 = flags_raw
            .parse()
            .map_err(|_| CustomSchemeValidationError::InvalidSerializedSwitch)?;
        let options = CustomSchemeOptions::from_cef_flags(flags)?;
        out.push(CustomSchemeConfig {
            scheme,
            options,
            domain: None,
            asset_source: None,
        });
    }
    validate_custom_schemes(&out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scheme(name: &str) -> CustomSchemeConfig {
        CustomSchemeConfig {
            scheme: name.into(),
            options: CustomSchemeOptions::standard_app(),
            domain: None,
            asset_source: None,
        }
    }

    #[test]
    fn reserved_and_over_limit_schemes_are_rejected() {
        for reserved in [
            "http",
            "HTTPS",
            "file",
            "data",
            "javascript",
            "blob",
            "about",
            "chrome",
            "devtools",
        ] {
            assert_eq!(
                validate_custom_schemes(&[scheme(reserved)]),
                Err(CustomSchemeValidationError::ReservedScheme),
                "{reserved}"
            );
        }

        assert_eq!(
            validate_custom_schemes(&[scheme("1bad")]),
            Err(CustomSchemeValidationError::InvalidSchemeSyntax)
        );
        assert_eq!(
            validate_custom_schemes(&[scheme("app"), scheme("APP")]),
            Err(CustomSchemeValidationError::DuplicateScheme)
        );

        let too_many: Vec<_> = (0..=MAX_CUSTOM_SCHEMES)
            .map(|i| scheme(&format!("s{i}")))
            .collect();
        assert_eq!(
            validate_custom_schemes(&too_many),
            Err(CustomSchemeValidationError::TooManySchemes)
        );
    }

    #[test]
    fn wire_switch_encoding_round_trips_and_rejects_malformed() {
        let list = vec![
            scheme("app"),
            CustomSchemeConfig {
                scheme: "asset".into(),
                options: CustomSchemeOptions {
                    is_standard: true,
                    is_local: true,
                    is_display_isolated: true,
                    is_secure: false,
                    is_cors_enabled: false,
                    is_csp_bypassing: false,
                    is_fetch_enabled: true,
                },
                domain: Some("local".into()),
                asset_source: None,
            },
        ];
        let encoded = encode_custom_schemes_switch(&list).unwrap();
        let decoded = decode_custom_schemes_switch(&encoded).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].scheme, "app");
        assert_eq!(decoded[0].options, CustomSchemeOptions::standard_app());
        assert_eq!(decoded[1].scheme, "asset");
        assert_eq!(decoded[1].options, list[1].options);

        assert!(decode_custom_schemes_switch("http:1").is_err());
        assert!(decode_custom_schemes_switch("app:999999").is_err());
        assert!(decode_custom_schemes_switch("app").is_err());
    }

    #[test]
    fn asset_and_header_validation_reject_traversal_and_reserved_headers() {
        assert_eq!(
            normalize_memory_asset_path("/index.html"),
            Ok("index.html".into())
        );
        for bad in ["", "/", "../secret.txt", "a\\b", "file:ads", "a\0b", "a//b"] {
            assert!(normalize_memory_asset_path(bad).is_err(), "{bad:?}");
        }

        assert!(validate_intercept_header("X-App-Token", "ok-value"));
        for bad_hdr in [
            "Host",
            "Cookie",
            "Content-Length",
            "Origin",
            "Referer",
            "Sec-Fetch-Site",
            "Proxy-Authorization",
            "Bad\r\nHeader",
        ] {
            assert!(
                !validate_intercept_header(bad_hdr, "val"),
                "{bad_hdr:?} should be rejected"
            );
        }
        assert!(!validate_intercept_header("X-Ok", "bad\r\nvalue"));
    }
}
