//! Pure contracts, bounded JSON validation, and deterministic evaluation
//! ledger for cross-process JavaScript execution and messaging.

use std::collections::BTreeMap;
use std::fmt;

use crate::contracts::{BrowserId, CreateError};

pub const MAX_JS_SCRIPT_BYTES: usize = 256 * 1024;
pub const MAX_JS_PAYLOAD_BYTES: usize = 256 * 1024;
pub const MAX_JS_JSON_DEPTH: usize = 32;
pub const MAX_PENDING_EVALS_PER_BROWSER: usize = 64;
pub const MAX_JS_MESSAGE_QUEUE_PER_BROWSER: usize = 64;
pub const DEFAULT_EVAL_TIMEOUT_MS: u64 = 5_000;
pub const MAX_EVAL_TIMEOUT_MS: u64 = 30_000;
pub const MAX_EVAL_ERROR_MESSAGE_BYTES: usize = 512;

/// Explicit opt-in configuration for per-browser cross-process JS bridging.
/// Disabled by default: when `enabled == false`, no V8 bridge object is injected
/// into any frame and bidirectional `postMessage` is rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsBridgeConfig {
    pub enabled: bool,
    pub allowed_origins: Vec<String>,
    pub max_message_bytes: usize,
    pub max_pending_evals: usize,
}

impl Default for JsBridgeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed_origins: Vec::new(),
            max_message_bytes: MAX_JS_PAYLOAD_BYTES,
            max_pending_evals: MAX_PENDING_EVALS_PER_BROWSER,
        }
    }
}

impl JsBridgeConfig {
    pub fn validate(&self) -> Result<(), CreateError> {
        if self.max_message_bytes == 0 || self.max_message_bytes > MAX_JS_PAYLOAD_BYTES {
            return Err(CreateError::InvalidOptions);
        }
        if self.max_pending_evals == 0 || self.max_pending_evals > MAX_PENDING_EVALS_PER_BROWSER {
            return Err(CreateError::InvalidOptions);
        }
        if self.allowed_origins.len() > 64 {
            return Err(CreateError::InvalidOptions);
        }
        for origin in &self.allowed_origins {
            if origin.trim().is_empty()
                || origin.len() > 512
                || origin.bytes().any(|b| b < 0x20 || b == 0x7f)
            {
                return Err(CreateError::InvalidOptions);
            }
        }
        Ok(())
    }
}

/// Unique host-allocated request identity for one `EvaluateScript` operation.
/// Monotonically allocated per Runtime; never reused across generations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EvalRequestId {
    pub browser: BrowserId,
    pub sequence: u64,
}

/// Target frame scope for `ExecuteJavaScript` / `EvaluateScript`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum JsFrameScope {
    #[default]
    MainFrame,
    FrameId(String),
}

#[derive(Clone, PartialEq, Eq)]
pub enum EvalError {
    Disabled,
    PolicyDenied,
    InvalidScript,
    PayloadTooLarge,
    InvalidJson,
    DepthExceeded,
    UnsupportedResultType,
    QueueFull,
    JsException { message: String, line: i32 },
    Navigated,
    RendererTerminated,
    Closed,
    Timeout,
    FrameNotFound,
}

impl fmt::Debug for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => f.write_str("Disabled"),
            Self::PolicyDenied => f.write_str("PolicyDenied"),
            Self::InvalidScript => f.write_str("InvalidScript"),
            Self::PayloadTooLarge => f.write_str("PayloadTooLarge"),
            Self::InvalidJson => f.write_str("InvalidJson"),
            Self::DepthExceeded => f.write_str("DepthExceeded"),
            Self::UnsupportedResultType => f.write_str("UnsupportedResultType"),
            Self::QueueFull => f.write_str("QueueFull"),
            Self::JsException { message, line } => f
                .debug_struct("JsException")
                .field("message_bytes", &message.len())
                .field("line", line)
                .finish(),
            Self::Navigated => f.write_str("Navigated"),
            Self::RendererTerminated => f.write_str("RendererTerminated"),
            Self::Closed => f.write_str("Closed"),
            Self::Timeout => f.write_str("Timeout"),
            Self::FrameNotFound => f.write_str("FrameNotFound"),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum EvalOutcome {
    Value(String),
    Err(EvalError),
}

