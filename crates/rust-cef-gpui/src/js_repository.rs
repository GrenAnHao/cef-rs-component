//! Public JS Object Repository SDK (`Task 2.1`).
//!
//! Provides host-side object registration, lazy dynamic object resolution
//! (`ResolveObject`), `BindObjectAsync` handling, callback handles (`JsCallbackHandle`),
//! `Date` marshaling (`JsDate`), lifecycle notifications (`ObjectBoundInJavascript` /
//! `ObjectsBoundInJavascript`), `PostMessage` routing, context cleanup, and the
//! graduated zero-new-dependency JSON parser/encoder ([`JsonVal`], [`JsonParser`]).
//!
//! Mirrors CefSharp's `IJavascriptObjectRepository` / `JavascriptBindingSettings`
//! (`D:\System Dir\Downloads\CefSharp\CefSharp\IJavascriptObjectRepository.cs:1-92`,
//! `D:\System Dir\Downloads\CefSharp\CefSharp\JavascriptBinding\JavascriptBindingSettings.cs:14-140`).

use std::collections::{BTreeMap, VecDeque};

use rust_cef_core::{js_bridge::MAX_JS_JSON_DEPTH, ports::EngineError};
use rust_cef_runtime::{
    JsRepositoryMethodStyle, JsRepositoryObjectDescriptor, JsRepositoryPropertyDescriptor,
    JsRepositorySettings, runtime::Runtime,
};

/// Bounded JSON AST graduated from `cef_example::jsb_rpc` with zero external crate
/// dependencies.
#[derive(Clone, Debug, PartialEq)]
pub enum JsonVal {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<JsonVal>),
    Obj(Vec<(String, JsonVal)>),
}

impl JsonVal {
    pub fn obj_get(&self, key: &str) -> Option<&JsonVal> {
        match self {
            JsonVal::Obj(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsonVal::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            JsonVal::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            JsonVal::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            JsonVal::Num(n)
                if n.is_finite()
                    && n.fract() == 0.0
                    && *n >= i64::MIN as f64
                    && *n < i64::MAX as f64 =>
            {
                Some(*n as i64)
            }
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[JsonVal]> {
        match self {
            JsonVal::Arr(arr) => Some(arr.as_slice()),
            _ => None,
        }
    }

    pub fn as_obj(&self) -> Option<&[(String, JsonVal)]> {
        match self {
            JsonVal::Obj(entries) => Some(entries.as_slice()),
            _ => None,
        }
    }

    pub fn to_json_string(&self) -> String {
        let mut out = String::new();
        self.write_json(&mut out);
        out
    }

    fn write_json(&self, out: &mut String) {
        match self {
            JsonVal::Null => out.push_str("null"),
            JsonVal::Bool(true) => out.push_str("true"),
            JsonVal::Bool(false) => out.push_str("false"),
            JsonVal::Num(n) => {
                if !n.is_finite() {
                    out.push_str("null");
                } else if n.fract() == 0.0 && n.abs() < 1e15 {
                    out.push_str(&format!("{}", *n as i64));
                } else {
                    out.push_str(&format!("{n}"));
                }
            }
            JsonVal::Str(s) => write_json_str(s, out),
            JsonVal::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_json(out);
                }
                out.push(']');
            }
            JsonVal::Obj(entries) => {
                out.push('{');
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_str(k, out);
                    out.push(':');
                    v.write_json(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_json_str(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Depth-bounded recursive-descent JSON parser.
pub struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    pub fn new(input: &'a str) -> Self {
        Self {
            bytes: input.as_bytes(),
            pos: 0,
        }
    }

    fn skip_ws(&mut self) {
        while let Some(&b) = self.bytes.get(self.pos) {
            if matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn parse_val(&mut self, depth: usize) -> Result<JsonVal, String> {
        if depth > MAX_JS_JSON_DEPTH {
            return Err("JSON depth limit exceeded".into());
        }
        self.skip_ws();
        let Some(&b) = self.bytes.get(self.pos) else {
            return Err("Unexpected EOF".into());
        };
        match b {
            b'n' => self.expect_lit(b"null", JsonVal::Null),
            b't' => self.expect_lit(b"true", JsonVal::Bool(true)),
            b'f' => self.expect_lit(b"false", JsonVal::Bool(false)),
            b'"' => self.parse_string().map(JsonVal::Str),
            b'[' => self.parse_array(depth + 1),
            b'{' => self.parse_object(depth + 1),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => Err(format!("Unexpected byte 0x{b:02x}")),
        }
    }

    fn expect_lit(&mut self, lit: &[u8], val: JsonVal) -> Result<JsonVal, String> {
        if self.bytes[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            Ok(val)
        } else {
            Err("Invalid literal".into())
        }
    }

    fn parse_hex4(&mut self) -> Result<u16, String> {
        if self.pos + 4 > self.bytes.len() {
            return Err("Truncated \\u escape".into());
        }
        let s = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
            .map_err(|_| "Invalid UTF-8 in \\u escape".to_owned())?;
        self.pos += 4;
        u16::from_str_radix(s, 16).map_err(|_| "Invalid hex in \\u escape".to_owned())
    }

    fn parse_string(&mut self) -> Result<String, String> {
        if self.bytes.get(self.pos) != Some(&b'"') {
            return Err("Expected '\"'".into());
        }
        self.pos += 1;
        let mut out = String::new();
        while let Some(&b) = self.bytes.get(self.pos) {
            if b == b'"' {
                self.pos += 1;
                return Ok(out);
            }
            if b == b'\\' {
                self.pos += 1;
                let Some(&esc) = self.bytes.get(self.pos) else {
                    return Err("Trailing backslash".into());
                };
                self.pos += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000c}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let code = self.parse_hex4()?;
                        if (0xD800..=0xDBFF).contains(&code)
                            && self.bytes.get(self.pos..self.pos + 2) == Some(b"\\u")
                        {
                            self.pos += 2;
                            let low = self.parse_hex4()?;
                            if (0xDC00..=0xDFFF).contains(&low) {
                                let cp = 0x10000
                                    + (((code as u32 - 0xD800) << 10) | (low as u32 - 0xDC00));
                                if let Some(ch) = char::from_u32(cp) {
                                    out.push(ch);
                                }
                            }
                        } else if let Some(ch) = char::from_u32(code as u32) {
                            out.push(ch);
                        }
                    }
                    _ => return Err("Unknown escape".into()),
                }
            } else if b < 0x80 {
                out.push(b as char);
                self.pos += 1;
            } else {
                let rest = std::str::from_utf8(&self.bytes[self.pos..])
                    .map_err(|_| "Invalid UTF-8 in JSON string".to_owned())?;
                let ch = rest.chars().next().ok_or("EOF")?;
                out.push(ch);
                self.pos += ch.len_utf8();
            }
        }
        Err("Unterminated string".into())
    }

    fn parse_number(&mut self) -> Result<JsonVal, String> {
        let start = self.pos;
        if self.bytes.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        while let Some(&b) = self.bytes.get(self.pos) {
            if b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-') {
                self.pos += 1;
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| "Invalid UTF-8 in number".to_owned())?;
        let num = s
            .parse::<f64>()
            .map_err(|_| format!("Invalid number '{s}'"))?;
        Ok(JsonVal::Num(num))
    }

    fn parse_array(&mut self, depth: usize) -> Result<JsonVal, String> {
        self.pos += 1;
        let mut arr = Vec::new();
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&b']') {
            self.pos += 1;
            return Ok(JsonVal::Arr(arr));
        }
        loop {
            arr.push(self.parse_val(depth)?);
            self.skip_ws();
            match self.bytes.get(self.pos) {
                Some(&b',') => self.pos += 1,
                Some(&b']') => {
                    self.pos += 1;
                    return Ok(JsonVal::Arr(arr));
                }
                _ => return Err("Expected ',' or ']'".into()),
            }
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<JsonVal, String> {
        self.pos += 1;
        let mut map = Vec::new();
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            return Ok(JsonVal::Obj(map));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            if self.bytes.get(self.pos) != Some(&b':') {
                return Err("Expected ':'".into());
            }
            self.pos += 1;
            let val = self.parse_val(depth)?;
            if !matches!(key.as_str(), "__proto__" | "constructor" | "prototype") {
                map.push((key, val));
            }
            self.skip_ws();
            match self.bytes.get(self.pos) {
                Some(&b',') => self.pos += 1,
                Some(&b'}') => {
                    self.pos += 1;
                    return Ok(JsonVal::Obj(map));
                }
                _ => return Err("Expected ',' or '}'".into()),
            }
        }
    }
}

/// Parses a JSON string using the bounded recursive-descent parser.
pub fn parse_json(input: &str) -> Result<JsonVal, String> {
    let mut parser = JsonParser::new(input);
    let val = parser.parse_val(0)?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err("Trailing characters after JSON value".into());
    }
    Ok(val)
}

/// JavaScript `Date` marshaling wrapper (`__cefDateMs` UTC milliseconds or local components).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsDate {
    EpochMillis(i64),
    LocalComponents {
        year: i32,
        month: i32,
        day: i32,
        hour: i32,
        minute: i32,
        second: i32,
    },
}

impl JsDate {
    pub fn to_json_val(self) -> JsonVal {
        match self {
            Self::EpochMillis(ms) => {
                JsonVal::Obj(vec![("__cefDateMs".into(), JsonVal::Num(ms as f64))])
            }
            Self::LocalComponents {
                year,
                month,
                day,
                hour,
                minute,
                second,
            } => JsonVal::Obj(vec![(
                "__cefLocalDate".into(),
                JsonVal::Arr(vec![
                    JsonVal::Num(year as f64),
                    JsonVal::Num(month as f64),
                    JsonVal::Num(day as f64),
                    JsonVal::Num(hour as f64),
                    JsonVal::Num(minute as f64),
                    JsonVal::Num(second as f64),
                ]),
            )]),
        }
    }

    pub fn from_json_val(val: &JsonVal) -> Option<Self> {
        if let Some(ms) = val.obj_get("__cefDateMs").and_then(JsonVal::as_i64) {
            return Some(Self::EpochMillis(ms));
        }
        let arr = val.obj_get("__cefLocalDate").and_then(JsonVal::as_arr)?;
        if arr.len() < 6 {
            return None;
        }
        Some(Self::LocalComponents {
            year: arr[0].as_i64()? as i32,
            month: arr[1].as_i64()? as i32,
            day: arr[2].as_i64()? as i32,
            hour: arr[3].as_i64()? as i32,
            minute: arr[4].as_i64()? as i32,
            second: arr[5].as_i64()? as i32,
        })
    }
}

/// Host handle referencing a renderer-retained JavaScript callback function (`__cefCbId`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsCallbackHandle {
    pub tab_id: u64,
    pub ctx_id: String,
    pub cb_id: String,
}

/// Typed JS bridge value supporting primitives, nested collections, [`JsDate`], and [`JsCallbackHandle`].
#[derive(Clone, Debug, PartialEq)]
pub enum JsValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsValue>),
    Object(Vec<(String, JsValue)>),
    Date(JsDate),
    Callback(JsCallbackHandle),
}

