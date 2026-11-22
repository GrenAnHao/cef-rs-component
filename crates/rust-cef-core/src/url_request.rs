//! Pure-std contracts, bounds validation, redaction, and concurrency ledger
//! for Profile-scoped and Frame-scoped `CefURLRequest` execution.
//!
//! Structural reference (`D:\System Dir\Downloads\CefSharp`, rev `41919703`):
//! - `CefSharp.Core.Runtime/UrlRequest.h:26-73`
//! - `CefSharp.Core.Runtime/Internals/CefFrameWrapper.cpp:402-416`
//! - `CefSharp.Core.Runtime/Internals/CefUrlRequestClientAdapter.cpp:14-45`

use std::collections::BTreeMap;
use std::fmt;

use crate::contracts::{BrowserId, ProfileHandle};
use crate::ports::EngineError;

pub const MAX_IN_FLIGHT_URL_REQUESTS: usize = 32;
pub const MAX_URL_REQUEST_HEADERS: usize = 64;
pub const MAX_URL_REQUEST_HEADER_BYTES: usize = 8 * 1024;
pub const MAX_URL_REQUEST_UPLOAD_BYTES: usize = 4 * 1024 * 1024;
pub const DEFAULT_MAX_URL_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
pub const HARD_MAX_URL_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
pub const MIN_URL_REQUEST_TIMEOUT_MS: u64 = 100;
pub const MAX_URL_REQUEST_TIMEOUT_MS: u64 = 120_000;
pub const DEFAULT_URL_REQUEST_TIMEOUT_MS: u64 = 30_000;

pub fn is_sensitive_http_header(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UrlRequestId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UrlRequestTarget {
    DefaultProfile,
    Profile(ProfileHandle),
    BrowserMainFrame(BrowserId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UrlRequestMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
}

impl UrlRequestMethod {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
        }
    }

    pub fn parse_allowlisted(raw: &str) -> Result<Self, EngineError> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "GET" => Ok(Self::Get),
            "POST" => Ok(Self::Post),
            "PUT" => Ok(Self::Put),
            "PATCH" => Ok(Self::Patch),
            "DELETE" => Ok(Self::Delete),
            "HEAD" => Ok(Self::Head),
            _ => Err(EngineError::InvalidRequest),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UrlRequestFlags {
    pub skip_cache: bool,
    pub allow_stored_credentials: bool,
    pub report_upload_progress: bool,
    pub no_download_data: bool,
    pub no_retry_on_5xx: bool,
    pub stop_on_redirect: bool,
}

/// Bounded asynchronous HTTP request specification. `Debug` masks sensitive headers
/// (`Authorization`, `Proxy-Authorization`, `Cookie`, `Set-Cookie`) and upload body bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct UrlRequestSpec {
    pub target: UrlRequestTarget,
    pub method: UrlRequestMethod,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub upload_body: Option<Vec<u8>>,
    pub upload_content_type: Option<String>,
    pub max_response_bytes: usize,
    pub timeout_ms: u64,
    pub flags: UrlRequestFlags,
}

impl UrlRequestSpec {
    pub fn get(target: UrlRequestTarget, url: impl Into<String>) -> Self {
        Self {
            target,
            method: UrlRequestMethod::Get,
            url: url.into(),
            headers: Vec::new(),
            upload_body: None,
            upload_content_type: None,
            max_response_bytes: DEFAULT_MAX_URL_RESPONSE_BYTES,
            timeout_ms: DEFAULT_URL_REQUEST_TIMEOUT_MS,
            flags: UrlRequestFlags {
                allow_stored_credentials: true,
                ..Default::default()
            },
        }
    }

    pub fn post(
        target: UrlRequestTarget,
        url: impl Into<String>,
        body: Vec<u8>,
        content_type: impl Into<String>,
    ) -> Self {
        Self {
            target,
            method: UrlRequestMethod::Post,
            url: url.into(),
            headers: Vec::new(),
            upload_body: Some(body),
            upload_content_type: Some(content_type.into()),
            max_response_bytes: DEFAULT_MAX_URL_RESPONSE_BYTES,
            timeout_ms: DEFAULT_URL_REQUEST_TIMEOUT_MS,
            flags: UrlRequestFlags {
                allow_stored_credentials: true,
                ..Default::default()
            },
        }
    }