impl fmt::Debug for EvalOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Value(json) => f
                .debug_struct("Value")
                .field("json_bytes", &json.len())
                .finish(),
            Self::Err(err) => f.debug_tuple("Err").field(err).finish(),
        }
    }
}

pub fn validate_script(script: &str) -> Result<(), EvalError> {
    if script.trim().is_empty() || script.bytes().any(|b| b == 0) {
        return Err(EvalError::InvalidScript);
    }
    if script.len() > MAX_JS_SCRIPT_BYTES {
        return Err(EvalError::PayloadTooLarge);
    }
    Ok(())
}

pub fn normalize_eval_timeout_ms(timeout_ms: u64) -> u64 {
    if timeout_ms == 0 {
        DEFAULT_EVAL_TIMEOUT_MS
    } else {
        timeout_ms.min(MAX_EVAL_TIMEOUT_MS)
    }
}

/// Validates that `input` is strictly valid RFC 8259 JSON whose UTF-8 byte
/// length does not exceed `max_bytes` and container nesting depth does not
/// exceed `max_depth`. Uses an explicit stack so deeply nested payloads cannot
/// overflow the call stack.
pub fn validate_bounded_json(
    input: &str,
    max_bytes: usize,
    max_depth: usize,
) -> Result<(), EvalError> {
    if input.len() > max_bytes {
        return Err(EvalError::PayloadTooLarge);
    }
    let bytes = input.as_bytes();
    let mut pos = 0usize;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Container {
        Object,
        Array,
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Expect {
        Value,
        AfterValue,
        ObjectKeyOrEnd,
        ObjectKey,
        ObjectColon,
        ArrayValueOrEnd,
    }

    let mut stack: Vec<Container> = Vec::new();
    let mut state = Expect::Value;

    let skip_ws = |p: &mut usize| {
        while *p < bytes.len() && matches!(bytes[*p], b' ' | b'\t' | b'\r' | b'\n') {
            *p += 1;
        }
    };

    loop {
        skip_ws(&mut pos);
        match state {
            Expect::Value | Expect::ArrayValueOrEnd => {
                if pos >= bytes.len() {
                    return Err(EvalError::InvalidJson);
                }
                if state == Expect::ArrayValueOrEnd && bytes[pos] == b']' {
                    pos += 1;
                    stack.pop();
                    state = Expect::AfterValue;
                    continue;
                }
                match bytes[pos] {
                    b'{' => {
                        if stack.len() >= max_depth {
                            return Err(EvalError::DepthExceeded);
                        }
                        pos += 1;
                        stack.push(Container::Object);
                        state = Expect::ObjectKeyOrEnd;
                    }
                    b'[' => {
                        if stack.len() >= max_depth {
                            return Err(EvalError::DepthExceeded);
                        }
                        pos += 1;
                        stack.push(Container::Array);
                        state = Expect::ArrayValueOrEnd;
                    }
                    b'"' => {
                        pos = scan_json_string(bytes, pos)?;
                        state = Expect::AfterValue;
                    }
                    b't' => {
                        if !bytes[pos..].starts_with(b"true") {
                            return Err(EvalError::InvalidJson);
                        }
                        pos += 4;
                        state = Expect::AfterValue;
                    }
                    b'f' => {
                        if !bytes[pos..].starts_with(b"false") {
                            return Err(EvalError::InvalidJson);
                        }
                        pos += 5;
                        state = Expect::AfterValue;
                    }
                    b'n' => {
                        if !bytes[pos..].starts_with(b"null") {
                            return Err(EvalError::InvalidJson);
                        }
                        pos += 4;
                        state = Expect::AfterValue;
                    }
                    b'-' | b'0'..=b'9' => {
                        pos = scan_json_number(bytes, pos)?;
                        state = Expect::AfterValue;
                    }
                    _ => return Err(EvalError::InvalidJson),
                }
            }
            Expect::ObjectKeyOrEnd => {
                if pos >= bytes.len() {
                    return Err(EvalError::InvalidJson);
                }
                if bytes[pos] == b'}' {
                    pos += 1;
                    stack.pop();
                    state = Expect::AfterValue;
                } else {
                    state = Expect::ObjectKey;
                }
            }
            Expect::ObjectKey => {
                if pos >= bytes.len() || bytes[pos] != b'"' {
                    return Err(EvalError::InvalidJson);
                }
                pos = scan_json_string(bytes, pos)?;
                state = Expect::ObjectColon;
            }
            Expect::ObjectColon => {
                if pos >= bytes.len() || bytes[pos] != b':' {
                    return Err(EvalError::InvalidJson);
                }
                pos += 1;
                state = Expect::Value;
            }
            Expect::AfterValue => match stack.last().copied() {
                None => {
                    skip_ws(&mut pos);
                    return if pos == bytes.len() {
                        Ok(())
                    } else {
                        Err(EvalError::InvalidJson)
                    };
                }
                Some(Container::Object) => {
                    if pos >= bytes.len() {
                        return Err(EvalError::InvalidJson);
                    }
                    match bytes[pos] {
                        b',' => {
                            pos += 1;
                            state = Expect::ObjectKey;
                        }
                        b'}' => {
                            pos += 1;
                            stack.pop();
                            state = Expect::AfterValue;
                        }
                        _ => return Err(EvalError::InvalidJson),
                    }
                }
                Some(Container::Array) => {
                    if pos >= bytes.len() {
                        return Err(EvalError::InvalidJson);
                    }
                    match bytes[pos] {
                        b',' => {
                            pos += 1;
                            state = Expect::Value;
                        }
                        b']' => {
                            pos += 1;
                            stack.pop();
                            state = Expect::AfterValue;
                        }
                        _ => return Err(EvalError::InvalidJson),
                    }
                }
            },
        }
    }
}

fn scan_json_string(bytes: &[u8], start: usize) -> Result<usize, EvalError> {
    let mut i = start + 1;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' {
            return Ok(i + 1);
        }
        if b < 0x20 {
            return Err(EvalError::InvalidJson);
        }
        if b == b'\\' {
            i += 1;
            if i >= bytes.len() {
                return Err(EvalError::InvalidJson);
            }
            match bytes[i] {
                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {
                    i += 1;
                }
                b'u' => {
                    i += 1;
                    if i + 4 > bytes.len() || !bytes[i..i + 4].iter().all(|c| c.is_ascii_hexdigit())
                    {
                        return Err(EvalError::InvalidJson);
                    }
                    i += 4;
                }
                _ => return Err(EvalError::InvalidJson),
            }
        } else {
            i += 1;
        }
    }
    Err(EvalError::InvalidJson)
}