impl JsValue {
    pub fn from_json(tab_id: u64, ctx_id: &str, val: &JsonVal) -> Self {
        match val {
            JsonVal::Null => Self::Null,
            JsonVal::Bool(b) => Self::Bool(*b),
            JsonVal::Num(n) => Self::Number(*n),
            JsonVal::Str(s) => Self::String(s.clone()),
            JsonVal::Arr(arr) => Self::Array(
                arr.iter()
                    .map(|item| Self::from_json(tab_id, ctx_id, item))
                    .collect(),
            ),
            JsonVal::Obj(entries) => {
                if let Some(cb_id) = val.obj_get("__cefCbId").and_then(JsonVal::as_str) {
                    return Self::Callback(JsCallbackHandle {
                        tab_id,
                        ctx_id: ctx_id.to_owned(),
                        cb_id: cb_id.to_owned(),
                    });
                }
                if let Some(date) = JsDate::from_json_val(val) {
                    return Self::Date(date);
                }
                Self::Object(
                    entries
                        .iter()
                        .map(|(k, v)| (k.clone(), Self::from_json(tab_id, ctx_id, v)))
                        .collect(),
                )
            }
        }
    }

    pub fn to_json(&self) -> JsonVal {
        match self {
            Self::Null => JsonVal::Null,
            Self::Bool(b) => JsonVal::Bool(*b),
            Self::Number(n) => JsonVal::Num(*n),
            Self::String(s) => JsonVal::Str(s.clone()),
            Self::Array(items) => JsonVal::Arr(items.iter().map(Self::to_json).collect()),
            Self::Object(entries) => JsonVal::Obj(
                entries
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_json()))
                    .collect(),
            ),
            Self::Date(d) => d.to_json_val(),
            Self::Callback(cb) => {
                JsonVal::Obj(vec![("__cefCbId".into(), JsonVal::Str(cb.cb_id.clone()))])
            }
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Number(n)
                if n.is_finite()
                    && n.fract() == 0.0
                    && *n >= i64::MIN as f64
                    && *n < i64::MAX as f64 =>
            {
                Some(*n as i64)
            }
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[JsValue]> {
        match self {
            Self::Array(arr) => Some(arr.as_slice()),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, JsValue)]> {
        match self {
            Self::Object(entries) => Some(entries.as_slice()),
            _ => None,
        }
    }