    pub fn validate(&self) -> Result<(), EngineError> {
        if self.url.trim().is_empty()
            || self
                .url
                .bytes()
                .any(|b| b == 0 || b == b'\r' || b == b'\n' || b < 0x20 || b == 0x7f)
        {
            return Err(EngineError::InvalidRequest);
        }
        if self.headers.len() > MAX_URL_REQUEST_HEADERS {
            return Err(EngineError::InvalidRequest);
        }
        for (name, value) in &self.headers {
            if !is_valid_header_name(name)
                || value.bytes().any(|b| b == 0 || b == b'\r' || b == b'\n')
                || name.len().saturating_add(value.len()) > MAX_URL_REQUEST_HEADER_BYTES
            {
                return Err(EngineError::InvalidRequest);
            }
        }
        if let Some(body) = &self.upload_body {
            if matches!(self.method, UrlRequestMethod::Get | UrlRequestMethod::Head) {
                return Err(EngineError::InvalidRequest);
            }
            if body.len() > MAX_URL_REQUEST_UPLOAD_BYTES {
                return Err(EngineError::InvalidRequest);
            }
        }
        if let Some(ct) = &self.upload_content_type {
            if ct.trim().is_empty()
                || ct.len() > MAX_URL_REQUEST_HEADER_BYTES
                || ct.bytes().any(|b| b == 0 || b == b'\r' || b == b'\n')
            {
                return Err(EngineError::InvalidRequest);
            }
        }
        let max_valid = if self.flags.no_download_data {
            self.max_response_bytes <= HARD_MAX_URL_RESPONSE_BYTES
        } else {
            (1..=HARD_MAX_URL_RESPONSE_BYTES).contains(&self.max_response_bytes)
        };
        if !max_valid {
            return Err(EngineError::InvalidRequest);
        }
        if !(MIN_URL_REQUEST_TIMEOUT_MS..=MAX_URL_REQUEST_TIMEOUT_MS).contains(&self.timeout_ms) {
            return Err(EngineError::InvalidRequest);
        }
        Ok(())
    }
}

fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_graphic()
                && b != b':'
                && b != b' '
                && !matches!(
                    b,
                    b'(' | b')'
                        | b'<'
                        | b'>'
                        | b'@'
                        | b','
                        | b';'
                        | b'\\'
                        | b'"'
                        | b'/'
                        | b'['
                        | b']'
                        | b'?'
                        | b'='
                        | b'{'
                        | b'}'
                )
        })
}

fn redact_headers_for_debug(headers: &[(String, String)]) -> Vec<(&str, &str)> {
    headers
        .iter()
        .map(|(k, v)| {
            if is_sensitive_http_header(k) {
                (k.as_str(), "[REDACTED]")
            } else {
                (k.as_str(), v.as_str())
            }
        })
        .collect()
}

impl fmt::Debug for UrlRequestSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UrlRequestSpec")
            .field("target", &self.target)
            .field("method", &self.method)
            .field("url_bytes", &self.url.len())
            .field("headers", &redact_headers_for_debug(&self.headers))
            .field("upload_bytes", &self.upload_body.as_ref().map(Vec::len))
            .field("upload_content_type", &self.upload_content_type)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("timeout_ms", &self.timeout_ms)
            .field("flags", &self.flags)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UrlRequestStatus {
    Unknown,
    Success,
    IoPending,
    Canceled,
    Failed,
}

#[derive(Clone, PartialEq, Eq)]
pub struct UrlRequestResponse {
    pub status: UrlRequestStatus,
    pub error_code: i32,
    pub http_status: Option<i32>,
    pub final_url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub response_was_cached: bool,
}