fn scan_json_number(bytes: &[u8], start: usize) -> Result<usize, EvalError> {
    let mut i = start;
    if bytes[i] == b'-' {
        i += 1;
        if i >= bytes.len() {
            return Err(EvalError::InvalidJson);
        }
    }
    match bytes[i] {
        b'0' => {
            i += 1;
        }
        b'1'..=b'9' => {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
        _ => return Err(EvalError::InvalidJson),
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        if i >= bytes.len() || !bytes[i].is_ascii_digit() {
            return Err(EvalError::InvalidJson);
        }
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
        i += 1;
        if i < bytes.len() && matches!(bytes[i], b'+' | b'-') {
            i += 1;
        }
        if i >= bytes.len() || !bytes[i].is_ascii_digit() {
            return Err(EvalError::InvalidJson);
        }
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    Ok(i)
}

#[derive(Clone, Debug)]
struct PendingEval {
    id: EvalRequestId,
    document_epoch: u64,
}

/// Deterministic bounded ledger for asynchronous `EvaluateScript` calls.
/// Enforces per-browser pending capacity, generation + document epoch matching,
/// and single terminal emission per request.
pub struct EvalLedger {
    next_sequence: u64,
    pending: BTreeMap<EvalRequestId, PendingEval>,
    stale_dropped: u64,
    completed_count: u64,
    cancelled_count: u64,
}

impl Default for EvalLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl EvalLedger {
    pub fn new() -> Self {
        Self {
            next_sequence: 1,
            pending: BTreeMap::new(),
            stale_dropped: 0,
            completed_count: 0,
            cancelled_count: 0,
        }
    }

    pub fn pending_count(&self) -> u64 {
        self.pending.len() as u64
    }

    pub fn contains(&self, id: EvalRequestId) -> bool {
        self.pending.contains_key(&id)
    }

    pub fn pending_for(&self, browser: BrowserId) -> usize {
        self.pending
            .range(
                EvalRequestId {
                    browser,
                    sequence: 0,
                }..=EvalRequestId {
                    browser,
                    sequence: u64::MAX,
                },
            )
            .count()
    }

    pub fn stale_dropped(&self) -> u64 {
        self.stale_dropped
    }

    pub fn completed_count(&self) -> u64 {
        self.completed_count
    }

    pub fn cancelled_count(&self) -> u64 {
        self.cancelled_count
    }

    pub fn register(
        &mut self,
        browser: BrowserId,
        document_epoch: u64,
        max_per_browser: usize,
    ) -> Result<EvalRequestId, EvalError> {
        let cap = max_per_browser.min(MAX_PENDING_EVALS_PER_BROWSER);
        if cap == 0 || self.pending_for(browser) >= cap {
            return Err(EvalError::QueueFull);
        }
        if self.next_sequence == 0 {
            return Err(EvalError::QueueFull);
        }
        let sequence = self.next_sequence;
        self.next_sequence = sequence.checked_add(1).unwrap_or(0);
        let id = EvalRequestId { browser, sequence };
        self.pending.insert(id, PendingEval { id, document_epoch });
        Ok(id)
    }

    /// Removes an unposted reservation without counting cancellation or completion.
    pub fn rollback(&mut self, id: EvalRequestId) -> bool {
        self.pending.remove(&id).is_some()
    }

    /// Completes `id` at most once. If `expected_epoch` is provided and differs
    /// from the epoch captured when registered, or if `id` is unknown/already
    /// retired, increments `stale_dropped` and returns `None`.
    pub fn complete(
        &mut self,
        id: EvalRequestId,
        expected_epoch: Option<u64>,
        outcome: EvalOutcome,
    ) -> Option<(EvalRequestId, EvalOutcome)> {
        let Some(entry_epoch) = self.pending.get(&id).map(|e| e.document_epoch) else {
            self.stale_dropped = self.stale_dropped.saturating_add(1);
            return None;
        };
        if expected_epoch.is_some_and(|epoch| epoch != entry_epoch) {
            self.stale_dropped = self.stale_dropped.saturating_add(1);
            return None;
        }
        let entry = self.pending.remove(&id)?;
        self.completed_count = self.completed_count.saturating_add(1);
        Some((entry.id, outcome))
    }

    pub fn cancel_for_browser(
        &mut self,
        browser: BrowserId,
        reason: EvalError,
    ) -> Vec<(EvalRequestId, EvalOutcome)> {
        let keys: Vec<_> = self
            .pending
            .range(
                EvalRequestId {
                    browser,
                    sequence: 0,
                }..=EvalRequestId {
                    browser,
                    sequence: u64::MAX,
                },
            )
            .map(|(k, _)| *k)
            .collect();
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(entry) = self.pending.remove(&key) {
                self.cancelled_count = self.cancelled_count.saturating_add(1);
                out.push((entry.id, EvalOutcome::Err(reason.clone())));
            }
        }
        out
    }

    pub fn cancel_all(&mut self, reason: EvalError) -> Vec<(EvalRequestId, EvalOutcome)> {
        let removed = std::mem::take(&mut self.pending);
        let count = removed.len() as u64;
        self.cancelled_count = self.cancelled_count.saturating_add(count);
        removed
            .into_values()
            .map(|entry| (entry.id, EvalOutcome::Err(reason.clone())))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::RuntimeGeneration;

    fn browser(gen_val: u64, local: u64) -> BrowserId {
        BrowserId::new(RuntimeGeneration::new(gen_val), local)
    }

    #[test]
    fn ledger_enforces_queue_cap_and_single_completion() {
        let mut ledger = EvalLedger::new();
        let b1 = browser(1, 1);
        let b2 = browser(1, 2);

        let mut ids = Vec::new();
        for _ in 0..MAX_PENDING_EVALS_PER_BROWSER {
            ids.push(
                ledger
                    .register(b1, 1, MAX_PENDING_EVALS_PER_BROWSER)
                    .expect("within queue limit"),
            );
        }
        assert_eq!(ledger.pending_for(b1), MAX_PENDING_EVALS_PER_BROWSER);
        assert_eq!(
            ledger.register(b1, 1, MAX_PENDING_EVALS_PER_BROWSER),
            Err(EvalError::QueueFull)
        );
        // Independent browser still has full capacity.
        let b2_id = ledger
            .register(b2, 3, MAX_PENDING_EVALS_PER_BROWSER)
            .expect("peer browser unaffected");

        // Completing twice or with wrong epoch drops the late reply.
        let first = ids[0];
        assert!(
            ledger
                .complete(first, Some(2), EvalOutcome::Value("1".into()))
                .is_none()
        );
        assert_eq!(ledger.stale_dropped(), 1);
        let resolved = ledger
            .complete(first, Some(1), EvalOutcome::Value("42".into()))
            .expect("exact match resolves");
        assert_eq!(resolved.0, first);
        assert!(
            ledger
                .complete(first, Some(1), EvalOutcome::Value("42".into()))
                .is_none()
        );
        assert_eq!(ledger.stale_dropped(), 2);

        // Cancel remaining b1 entries on navigation without touching b2.
        let cancelled = ledger.cancel_for_browser(b1, EvalError::Navigated);
        assert_eq!(cancelled.len(), MAX_PENDING_EVALS_PER_BROWSER - 1);
        assert_eq!(ledger.pending_for(b1), 0);
        assert_eq!(ledger.pending_for(b2), 1);

        // Late reply for cancelled id or foreign generation is dropped.
        assert!(
            ledger
                .complete(ids[1], Some(1), EvalOutcome::Value("late".into()))
                .is_none()
        );
        let foreign = EvalRequestId {
            browser: browser(2, 2),
            sequence: b2_id.sequence,
        };
        assert!(
            ledger
                .complete(foreign, Some(3), EvalOutcome::Value("foreign".into()))
                .is_none()
        );
        assert_eq!(ledger.pending_count(), 1);
    }

    #[test]
    fn json_validation_enforces_syntax_depth_and_byte_limits() {
        assert_eq!(
            validate_bounded_json(
                r#"{"ok":true,"arr":[1,-2.5e+1,null,"hi\n\u0041"]}"#,
                1024,
                32
            ),
            Ok(())
        );
        assert_eq!(
            validate_bounded_json("", 1024, 32),
            Err(EvalError::InvalidJson)
        );
        assert_eq!(
            validate_bounded_json(r#"{"a":1,}"#, 1024, 32),
            Err(EvalError::InvalidJson)
        );
        assert_eq!(
            validate_bounded_json(r#"[1] extra"#, 1024, 32),
            Err(EvalError::InvalidJson)
        );
        assert_eq!(
            validate_bounded_json(r#""12345""#, 4, 32),
            Err(EvalError::PayloadTooLarge)
        );

        let depth_32 = format!("{}{}", "[".repeat(32), "]".repeat(32));
        assert_eq!(
            validate_bounded_json(&depth_32, MAX_JS_PAYLOAD_BYTES, MAX_JS_JSON_DEPTH),
            Ok(())
        );
        let depth_33 = format!("{}{}", "[".repeat(33), "]".repeat(33));
        assert_eq!(
            validate_bounded_json(&depth_33, MAX_JS_PAYLOAD_BYTES, MAX_JS_JSON_DEPTH),
            Err(EvalError::DepthExceeded)
        );
    }

    #[test]
    fn debug_formats_redact_script_payload_and_exception_text() {
        let outcome = EvalOutcome::Value("SECRET_EVAL_VALUE_ABC".into());
        let formatted = format!("{outcome:?}");
        assert!(!formatted.contains("SECRET_EVAL_VALUE_ABC"));
        assert!(formatted.contains("json_bytes"));

        let err = EvalError::JsException {
            message: "SECRET_STACK_DETAIL".into(),
            line: 7,
        };
        let formatted_err = format!("{err:?}");
        assert!(!formatted_err.contains("SECRET_STACK_DETAIL"));
        assert!(formatted_err.contains("message_bytes"));
    }
}