    pub fn as_date(&self) -> Option<JsDate> {
        match self {
            Self::Date(d) => Some(*d),
            _ => None,
        }
    }

    pub fn as_callback(&self) -> Option<&JsCallbackHandle> {
        match self {
            Self::Callback(cb) => Some(cb),
            _ => None,
        }
    }

    pub fn obj_get(&self, key: &str) -> Option<&JsValue> {
        match self {
            Self::Object(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

impl From<bool> for JsValue {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

impl From<i64> for JsValue {
    fn from(v: i64) -> Self {
        Self::Number(v as f64)
    }
}

impl From<i32> for JsValue {
    fn from(v: i32) -> Self {
        Self::Number(f64::from(v))
    }
}

impl From<f64> for JsValue {
    fn from(v: f64) -> Self {
        Self::Number(v)
    }
}

impl From<String> for JsValue {
    fn from(v: String) -> Self {
        Self::String(v)
    }
}

impl From<&str> for JsValue {
    fn from(v: &str) -> Self {
        Self::String(v.to_owned())
    }
}

impl From<JsDate> for JsValue {
    fn from(v: JsDate) -> Self {
        Self::Date(v)
    }
}

/// JavaScript property/method name converter policy matching CefSharp's `IJavascriptNameConverter`
/// (`JavascriptBindingSettings.cs:71-73`).
#[derive(Clone, Copy, Debug, Default, Eq)]
pub enum JsNameConverter {
    #[default]
    CamelCase,
    PascalCase,
    Verbatim,
    Custom(fn(&str) -> String),
}

impl PartialEq for JsNameConverter {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::CamelCase, Self::CamelCase)
            | (Self::PascalCase, Self::PascalCase)
            | (Self::Verbatim, Self::Verbatim) => true,
            (Self::Custom(a), Self::Custom(b)) => std::ptr::fn_addr_eq(*a, *b),
            _ => false,
        }
    }
}

impl JsNameConverter {
    pub fn convert(self, name: &str) -> String {
        match self {
            Self::CamelCase => to_camel_case(name),
            Self::PascalCase => to_pascal_case(name),
            Self::Verbatim => name.to_owned(),
            Self::Custom(f) => f(name),
        }
    }
}

fn to_camel_case(name: &str) -> String {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out = String::with_capacity(name.len());
    for c in first.to_lowercase() {
        out.push(c);
    }
    out.push_str(chars.as_str());
    out
}

fn to_pascal_case(name: &str) -> String {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out = String::with_capacity(name.len());
    for c in first.to_uppercase() {
        out.push(c);
    }
    out.push_str(chars.as_str());
    out
}

/// Host-wide JS binding configuration (`JavascriptBindingSettings` parity).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsBindingSettings {
    pub js_binding_api_enabled: bool,
    pub global_object_name: Option<String>,
    pub method_style: JsRepositoryMethodStyle,
    pub legacy_binding_enabled: bool,
    pub name_converter: JsNameConverter,
}

impl Default for JsBindingSettings {
    fn default() -> Self {
        Self {
            js_binding_api_enabled: true,
            global_object_name: None,
            method_style: JsRepositoryMethodStyle::AutoByGlobalName,
            legacy_binding_enabled: false,
            name_converter: JsNameConverter::CamelCase,
        }
    }
}

/// Per-object binding options (`BindingOptions` parity).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsBindingOptions {
    pub is_async: bool,
    pub name_converter: Option<JsNameConverter>,
}

impl Default for JsBindingOptions {
    fn default() -> Self {
        Self {
            is_async: true,
            name_converter: None,
        }
    }
}

/// Property descriptor exported to the renderer V8 bootstrap for a [`BoundObject`].
#[derive(Clone, Debug, PartialEq)]
pub struct BoundProperty {
    pub name: String,
    pub initial_value: JsValue,
    pub readonly: bool,
}

/// Result of invoking a registered Rust method on a [`BoundObject`].
#[derive(Clone, Debug, PartialEq)]
pub enum JsInvocationOutcome {
    /// Reply synchronously to the JS caller via `rpcResult`.
    Immediate(Result<JsValue, String>),
    /// Method handler took ownership of [`JsInvocationToken`] and will complete later via
    /// [`JsObjectRepository::finish_rpc`] or [`JsObjectRepository::fail_rpc`].
    Deferred,
}

/// Token representing an in-flight `rpcCall` that a deferred method handler can complete later.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsInvocationToken {
    pub tab_id: u64,
    pub ctx_id: String,
    pub rpc_id: i64,
}

/// Outbound callback/message request emitted while handling an RPC call or `PostMessage`.
#[derive(Clone, Debug, PartialEq)]
pub struct OutboundCallbackRequest {
    pub cb: JsCallbackHandle,
    pub args: Vec<JsValue>,
    pub dispose: bool,
    pub reply_handler: Option<CallbackReplyKind>,
}

/// Action taken when JS resolves or rejects a callback invoked from Rust (`cbResult`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallbackReplyKind {
    ForwardRpcResult {
        reply_to: JsInvocationToken,
    },
    FormatReturnPrefix {
        reply_to: JsInvocationToken,
        prefix: String,
    },
    EchoErrorPrefix {
        reply_to: JsInvocationToken,
        prefix: String,
    },
    ReportCallbackOutcome {
        result_cb: JsCallbackHandle,
        on_unexpected_ok: String,
        error_prefix: String,
    },
    Ignore,
}

/// Mutable call context passed into [`BoundMethodFn`] implementations.
pub struct JsCallContext<'a> {
    pub token: JsInvocationToken,
    pub target: &'a str,
    pub method: &'a str,
    outbound: &'a mut Vec<(u64, String)>,
    next_reply_id: &'a mut i64,
    pending_callbacks: &'a mut BTreeMap<i64, PendingCallbackEntry>,
}

impl<'a> JsCallContext<'a> {
    /// Builds an `invokeCallback` JSON message without immediately enqueueing it
    /// (useful when a host schedules delayed callback delivery).
    pub fn build_callback_json(
        &mut self,
        cb: &JsCallbackHandle,
        args: Vec<JsValue>,
        dispose: bool,
        reply_kind: Option<CallbackReplyKind>,
    ) -> String {
        let reply_id = reply_kind.map(|kind| {
            let id = *self.next_reply_id;
            *self.next_reply_id = self.next_reply_id.saturating_add(1);
            self.pending_callbacks.insert(
                id,
                PendingCallbackEntry {
                    tab_id: cb.tab_id,
                    ctx_id: cb.ctx_id.clone(),
                    kind,
                },
            );
            id
        });
        let raw_args: Vec<JsonVal> = args.iter().map(JsValue::to_json).collect();
        encode_invoke_callback_json(&cb.ctx_id, &cb.cb_id, raw_args, dispose, reply_id)
    }