impl fmt::Debug for UrlRequestResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UrlRequestResponse")
            .field("status", &self.status)
            .field("error_code", &self.error_code)
            .field("http_status", &self.http_status)
            .field("final_url_bytes", &self.final_url.len())
            .field("headers", &redact_headers_for_debug(&self.headers))
            .field("body_bytes", &self.body.len())
            .field("response_was_cached", &self.response_was_cached)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UrlRequestError {
    PolicyDenied,
    InvalidRequest,
    Backpressure,
    NotReady,
    Closing,
    ResponseTooLarge,
    Timeout,
    Cancelled,
    Failed { error_code: i32 },
}

impl fmt::Display for UrlRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "url request error: {self:?}")
    }
}

impl std::error::Error for UrlRequestError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UrlRequestProgress {
    pub id: UrlRequestId,
    pub uploaded_bytes: i64,
    pub upload_total: i64,
    pub downloaded_bytes: i64,
    pub download_total: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UrlRequestEntryState {
    Active,
    Cancelling,
}

/// Bounded in-flight ledger participating in Runtime shutdown barriers.
#[derive(Debug)]
pub struct UrlRequestLedger {
    next_id: u64,
    draining: bool,
    active: BTreeMap<UrlRequestId, UrlRequestEntryState>,
}

impl Default for UrlRequestLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl UrlRequestLedger {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            draining: false,
            active: BTreeMap::new(),
        }
    }

    pub fn admit(&mut self) -> Result<UrlRequestId, UrlRequestError> {
        if self.draining {
            return Err(UrlRequestError::Closing);
        }
        if self.active.len() >= MAX_IN_FLIGHT_URL_REQUESTS {
            return Err(UrlRequestError::Backpressure);
        }
        let id = UrlRequestId(self.next_id);
        let next = self
            .next_id
            .checked_add(1)
            .ok_or(UrlRequestError::Backpressure)?;
        self.next_id = next;
        self.active.insert(id, UrlRequestEntryState::Active);
        Ok(id)
    }

    pub fn mark_cancelling(&mut self, id: UrlRequestId) -> bool {
        let Some(state) = self.active.get_mut(&id) else {
            return false;
        };
        if *state == UrlRequestEntryState::Cancelling {
            return false;
        }
        *state = UrlRequestEntryState::Cancelling;
        true
    }

    pub fn complete(&mut self, id: UrlRequestId) -> bool {
        self.active.remove(&id).is_some()
    }

    pub fn begin_drain(&mut self) -> Vec<UrlRequestId> {
        self.draining = true;
        let mut to_cancel = Vec::new();
        for (id, state) in &mut self.active {
            if *state == UrlRequestEntryState::Active {
                *state = UrlRequestEntryState::Cancelling;
                to_cancel.push(*id);
            }
        }
        to_cancel
    }

    pub fn in_flight_count(&self) -> usize {
        self.active.len()
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    pub fn is_draining(&self) -> bool {
        self.draining
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_allowlist_and_spec_bounds_enforced() {
        assert_eq!(
            UrlRequestMethod::parse_allowlisted("post"),
            Ok(UrlRequestMethod::Post)
        );
        assert_eq!(
            UrlRequestMethod::parse_allowlisted("CONNECT"),
            Err(EngineError::InvalidRequest)
        );

        let mut get_spec = UrlRequestSpec::get(
            UrlRequestTarget::DefaultProfile,
            "https://trusted.example.com/api",
        );
        assert_eq!(get_spec.validate(), Ok(()));

        // GET cannot carry upload body.
        get_spec.upload_body = Some(vec![1, 2, 3]);
        assert_eq!(get_spec.validate(), Err(EngineError::InvalidRequest));

        // Oversized response cap and bad timeouts fail validation.
        let mut post = UrlRequestSpec::post(
            UrlRequestTarget::DefaultProfile,
            "https://trusted.example.com/upload",
            vec![1; 16],
            "application/octet-stream",
        );
        post.max_response_bytes = HARD_MAX_URL_RESPONSE_BYTES + 1;
        assert_eq!(post.validate(), Err(EngineError::InvalidRequest));
        post.max_response_bytes = DEFAULT_MAX_URL_RESPONSE_BYTES;
        post.timeout_ms = MIN_URL_REQUEST_TIMEOUT_MS - 1;
        assert_eq!(post.validate(), Err(EngineError::InvalidRequest));
        post.timeout_ms = DEFAULT_URL_REQUEST_TIMEOUT_MS;

        // CR/LF header injection fails validation.
        post.headers
            .push(("X-Test".into(), "ok\r\nEvil: injected".into()));
        assert_eq!(post.validate(), Err(EngineError::InvalidRequest));
        post.headers.clear();
        post.upload_body = Some(vec![0; MAX_URL_REQUEST_UPLOAD_BYTES + 1]);
        assert_eq!(post.validate(), Err(EngineError::InvalidRequest));
    }

    #[test]
    fn debug_redacts_sensitive_headers_and_bodies() {
        let mut spec = UrlRequestSpec::post(
            UrlRequestTarget::DefaultProfile,
            "https://trusted.example.com/login?secret=abc",
            b"SECRET_UPLOAD_BODY_XYZ".to_vec(),
            "text/plain",
        );
        spec.headers = vec![
            ("Authorization".into(), "Bearer TOP_SECRET_TOKEN".into()),
            ("Cookie".into(), "sid=SUPER_SECRET_COOKIE".into()),
            ("X-Trace".into(), "trace-42".into()),
        ];
        let spec_dbg = format!("{spec:?}");
        assert!(!spec_dbg.contains("TOP_SECRET_TOKEN"));
        assert!(!spec_dbg.contains("SUPER_SECRET_COOKIE"));
        assert!(!spec_dbg.contains("SECRET_UPLOAD_BODY_XYZ"));
        assert!(!spec_dbg.contains("secret=abc"));
        assert!(spec_dbg.contains("trace-42"));
        assert!(spec_dbg.contains("[REDACTED]"));

        let resp = UrlRequestResponse {
            status: UrlRequestStatus::Success,
            error_code: 0,
            http_status: Some(200),
            final_url: "https://trusted.example.com/ok?tok=hidden".into(),
            headers: vec![
                ("Set-Cookie".into(), "session=SENSITIVE_VALUE".into()),
                ("Content-Type".into(), "application/json".into()),
            ],
            body: b"PRIVATE_RESPONSE_PAYLOAD".to_vec(),
            response_was_cached: false,
        };
        let resp_dbg = format!("{resp:?}");
        assert!(!resp_dbg.contains("SENSITIVE_VALUE"));
        assert!(!resp_dbg.contains("PRIVATE_RESPONSE_PAYLOAD"));
        assert!(!resp_dbg.contains("tok=hidden"));
        assert!(resp_dbg.contains("application/json"));
        assert!(resp_dbg.contains("[REDACTED]"));
    }

    #[test]
    fn ledger_enforces_concurrency_cap_idempotent_cancel_and_drain() {
        let mut ledger = UrlRequestLedger::new();
        let mut ids = Vec::new();
        for _ in 0..MAX_IN_FLIGHT_URL_REQUESTS {
            ids.push(ledger.admit().unwrap());
        }
        assert_eq!(ledger.in_flight_count(), MAX_IN_FLIGHT_URL_REQUESTS);
        assert_eq!(ledger.admit(), Err(UrlRequestError::Backpressure));

        assert!(ledger.mark_cancelling(ids[0]));
        assert!(!ledger.mark_cancelling(ids[0]));

        let draining_cancel = ledger.begin_drain();
        assert_eq!(draining_cancel.len(), MAX_IN_FLIGHT_URL_REQUESTS - 1);
        assert!(ledger.is_draining());
        assert_eq!(ledger.admit(), Err(UrlRequestError::Closing));

        for id in ids {
            assert!(ledger.complete(id));
        }
        assert!(ledger.is_empty());
    }
}