    /// Queues an invocation of a JS callback passed in by the renderer.
    pub fn invoke_callback(
        &mut self,
        cb: &JsCallbackHandle,
        args: Vec<JsValue>,
        dispose: bool,
        reply_kind: Option<CallbackReplyKind>,
    ) {
        let msg = self.build_callback_json(cb, args, dispose, reply_kind);
        self.outbound.push((cb.tab_id, msg));
    }

    /// Emits an extra host-to-JS message (for example `postMessageInt`).
    pub fn push_outbound_json(&mut self, tab_id: u64, payload_json: String) {
        self.outbound.push((tab_id, payload_json));
    }
}

pub type BoundMethodFn =
    Box<dyn FnMut(&mut JsCallContext<'_>, Vec<JsValue>) -> JsInvocationOutcome + Send>;

pub type ResolveObjectFn = Box<dyn FnMut(&str) -> Option<BoundObject> + Send>;
pub type ObjectBoundFn = Box<dyn FnMut(u64, &str, bool) + Send>;
pub type ObjectsBoundFn = Box<dyn FnMut(u64, &[String], bool) + Send>;
pub type PostMessageFn = Box<dyn FnMut(&PostMessageEvent, &mut Vec<(u64, String)>) + Send>;

#[derive(Clone, Debug, PartialEq)]
pub struct PostMessageEvent {
    pub tab_id: u64,
    pub ctx_id: String,
    pub payload: JsValue,
    pub raw_payload_json: JsonVal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingCallbackEntry {
    tab_id: u64,
    ctx_id: String,
    kind: CallbackReplyKind,
}

/// Host object descriptor and Rust method handlers registered with [`JsObjectRepository`].
pub struct BoundObject {
    pub name: String,
    pub options: JsBindingOptions,
    pub properties: Vec<BoundProperty>,
    pub sub_objects: BTreeMap<String, BoundObject>,
    methods: BTreeMap<String, BoundMethodFn>,
}

impl BoundObject {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            options: JsBindingOptions::default(),
            properties: Vec::new(),
            sub_objects: BTreeMap::new(),
            methods: BTreeMap::new(),
        }
    }

    pub fn with_options(mut self, options: JsBindingOptions) -> Self {
        self.options = options;
        self
    }

    pub fn with_property(
        mut self,
        name: impl Into<String>,
        initial_value: impl Into<JsValue>,
        readonly: bool,
    ) -> Self {
        self.properties.push(BoundProperty {
            name: name.into(),
            initial_value: initial_value.into(),
            readonly,
        });
        self
    }

    pub fn with_sub_object(mut self, sub: BoundObject) -> Self {
        self.sub_objects.insert(sub.name.clone(), sub);
        self
    }

    /// Registers a synchronous RPC handler whose return value or error resolves the JS Promise.
    pub fn with_method<F>(mut self, name: impl Into<String>, mut handler: F) -> Self
    where
        F: FnMut(&mut JsCallContext<'_>, Vec<JsValue>) -> Result<JsValue, String> + Send + 'static,
    {
        self.methods.insert(
            name.into(),
            Box::new(move |ctx, args| JsInvocationOutcome::Immediate(handler(ctx, args))),
        );
        self
    }

    /// Registers an outcome-returning RPC handler supporting synchronous or [`JsInvocationOutcome::Deferred`] completion.
    pub fn with_raw_method<F>(mut self, name: impl Into<String>, handler: F) -> Self
    where
        F: FnMut(&mut JsCallContext<'_>, Vec<JsValue>) -> JsInvocationOutcome + Send + 'static,
    {
        self.methods.insert(name.into(), Box::new(handler));
        self
    }

    fn resolve_target_mut(&mut self, path: &[&str]) -> Option<&mut BoundObject> {
        let mut cur = self;
        for segment in path {
            cur = cur.sub_objects.get_mut(*segment)?;
        }
        Some(cur)
    }

    /// Converts this host object into the runtime V8 descriptor sent to the subprocess.
    pub fn to_descriptor(
        &self,
        fallback_converter: JsNameConverter,
    ) -> JsRepositoryObjectDescriptor {
        let converter = self.options.name_converter.unwrap_or(fallback_converter);
        let properties = self
            .properties
            .iter()
            .map(|p| JsRepositoryPropertyDescriptor {
                name: converter.convert(&p.name),
                value_json: p.initial_value.to_json().to_json_string(),
                readonly: p.readonly,
            })
            .collect();
        let methods = self.methods.keys().map(|m| converter.convert(m)).collect();
        let sub_objects = self
            .sub_objects
            .values()
            .map(|s| s.to_descriptor(converter))
            .collect();
        JsRepositoryObjectDescriptor {
            name: self.name.clone(),
            is_async: self.options.is_async,
            methods,
            properties,
            sub_objects,
        }
    }

    fn to_descriptor_json_val(&self, fallback_converter: JsNameConverter) -> JsonVal {
        descriptor_to_json_val(&self.to_descriptor(fallback_converter))
    }
}

fn descriptor_to_json_val(desc: &JsRepositoryObjectDescriptor) -> JsonVal {
    let props = desc
        .properties
        .iter()
        .map(|p| {
            JsonVal::Obj(vec![
                ("name".into(), JsonVal::Str(p.name.clone())),
                ("valueJson".into(), JsonVal::Str(p.value_json.clone())),
                ("readonly".into(), JsonVal::Bool(p.readonly)),
            ])
        })
        .collect();
    let methods = desc
        .methods
        .iter()
        .map(|m| JsonVal::Str(m.clone()))
        .collect();
    let subs = desc
        .sub_objects
        .iter()
        .map(descriptor_to_json_val)
        .collect();
    JsonVal::Obj(vec![
        ("name".into(), JsonVal::Str(desc.name.clone())),
        ("isAsync".into(), JsonVal::Bool(desc.is_async)),
        ("methods".into(), JsonVal::Arr(methods)),
        ("properties".into(), JsonVal::Arr(props)),
        ("subObjects".into(), JsonVal::Arr(subs)),
    ])
}

/// Host-side JS Object Repository mirroring CefSharp's `IJavascriptObjectRepository`.
pub struct JsObjectRepository {
    settings: JsBindingSettings,
    objects: BTreeMap<String, BoundObject>,
    resolve_object: Option<ResolveObjectFn>,
    object_bound: Option<ObjectBoundFn>,
    objects_bound: Option<ObjectsBoundFn>,
    post_message: Option<PostMessageFn>,
    bound_by_ctx: BTreeMap<String, (u64, Vec<String>)>,
    pending_callbacks: BTreeMap<i64, PendingCallbackEntry>,
    next_reply_id: i64,
    outbound: VecDeque<(u64, String)>,
}

impl Default for JsObjectRepository {
    fn default() -> Self {
        Self::new(JsBindingSettings::default())
    }
}

impl JsObjectRepository {
    pub fn new(settings: JsBindingSettings) -> Self {
        Self {
            settings,
            objects: BTreeMap::new(),
            resolve_object: None,
            object_bound: None,
            objects_bound: None,
            post_message: None,
            bound_by_ctx: BTreeMap::new(),
            pending_callbacks: BTreeMap::new(),
            next_reply_id: 1,
            outbound: VecDeque::new(),
        }
    }

    pub fn settings(&self) -> &JsBindingSettings {
        &self.settings
    }

    pub fn set_settings(&mut self, settings: JsBindingSettings) {
        self.settings = settings;
    }

    pub fn register(&mut self, object: BoundObject) {
        self.objects.insert(object.name.clone(), object);
    }

    pub fn register_with_options(
        &mut self,
        name: impl Into<String>,
        options: JsBindingOptions,
        build: impl FnOnce(BoundObject) -> BoundObject,
    ) {
        let obj = build(BoundObject::new(name).with_options(options));
        self.register(obj);
    }

    pub fn is_bound(&self, name: &str) -> bool {
        self.objects.contains_key(name)
    }

    pub fn unbind(&mut self, name: &str) -> bool {
        self.objects.remove(name).is_some()
    }

    pub fn unbind_all(&mut self) {
        self.objects.clear();
    }

    pub fn has_bound_objects(&self) -> bool {
        !self.objects.is_empty()
    }

    pub fn set_resolve_object<F>(&mut self, handler: F)
    where
        F: FnMut(&str) -> Option<BoundObject> + Send + 'static,
    {
        self.resolve_object = Some(Box::new(handler));
    }

    pub fn set_object_bound_handler<F>(&mut self, handler: F)
    where
        F: FnMut(u64, &str, bool) + Send + 'static,
    {
        self.object_bound = Some(Box::new(handler));
    }

    pub fn set_objects_bound_handler<F>(&mut self, handler: F)
    where
        F: FnMut(u64, &[String], bool) + Send + 'static,
    {
        self.objects_bound = Some(Box::new(handler));
    }

    pub fn set_post_message_handler<F>(&mut self, handler: F)
    where
        F: FnMut(&PostMessageEvent, &mut Vec<(u64, String)>) + Send + 'static,
    {
        self.post_message = Some(Box::new(handler));
    }

    /// Exports runtime configuration suitable for `Runtime::set_js_repository_settings`.
    pub fn to_runtime_settings(&self) -> JsRepositorySettings {
        let objects = self
            .objects
            .values()
            .map(|o| o.to_descriptor(self.settings.name_converter))
            .collect();
        JsRepositorySettings {
            enabled: self.settings.js_binding_api_enabled,
            global_object_name: self.settings.global_object_name.clone(),
            method_style: self.settings.method_style,
            legacy_binding_enabled: self.settings.legacy_binding_enabled,
            objects,
        }
    }

    /// Applies the repository's configuration onto [`Runtime`] for newly created Browsers.
    pub fn apply_to_runtime(&self, runtime: &Runtime) -> Result<(), EngineError> {
        runtime.set_js_repository_settings(Some(self.to_runtime_settings()))
    }

    /// Drains queued outbound JSON IPC payloads `(tab_id, payload_json)`.
    pub fn drain_outbound(&mut self) -> Vec<(u64, String)> {
        self.outbound.drain(..).collect()
    }

    /// Cleans up pending callbacks and context state associated with `tab_id` (browser close/nav).
    pub fn purge_browser(&mut self, tab_id: u64) {
        self.pending_callbacks
            .retain(|_, entry| entry.tab_id != tab_id);
        self.bound_by_ctx.retain(|_, (tid, _)| *tid != tab_id);
        self.outbound.retain(|(tid, _)| *tid != tab_id);
    }

    /// Cleans up state associated with a single V8 renderer context `ctx_id`.
    pub fn purge_context(&mut self, ctx_id: &str) {
        self.pending_callbacks
            .retain(|_, entry| entry.ctx_id != ctx_id);
        self.bound_by_ctx.remove(ctx_id);
    }

    /// Resolves a deferred RPC call cleanly.
    pub fn finish_rpc(&mut self, token: &JsInvocationToken, value: JsValue) {
        self.outbound.push_back((
            token.tab_id,
            encode_rpc_ok_json(&token.ctx_id, token.rpc_id, value.to_json()),
        ));
    }

    /// Rejects a deferred RPC call cleanly.
    pub fn fail_rpc(&mut self, token: &JsInvocationToken, error: &str) {
        self.outbound.push_back((
            token.tab_id,
            encode_rpc_err_json(&token.ctx_id, token.rpc_id, error),
        ));
    }

    /// Queues a `postMessageInt` frame event used by `PostMessageTest.html`.
    pub fn push_post_message_int(&mut self, tab_id: u64, ctx_id: &str, value: i64) {
        let reply = JsonVal::Obj(vec![
            ("__cefKind".into(), JsonVal::Str("postMessageInt".into())),
            ("ctxId".into(), JsonVal::Str(ctx_id.to_owned())),
            ("value".into(), JsonVal::Num(value as f64)),
        ]);
        self.outbound.push_back((tab_id, reply.to_json_string()));
    }

    /// Queues a JS callback invocation outside an active synchronous method call.
    pub fn invoke_callback(
        &mut self,
        cb: &JsCallbackHandle,
        args: Vec<JsValue>,
        dispose: bool,
        reply_kind: Option<CallbackReplyKind>,
    ) {
        let reply_id = reply_kind.map(|kind| {
            let id = self.next_reply_id;
            self.next_reply_id = self.next_reply_id.saturating_add(1);
            self.pending_callbacks.insert(
                id,
                PendingCallbackEntry {
                    tab_id: cb.tab_id,
                    ctx_id: cb.ctx_id.clone(),
                    kind,
                },
            );
            id
        });
        let raw_args = args.iter().map(JsValue::to_json).collect();
        self.outbound.push_back((
            cb.tab_id,
            encode_invoke_callback_json(&cb.ctx_id, &cb.cb_id, raw_args, dispose, reply_id),
        ));
    }

    /// Processes a batch of raw IPC `(tab_id, raw_json)` envelopes and returns any
    /// unhandled envelopes (or appends outbound messages into `out_messages`).
    pub fn handle_incoming(
        &mut self,
        tab_id: u64,
        raw_json: &str,
        out_messages: &mut Vec<(u64, String)>,
    ) -> bool {
        let Ok(root_val) = parse_json(raw_json) else {
            return false;
        };
        let Some(kind) = root_val.obj_get("__cefKind").and_then(JsonVal::as_str) else {
            return false;
        };
        let ctx_id = root_val
            .obj_get("ctxId")
            .and_then(JsonVal::as_str)
            .unwrap_or("");
        let handled = match kind {
            "bindObject" => {
                self.handle_bind_object(tab_id, ctx_id, &root_val, out_messages);
                true
            }
            "objectsBound" => {
                self.handle_objects_bound(tab_id, ctx_id, &root_val);
                true
            }
            "rpcCall" => {
                self.handle_rpc_call(tab_id, ctx_id, &root_val, out_messages);
                true
            }
            "cbResult" => {
                self.handle_cb_result(&root_val, out_messages);
                true
            }
            "postMessage" => {
                if let Some(handler) = self.post_message.as_mut() {
                    let raw_payload = root_val
                        .obj_get("payload")
                        .cloned()
                        .unwrap_or(JsonVal::Null);
                    let ev = PostMessageEvent {
                        tab_id,
                        ctx_id: ctx_id.to_owned(),
                        payload: JsValue::from_json(tab_id, ctx_id, &raw_payload),
                        raw_payload_json: raw_payload,
                    };
                    handler(&ev, out_messages);
                    true
                } else {
                    false
                }
            }
            _ => false,
        };
        while let Some(msg) = self.outbound.pop_front() {
            out_messages.push(msg);
        }
        handled
    }

    fn handle_bind_object(
        &mut self,
        tab_id: u64,
        ctx_id: &str,
        msg: &JsonVal,
        out_messages: &mut Vec<(u64, String)>,
    ) {
        let req_id = msg.obj_get("id").and_then(JsonVal::as_i64).unwrap_or(0);
        let names: Vec<String> = msg
            .obj_get("names")
            .and_then(JsonVal::as_arr)
            .unwrap_or(&[])
            .iter()
            .filter_map(JsonVal::as_str)
            .map(ToOwned::to_owned)
            .collect();

        let mut descriptors = Vec::new();
        for name in names {
            if !self.objects.contains_key(&name)
                && let Some(resolver) = self.resolve_object.as_mut()
                && let Some(obj) = resolver(&name)
            {
                let mut obj = obj;
                if obj.name != name {
                    obj.name = name.clone();
                }
                self.objects.insert(name.clone(), obj);
            }
            if let Some(obj) = self.objects.get(&name) {
                descriptors.push(obj.to_descriptor_json_val(self.settings.name_converter));
            }
        }
        let ok = !descriptors.is_empty();
        let reply = JsonVal::Obj(vec![
            ("__cefKind".into(), JsonVal::Str("bindResult".into())),
            ("ctxId".into(), JsonVal::Str(ctx_id.to_owned())),
            ("id".into(), JsonVal::Num(req_id as f64)),
            ("ok".into(), JsonVal::Bool(ok)),
            ("objects".into(), JsonVal::Arr(descriptors)),
            (
                "message".into(),
                JsonVal::Str(if ok {
                    "Object(s) bound".to_owned()
                } else {
                    "No matching object found in JsObjectRepository".to_owned()
                }),
            ),
        ]);
        out_messages.push((tab_id, reply.to_json_string()));
    }

    fn handle_objects_bound(&mut self, tab_id: u64, ctx_id: &str, msg: &JsonVal) {
        let already_bound = msg
            .obj_get("alreadyBound")
            .and_then(JsonVal::as_bool)
            .unwrap_or(false);
        let names: Vec<String> = msg
            .obj_get("names")
            .and_then(JsonVal::as_arr)
            .unwrap_or(&[])
            .iter()
            .filter_map(JsonVal::as_str)
            .map(ToOwned::to_owned)
            .collect();
        if !names.is_empty() && !ctx_id.is_empty() {
            let entry = self
                .bound_by_ctx
                .entry(ctx_id.to_owned())
                .or_insert_with(|| (tab_id, Vec::new()));
            entry.0 = tab_id;
            for name in &names {
                if !entry.1.contains(name) {
                    entry.1.push(name.clone());
                }
            }
        }
        if let Some(cb) = self.object_bound.as_mut() {
            for name in &names {
                cb(tab_id, name, already_bound);
            }
        }
        if let Some(cb) = self.objects_bound.as_mut() {
            cb(tab_id, &names, already_bound);
        }
    }

    fn handle_rpc_call(
        &mut self,
        tab_id: u64,
        ctx_id: &str,
        msg: &JsonVal,
        out_messages: &mut Vec<(u64, String)>,
    ) {
        let Some(rpc_id) = msg.obj_get("id").and_then(JsonVal::as_i64) else {
            return;
        };
        let target = msg
            .obj_get("target")
            .and_then(JsonVal::as_str)
            .unwrap_or("");
        let method = msg
            .obj_get("method")
            .and_then(JsonVal::as_str)
            .unwrap_or("");
        let raw_args = msg.obj_get("args").and_then(JsonVal::as_arr).unwrap_or(&[]);
        let args: Vec<JsValue> = raw_args
            .iter()
            .map(|a| JsValue::from_json(tab_id, ctx_id, a))
            .collect();

        let segments: Vec<&str> = target.split('.').collect();
        let Some((root_name, sub_path)) = segments.split_first() else {
            out_messages.push((
                tab_id,
                encode_rpc_err_json(ctx_id, rpc_id, "Missing RPC target"),
            ));
            return;
        };

        let Some(root_obj) = self.objects.get_mut(*root_name) else {
            out_messages.push((
                tab_id,
                encode_rpc_err_json(ctx_id, rpc_id, &format!("Unknown object '{root_name}'")),
            ));
            return;
        };
        let fallback_converter = self.settings.name_converter;
        let obj_converter = root_obj
            .options
            .name_converter
            .unwrap_or(fallback_converter);
        let Some(target_obj) = root_obj.resolve_target_mut(sub_path) else {
            out_messages.push((
                tab_id,
                encode_rpc_err_json(ctx_id, rpc_id, &format!("Unknown target '{target}'")),
            ));
            return;
        };

        let resolved_key = if target_obj.methods.contains_key(method) {
            Some(method.to_owned())
        } else {
            target_obj
                .methods
                .keys()
                .find(|k| obj_converter.convert(k) == method)
                .cloned()
        };

        let Some(handler) = resolved_key.and_then(|k| target_obj.methods.get_mut(&k)) else {
            out_messages.push((
                tab_id,
                encode_rpc_err_json(
                    ctx_id,
                    rpc_id,
                    &format!("Unknown method '{target}.{method}'"),
                ),
            ));
            return;
        };

        let token = JsInvocationToken {
            tab_id,
            ctx_id: ctx_id.to_owned(),
            rpc_id,
        };
        let mut ctx = JsCallContext {
            token: token.clone(),
            target,
            method,
            outbound: out_messages,
            next_reply_id: &mut self.next_reply_id,
            pending_callbacks: &mut self.pending_callbacks,
        };
        match handler(&mut ctx, args) {
            JsInvocationOutcome::Immediate(Ok(val)) => {
                out_messages.push((tab_id, encode_rpc_ok_json(ctx_id, rpc_id, val.to_json())));
            }
            JsInvocationOutcome::Immediate(Err(err)) => {
                out_messages.push((tab_id, encode_rpc_err_json(ctx_id, rpc_id, &err)));
            }
            JsInvocationOutcome::Deferred => {}
        }
    }

    fn handle_cb_result(&mut self, msg: &JsonVal, out_messages: &mut Vec<(u64, String)>) {
        let Some(reply_id) = msg.obj_get("replyId").and_then(JsonVal::as_i64) else {
            return;
        };
        let Some(pending) = self.pending_callbacks.remove(&reply_id) else {
            return;
        };
        let ok = msg
            .obj_get("ok")
            .and_then(JsonVal::as_bool)
            .unwrap_or(false);
        let result = msg.obj_get("result").cloned().unwrap_or(JsonVal::Null);
        let err = msg
            .obj_get("error")
            .and_then(JsonVal::as_str)
            .unwrap_or("Callback error")
            .to_owned();

        match pending.kind {
            CallbackReplyKind::Ignore => {}
            CallbackReplyKind::ForwardRpcResult { reply_to } => {
                let encoded = if ok {
                    encode_rpc_ok_json(&reply_to.ctx_id, reply_to.rpc_id, result)
                } else {
                    encode_rpc_err_json(&reply_to.ctx_id, reply_to.rpc_id, &err)
                };
                out_messages.push((reply_to.tab_id, encoded));
            }
            CallbackReplyKind::FormatReturnPrefix { reply_to, prefix } => {
                let text = if ok {
                    match result {
                        JsonVal::Str(s) => format!("{prefix}{s}"),
                        JsonVal::Null => format!("{prefix}null"),
                        other => format!("{prefix}{}", other.to_json_string()),
                    }
                } else {
                    err
                };
                out_messages.push((
                    reply_to.tab_id,
                    encode_rpc_ok_json(&reply_to.ctx_id, reply_to.rpc_id, JsonVal::Str(text)),
                ));
            }
            CallbackReplyKind::EchoErrorPrefix { reply_to, prefix } => {
                let msg_text = format!("{prefix}{err}");
                let encoded =
                    encode_rpc_ok_json(&reply_to.ctx_id, reply_to.rpc_id, JsonVal::Str(msg_text));
                out_messages.push((reply_to.tab_id, encoded));
            }
            CallbackReplyKind::ReportCallbackOutcome {
                result_cb,
                on_unexpected_ok,
                error_prefix,
            } => {
                let text = if ok {
                    on_unexpected_ok
                } else {
                    format!("{error_prefix}{err}")
                };
                let encoded = encode_invoke_callback_json(
                    &result_cb.ctx_id,
                    &result_cb.cb_id,
                    vec![JsonVal::Str(text)],
                    true,
                    None,
                );
                out_messages.push((result_cb.tab_id, encoded));
            }
        }
    }
}

pub fn encode_rpc_ok_json(ctx_id: &str, id: i64, result: JsonVal) -> String {
    JsonVal::Obj(vec![
        ("__cefKind".into(), JsonVal::Str("rpcResult".into())),
        ("ctxId".into(), JsonVal::Str(ctx_id.to_owned())),
        ("id".into(), JsonVal::Num(id as f64)),
        ("ok".into(), JsonVal::Bool(true)),
        ("result".into(), result),
    ])
    .to_json_string()
}

pub fn encode_rpc_err_json(ctx_id: &str, id: i64, error: &str) -> String {
    JsonVal::Obj(vec![
        ("__cefKind".into(), JsonVal::Str("rpcResult".into())),
        ("ctxId".into(), JsonVal::Str(ctx_id.to_owned())),
        ("id".into(), JsonVal::Num(id as f64)),
        ("ok".into(), JsonVal::Bool(false)),
        ("error".into(), JsonVal::Str(error.to_owned())),
    ])
    .to_json_string()
}

pub fn encode_invoke_callback_json(
    ctx_id: &str,
    cb_id: &str,
    args: Vec<JsonVal>,
    dispose: bool,
    reply_id: Option<i64>,
) -> String {
    let mut fields = vec![
        ("__cefKind".into(), JsonVal::Str("invokeCallback".into())),
        ("ctxId".into(), JsonVal::Str(ctx_id.to_owned())),
        ("cbId".into(), JsonVal::Str(cb_id.to_owned())),
        ("args".into(), JsonVal::Arr(args)),
        ("dispose".into(), JsonVal::Bool(dispose)),
    ];
    if let Some(rid) = reply_id {
        fields.push(("replyId".into(), JsonVal::Num(rid as f64)));
    }
    JsonVal::Obj(fields).to_json_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn json_parser_roundtrip_escapes_surrogates_and_depth_limit() {
        let raw = r#"{"a":123,"b":-4.5,"c":true,"d":null,"arr":[1,"hi\n\t\"\\",{"__proto__":1,"ok":2}],"u":"\u4e2d\uD83D\uDE00"}"#;
        let val = parse_json(raw).expect("parse");
        assert_eq!(val.obj_get("a").and_then(JsonVal::as_i64), Some(123));
        assert_eq!(val.obj_get("b").and_then(JsonVal::as_f64), Some(-4.5));
        assert_eq!(val.obj_get("c").and_then(JsonVal::as_bool), Some(true));
        assert_eq!(val.obj_get("u").and_then(JsonVal::as_str), Some("中😀"));
        let inner_obj = &val.obj_get("arr").and_then(JsonVal::as_arr).expect("arr")[2];
        assert!(inner_obj.obj_get("__proto__").is_none());
        assert_eq!(inner_obj.obj_get("ok").and_then(JsonVal::as_i64), Some(2));
        let reencoded = val.to_json_string();
        assert_eq!(parse_json(&reencoded).expect("reparse"), val);

        let too_deep = format!(
            "{}1{}",
            "[".repeat(MAX_JS_JSON_DEPTH + 2),
            "]".repeat(MAX_JS_JSON_DEPTH + 2)
        );
        assert!(parse_json(&too_deep).is_err());
    }

    #[test]
    fn repository_handles_resolve_object_binding_callbacks_dates_and_cleanup() {
        let mut repo = JsObjectRepository::new(JsBindingSettings {
            global_object_name: Some("customApi".into()),
            method_style: JsRepositoryMethodStyle::CamelCase,
            legacy_binding_enabled: true,
            ..Default::default()
        });
        let bound_log: Arc<Mutex<Vec<(u64, String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        let log_clone = Arc::clone(&bound_log);
        repo.set_object_bound_handler(move |tab_id, name, already| {
            log_clone
                .lock()
                .unwrap()
                .push((tab_id, name.to_owned(), already));
        });
        repo.set_resolve_object(|name| {
            if name == "math" {
                Some(
                    BoundObject::new("math")
                        .with_property("Version", 7, true)
                        .with_method("Add", |_ctx, args| {
                            let a = args.first().and_then(JsValue::as_i64).unwrap_or(0);
                            let b = args.get(1).and_then(JsValue::as_i64).unwrap_or(0);
                            Ok(JsValue::from(a + b))
                        })
                        .with_method("EchoDate", |_ctx, args| {
                            Ok(args.into_iter().next().unwrap_or(JsValue::Null))
                        })
                        .with_raw_method("RunCb", |ctx, args| {
                            let Some(cb) = args.first().and_then(JsValue::as_callback).cloned()
                            else {
                                return JsInvocationOutcome::Immediate(Err("missing cb".into()));
                            };
                            ctx.invoke_callback(
                                &cb,
                                vec![JsValue::from("arg1")],
                                true,
                                Some(CallbackReplyKind::FormatReturnPrefix {
                                    reply_to: ctx.token.clone(),
                                    prefix: "Returned: ".into(),
                                }),
                            );
                            JsInvocationOutcome::Deferred
                        }),
                )
            } else {
                None
            }
        });

        let mut out = Vec::new();
        let bind_req = JsonVal::Obj(vec![
            ("__cefKind".into(), JsonVal::Str("bindObject".into())),
            ("ctxId".into(), JsonVal::Str("ctx_1".into())),
            ("id".into(), JsonVal::Num(1.0)),
            (
                "names".into(),
                JsonVal::Arr(vec![
                    JsonVal::Str("math".into()),
                    JsonVal::Str("missing".into()),
                ]),
            ),
        ])
        .to_json_string();
        assert!(repo.handle_incoming(9, &bind_req, &mut out));
        assert_eq!(out.len(), 1);
        let bind_resp = parse_json(&out.pop().unwrap().1).expect("bind resp");
        assert_eq!(
            bind_resp.obj_get("ok").and_then(JsonVal::as_bool),
            Some(true)
        );
        let objs = bind_resp
            .obj_get("objects")
            .and_then(JsonVal::as_arr)
            .expect("objects");
        assert_eq!(objs.len(), 1);
        assert_eq!(
            objs[0].obj_get("name").and_then(JsonVal::as_str),
            Some("math")
        );
        let props = objs[0]
            .obj_get("properties")
            .and_then(JsonVal::as_arr)
            .unwrap();
        assert_eq!(
            props[0].obj_get("name").and_then(JsonVal::as_str),
            Some("version")
        );
        assert!(repo.is_bound("math"));

        // Notify ObjectsBoundInJavascript
        let bound_note = JsonVal::Obj(vec![
            ("__cefKind".into(), JsonVal::Str("objectsBound".into())),
            ("ctxId".into(), JsonVal::Str("ctx_1".into())),
            (
                "names".into(),
                JsonVal::Arr(vec![JsonVal::Str("math".into())]),
            ),
            ("alreadyBound".into(), JsonVal::Bool(false)),
        ])
        .to_json_string();
        assert!(repo.handle_incoming(9, &bound_note, &mut out));
        assert_eq!(
            *bound_log.lock().unwrap(),
            vec![(9, "math".to_owned(), false)]
        );

        // RPC call with camelCase lookup + callback + cbResult roundtrip
        let cb_call = JsonVal::Obj(vec![
            ("__cefKind".into(), JsonVal::Str("rpcCall".into())),
            ("ctxId".into(), JsonVal::Str("ctx_1".into())),
            ("id".into(), JsonVal::Num(11.0)),
            ("target".into(), JsonVal::Str("math".into())),
            ("method".into(), JsonVal::Str("runCb".into())),
            (
                "args".into(),
                JsonVal::Arr(vec![JsonVal::Obj(vec![(
                    "__cefCbId".into(),
                    JsonVal::Str("cb_7".into()),
                )])]),
            ),
        ])
        .to_json_string();
        assert!(repo.handle_incoming(9, &cb_call, &mut out));
        let invoke_msg = parse_json(&out.pop().unwrap().1).expect("invokeCallback");
        assert_eq!(
            invoke_msg.obj_get("__cefKind").and_then(JsonVal::as_str),
            Some("invokeCallback")
        );
        let reply_id = invoke_msg
            .obj_get("replyId")
            .and_then(JsonVal::as_i64)
            .unwrap();

        let cb_res = JsonVal::Obj(vec![
            ("__cefKind".into(), JsonVal::Str("cbResult".into())),
            ("ctxId".into(), JsonVal::Str("ctx_1".into())),
            ("replyId".into(), JsonVal::Num(reply_id as f64)),
            ("ok".into(), JsonVal::Bool(true)),
            ("result".into(), JsonVal::Str("ok42".into())),
        ])
        .to_json_string();
        assert!(repo.handle_incoming(9, &cb_res, &mut out));
        let rpc_res = parse_json(&out.pop().unwrap().1).expect("rpcResult");
        assert_eq!(
            rpc_res.obj_get("result").and_then(JsonVal::as_str),
            Some("Returned: ok42")
        );

        // Date roundtrip
        let date_call = JsonVal::Obj(vec![
            ("__cefKind".into(), JsonVal::Str("rpcCall".into())),
            ("ctxId".into(), JsonVal::Str("ctx_1".into())),
            ("id".into(), JsonVal::Num(12.0)),
            ("target".into(), JsonVal::Str("math".into())),
            ("method".into(), JsonVal::Str("echoDate".into())),
            (
                "args".into(),
                JsonVal::Arr(vec![JsDate::EpochMillis(1_700_000_000_000).to_json_val()]),
            ),
        ])
        .to_json_string();
        assert!(repo.handle_incoming(9, &date_call, &mut out));
        let date_res = parse_json(&out.pop().unwrap().1).expect("date result");
        assert_eq!(
            date_res
                .obj_get("result")
                .and_then(|v| v.obj_get("__cefDateMs"))
                .and_then(JsonVal::as_i64),
            Some(1_700_000_000_000)
        );

        // Unbind and browser cleanup
        repo.purge_browser(9);
        assert!(repo.unbind("math"));
        assert!(!repo.has_bound_objects());
    }
}
