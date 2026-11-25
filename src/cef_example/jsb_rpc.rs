//! Host-side `cef-rs-gpui` JS Object Repository RPC dispatcher and minimal RFC-8259
//! JSON codec for `PostMessage`, `boundAsync`, `boundAsync2`, and `bound` async callbacks.

use std::collections::{BTreeMap, VecDeque};

use gpui::{AsyncApp, WindowHandle};
use rust_cef_gpui::{
    Runtime, UrlRequestState, UrlRequestTicket,
    url_request_contracts::{UrlRequestSpec, UrlRequestStatus, UrlRequestTarget},
};

use super::ExampleAction;

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
            Self::Obj(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Num(n) if n.is_finite() => {
                let rounded = n.round();
                if (n - rounded).abs() < 1e-9 && rounded >= i64::MIN as f64 && rounded <= i64::MAX as f64 {
                    Some(rounded as i64)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Num(n) if n.is_finite() => Some(*n),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[JsonVal]> {
        match self {
            Self::Arr(v) => Some(v.as_slice()),
            _ => None,
        }
    }

    pub fn to_json_string(&self) -> String {
        let mut out = String::new();
        self.write_to(&mut out);
        out
    }

    fn write_to(&self, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Self::Num(n) => {
                if !n.is_finite() {
                    out.push_str("null");
                } else if (n - n.round()).abs() < 1e-12 && n.abs() <= 9_007_199_254_740_991.0 {
                    out.push_str(&format!("{}", *n as i64));
                } else {
                    out.push_str(&format!("{n}"));
                }
            }
            Self::Str(s) => write_json_str(s, out),
            Self::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_to(out);
                }
                out.push(']');
            }
            Self::Obj(entries) => {
                out.push('{');
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_str(k, out);
                    out.push(':');
                    v.write_to(out);
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
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
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

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            bytes: input.as_bytes(),
            pos: 0,
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn parse_val(&mut self, depth: usize) -> Result<JsonVal, String> {
        if depth > 48 {
            return Err("json depth exceeded".into());
        }
        self.skip_ws();
        let Some(b) = self.peek() else {
            return Err("unexpected eof".into());
        };
        match b {
            b'n' => self.expect_lit(b"null", JsonVal::Null),
            b't' => self.expect_lit(b"true", JsonVal::Bool(true)),
            b'f' => self.expect_lit(b"false", JsonVal::Bool(false)),
            b'"' => self.parse_string().map(JsonVal::Str),
            b'[' => self.parse_array(depth),
            b'{' => self.parse_object(depth),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => Err(format!("unexpected byte '{}'", b as char)),
        }
    }

    fn expect_lit(&mut self, lit: &[u8], val: JsonVal) -> Result<JsonVal, String> {
        if self.bytes[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            Ok(val)
        } else {
            Err("invalid literal".into())
        }
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.pos += 1; // '"'
        let mut out = String::new();
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'"' {
                self.pos += 1;
                return Ok(out);
            }
            if b == b'\\' {
                self.pos += 1;
                let Some(esc) = self.peek() else {
                    return Err("unterminated escape".into());
                };
                self.pos += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\x08'),
                    b'f' => out.push('\x0c'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let code = self.parse_hex4()?;
                        if (0xD800..=0xDBFF).contains(&code)
                            && self.bytes[self.pos..].starts_with(b"\\u")
                        {
                            self.pos += 2;
                            let low = self.parse_hex4()?;
                            if (0xDC00..=0xDFFF).contains(&low) {
                                let cp = 0x10000
                                    + (((u32::from(code) - 0xD800) << 10)
                                        | (u32::from(low) - 0xDC00));
                                if let Some(ch) = char::from_u32(cp) {
                                    out.push(ch);
                                }
                            }
                        } else if let Some(ch) = char::from_u32(u32::from(code)) {
                            out.push(ch);
                        }
                    }
                    _ => return Err("bad escape".into()),
                }
            } else {
                let rest = std::str::from_utf8(&self.bytes[self.pos..])
                    .map_err(|_| "invalid utf8".to_owned())?;
                let ch = rest.chars().next().ok_or("eof")?;
                out.push(ch);
                self.pos += ch.len_utf8();
            }
        }
        Err("unterminated string".into())
    }

    fn parse_hex4(&mut self) -> Result<u16, String> {
        if self.pos + 4 > self.bytes.len() {
            return Err("short unicode escape".into());
        }
        let hex = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
            .map_err(|_| "invalid unicode escape".to_owned())?;
        self.pos += 4;
        u16::from_str_radix(hex, 16).map_err(|_| "invalid hex".into())
    }

    fn parse_number(&mut self) -> Result<JsonVal, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while let Some(b) = self.peek() {
            if matches!(b, b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                self.pos += 1;
            } else {
                break;
            }
        }
        let slice = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| "invalid number".to_owned())?;
        let num = slice
            .parse::<f64>()
            .map_err(|_| format!("invalid float {slice:?}"))?;
        Ok(JsonVal::Num(num))
    }

    fn parse_array(&mut self, depth: usize) -> Result<JsonVal, String> {
        self.pos += 1;
        let mut arr = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsonVal::Arr(arr));
        }
        loop {
            arr.push(self.parse_val(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(JsonVal::Arr(arr));
                }
                _ => return Err("expected ',' or ']'".into()),
            }
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<JsonVal, String> {
        self.pos += 1;
        let mut map = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonVal::Obj(map));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err("expected object key".into());
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err("expected ':'".into());
            }
            self.pos += 1;
            let val = self.parse_val(depth + 1)?;
            map.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(JsonVal::Obj(map));
                }
                _ => return Err("expected ',' or '}'".into()),
            }
        }
    }
}

pub fn parse_json(input: &str) -> Result<JsonVal, String> {
    let mut parser = JsonParser::new(input);
    let val = parser.parse_val(0)?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err("trailing characters in JSON".into());
    }
    Ok(val)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QUnitSummary {
    pub total: u64,
    pub passed: u64,
    pub failed: u64,
    pub runtime_ms: u64,
}

#[derive(Clone, Debug)]
enum PendingCallbackKind {
    RpcPromiseEcho {
        tab_id: u64,
        ctx_id: String,
        rpc_id: i64,
    },
    ExceptionTestReport {
        tab_id: u64,
        ctx_id: String,
        result_cb_id: String,
    },
}

struct PendingUrlDownload {
    tab_id: u64,
    ctx_id: String,
    rpc_id: i64,
    ticket: UrlRequestTicket,
}

#[derive(Default)]
pub(super) struct CefRsGpuiRpcHost {
    next_reply_id: u64,
    pending_callbacks: BTreeMap<u64, PendingCallbackKind>,
    pending_downloads: Vec<PendingUrlDownload>,
    pub qunit_results: BTreeMap<u64, QUnitSummary>,
    pub qunit_failures: Vec<(u64, String)>,
}

impl CefRsGpuiRpcHost {
    fn next_reply_id(&mut self) -> u64 {
        self.next_reply_id += 1;
        self.next_reply_id
    }

    pub(super) fn handle_incoming_messages(
        &mut self,
        incoming: &mut VecDeque<(u64, String)>,
        rt: Option<&Runtime>,
        cx: &mut AsyncApp,
        root: WindowHandle<super::ui::CefExampleRoot>,
        out_messages: &mut Vec<(u64, String)>,
    ) {
        while let Some((tab_id, raw_json)) = incoming.pop_front() {
            let Ok(root_val) = parse_json(&raw_json) else {
                continue;
            };
            let Some(kind) = root_val.obj_get("__cefKind").and_then(JsonVal::as_str) else {
                continue;
            };
            let ctx_id = root_val
                .obj_get("ctxId")
                .and_then(JsonVal::as_str)
                .unwrap_or_default()
                .to_owned();
            match kind {
                "postMessage" => {
                    self.handle_post_message(tab_id, &ctx_id, &root_val, out_messages);
                }
                "cbResult" => {
                    self.handle_callback_result(&root_val, out_messages);
                }
                "rpcCall" => {
                    self.handle_rpc_call(tab_id, &ctx_id, &root_val, rt, cx, root, out_messages);
                }
                _ => {}
            }
        }
    }

    pub(super) fn poll_downloads(&mut self, out_messages: &mut Vec<(u64, String)>) {
        let mut idx = 0;
        while idx < self.pending_downloads.len() {
            match self.pending_downloads[idx].ticket.poll() {
                UrlRequestState::Pending(_) => {
                    idx += 1;
                }
                UrlRequestState::Completed(resp) => {
                    let item = self.pending_downloads.swap_remove(idx);
                    let ok = resp.status == UrlRequestStatus::Success
                        && resp.http_status.is_none_or(|c| c < 400);
                    let reply = if ok {
                        let text = String::from_utf8_lossy(&resp.body);
                        let lines: Vec<JsonVal> = text
                            .split('\n')
                            .filter(|line| !line.trim().is_empty())
                            .map(|line| JsonVal::Str(line.trim_end_matches('\r').to_owned()))
                            .collect();
                        rpc_ok_json(&item.ctx_id, item.rpc_id, JsonVal::Arr(lines))
                    } else {
                        rpc_err_json(
                            &item.ctx_id,
                            item.rpc_id,
                            &format!(
                                "Download failed (status={:?}, http={:?}, err={})",
                                resp.status, resp.http_status, resp.error_code
                            ),
                        )
                    };
                    out_messages.push((item.tab_id, reply));
                }
                UrlRequestState::Failed(err) => {
                    let item = self.pending_downloads.swap_remove(idx);
                    out_messages.push((
                        item.tab_id,
                        rpc_err_json(
                            &item.ctx_id,
                            item.rpc_id,
                            &format!("UrlRequest failed: {err:?}"),
                        ),
                    ));
                }
            }
        }
    }

    fn handle_post_message(
        &mut self,
        tab_id: u64,
        ctx_id: &str,
        envelope: &JsonVal,
        out_messages: &mut Vec<(u64, String)>,
    ) {
        let Some(payload) = envelope.obj_get("payload") else {
            return;
        };
        if let Some(n) = payload.as_i64() {
            let reply = JsonVal::Obj(vec![
                ("__cefKind".into(), JsonVal::Str("postMessageInt".into())),
                ("ctxId".into(), JsonVal::Str(ctx_id.to_owned())),
                ("value".into(), JsonVal::Num(n as f64)),
            ])
            .to_json_string();
            out_messages.push((tab_id, reply));
            return;
        }
        let Some(msg_type) = payload.obj_get("Type").and_then(JsonVal::as_str) else {
            return;
        };
        match msg_type {
            "Update" => {
                if let Some(cb_id) = payload
                    .obj_get("Callback")
                    .and_then(|c| c.obj_get("__cefCbId"))
                    .and_then(JsonVal::as_str)
                {
                    let reply = invoke_callback_json(
                        ctx_id,
                        cb_id,
                        None,
                        vec![JsonVal::Str("Update".into())],
                        true,
                    );
                    out_messages.push((tab_id, reply));
                }
            }
            "QUnitExecutionComplete" => {
                if let Some(details) = payload.obj_get("Details") {
                    let summary = QUnitSummary {
                        total: details
                            .obj_get("total")
                            .and_then(JsonVal::as_i64)
                            .unwrap_or(0)
                            .max(0) as u64,
                        passed: details
                            .obj_get("passed")
                            .and_then(JsonVal::as_i64)
                            .unwrap_or(0)
                            .max(0) as u64,
                        failed: details
                            .obj_get("failed")
                            .and_then(JsonVal::as_i64)
                            .unwrap_or(0)
                            .max(0) as u64,
                        runtime_ms: details
                            .obj_get("runtime")
                            .and_then(JsonVal::as_i64)
                            .unwrap_or(0)
                            .max(0) as u64,
                    };
                    self.qunit_results.insert(tab_id, summary);
                }
            }
            "QUnitTestFailed" => {
                if let Some(out) = payload.obj_get("Output").and_then(JsonVal::as_str) {
                    self.qunit_failures.push((tab_id, out.to_owned()));
                }
            }
            _ => {}
        }
    }

    fn handle_callback_result(&mut self, msg: &JsonVal, out_messages: &mut Vec<(u64, String)>) {
        let Some(reply_id) = msg.obj_get("replyId").and_then(JsonVal::as_i64) else {
            return;
        };
        let Ok(reply_key) = u64::try_from(reply_id) else {
            return;
        };
        let Some(pending) = self.pending_callbacks.remove(&reply_key) else {
            return;
        };
        let ok = msg
            .obj_get("ok")
            .and_then(JsonVal::as_bool)
            .unwrap_or(false);
        let result = msg.obj_get("result").cloned().unwrap_or(JsonVal::Null);
        let error = msg
            .obj_get("error")
            .and_then(JsonVal::as_str)
            .unwrap_or("Callback failed")
            .to_owned();

        match pending {
            PendingCallbackKind::RpcPromiseEcho {
                tab_id,
                ctx_id,
                rpc_id,
            } => {
                let reply = if ok {
                    rpc_ok_json(&ctx_id, rpc_id, result)
                } else {
                    rpc_err_json(&ctx_id, rpc_id, &error)
                };
                out_messages.push((tab_id, reply));
            }
            PendingCallbackKind::ExceptionTestReport {
                tab_id,
                ctx_id,
                result_cb_id,
            } => {
                let text = if ok {
                    "Fatal: No Exception thrown in error callback".to_owned()
                } else {
                    format!("Exception Thrown: {error}")
                };
                let reply = invoke_callback_json(
                    &ctx_id,
                    &result_cb_id,
                    None,
                    vec![JsonVal::Str(text)],
                    true,
                );
                out_messages.push((tab_id, reply));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_rpc_call(
        &mut self,
        tab_id: u64,
        ctx_id: &str,
        msg: &JsonVal,
        rt: Option<&Runtime>,
        cx: &mut AsyncApp,
        root: WindowHandle<super::ui::CefExampleRoot>,
        out_messages: &mut Vec<(u64, String)>,
    ) {
        let Some(rpc_id) = msg.obj_get("id").and_then(JsonVal::as_i64) else {
            return;
        };
        let method = msg
            .obj_get("method")
            .and_then(JsonVal::as_str)
            .unwrap_or_default();
        let args = msg.obj_get("args").and_then(JsonVal::as_arr).unwrap_or(&[]);

        match method {
            "error" => {
                out_messages.push((
                    tab_id,
                    rpc_err_json(ctx_id, rpc_id, "This is an exception coming from C#"),
                ));
            }
            "div" => {
                let a = arg_i64(args, 0).unwrap_or(0);
                let b = arg_i64(args, 1).unwrap_or(0);
                if b == 0 {
                    out_messages.push((
                        tab_id,
                        rpc_err_json(ctx_id, rpc_id, "Attempted to divide by zero."),
                    ));
                } else {
                    out_messages.push((
                        tab_id,
                        rpc_ok_json(ctx_id, rpc_id, JsonVal::Num((a / b) as f64)),
                    ));
                }
            }
            "divWithBlockingTaskCall" => {
                let a = arg_i64(args, 0).unwrap_or(0);
                let b = arg_i64(args, 1).unwrap_or(0);
                if b == 0 {
                    out_messages.push((
                        tab_id,
                        rpc_err_json(ctx_id, rpc_id, "Attempted to divide by zero."),
                    ));
                } else {
                    out_messages.push((
                        tab_id,
                        rpc_ok_json(ctx_id, rpc_id, JsonVal::Str((a / b).to_string())),
                    ));
                }
            }
            "uIntAddModel" => {
                let model = args.first();
                let a = model
                    .and_then(|m| m.obj_get("ParamA").or_else(|| m.obj_get("paramA")))
                    .and_then(JsonVal::as_i64)
                    .unwrap_or(0);
                let b = model
                    .and_then(|m| m.obj_get("ParamB").or_else(|| m.obj_get("paramB")))
                    .and_then(JsonVal::as_i64)
                    .unwrap_or(0);
                out_messages.push((
                    tab_id,
                    rpc_ok_json(ctx_id, rpc_id, JsonVal::Num((a + b) as f64)),
                ));
            }
            "uIntAdd" => {
                let a = arg_i64(args, 0).unwrap_or(0);
                let b = arg_i64(args, 1).unwrap_or(0);
                out_messages.push((
                    tab_id,
                    rpc_ok_json(ctx_id, rpc_id, JsonVal::Num((a + b) as f64)),
                ));
            }
            "hello" => {
                let name = arg_str(args, 0).unwrap_or_default();
                out_messages.push((
                    tab_id,
                    rpc_ok_json(ctx_id, rpc_id, JsonVal::Str(format!("Hello {name}"))),
                ));
            }
            "doSomething" => {
                let reply = rpc_ok_json(
                    ctx_id,
                    rpc_id,
                    JsonVal::Str("Waited for 1000ms before returning".into()),
                );
                schedule_delayed_message(cx, root, tab_id, 1000, reply);
            }
            "returnObject" | "returnClass" | "returnClassAsJsonString" => {
                let name = arg_str(args, 0).unwrap_or_default().to_owned();
                out_messages.push((
                    tab_id,
                    rpc_ok_json(
                        ctx_id,
                        rpc_id,
                        JsonVal::Obj(vec![("Value".into(), JsonVal::Str(name))]),
                    ),
                ));
            }
            "returnStructArray" | "returnClassesArray" => {
                let name = arg_str(args, 0).unwrap_or_default();
                let arr = JsonVal::Arr(vec![
                    JsonVal::Obj(vec![("Value".into(), JsonVal::Str(format!("{name}Item1")))]),
                    JsonVal::Obj(vec![("Value".into(), JsonVal::Str(format!("{name}Item2")))]),
                ]);
                out_messages.push((tab_id, rpc_ok_json(ctx_id, rpc_id, arr)));
            }
            "echoDateTime" | "echoNullableDateTime" => {
                let echoed = args.first().cloned().unwrap_or(JsonVal::Null);
                out_messages.push((tab_id, rpc_ok_json(ctx_id, rpc_id, echoed)));
            }
            "echoArray" => {
                let echoed = args.first().cloned().unwrap_or(JsonVal::Arr(Vec::new()));
                out_messages.push((tab_id, rpc_ok_json(ctx_id, rpc_id, echoed)));
            }
            "echoValueTypeArray" => {
                let items = args
                    .first()
                    .and_then(JsonVal::as_arr)
                    .map(|slice| {
                        slice
                            .iter()
                            .map(|v| JsonVal::Num(v.as_i64().unwrap_or(0) as f64))
                            .collect()
                    })
                    .unwrap_or_default();
                out_messages.push((tab_id, rpc_ok_json(ctx_id, rpc_id, JsonVal::Arr(items))));
            }
            "echoMultidimensionalArray" => {
                let echoed = args.first().cloned().unwrap_or(JsonVal::Arr(Vec::new()));
                out_messages.push((tab_id, rpc_ok_json(ctx_id, rpc_id, echoed)));
            }
            "dynamiObjectList" => {
                let mut builder = String::new();
                if let Some(items) = args.first().and_then(JsonVal::as_arr) {
                    for b in items {
                        let name = b.obj_get("Name").and_then(JsonVal::as_str).unwrap_or("");
                        let eng = b
                            .obj_get("Engine")
                            .and_then(|e| e.obj_get("Name"))
                            .and_then(JsonVal::as_str)
                            .unwrap_or("");
                        builder.push_str(&format!("Browser(Name:{name};Engine:{eng});"));
                    }
                }
                out_messages.push((tab_id, rpc_ok_json(ctx_id, rpc_id, JsonVal::Str(builder))));
            }
            "methodReturnsList" => {
                let list = JsonVal::Arr(vec![
                    JsonVal::Str("Element 0 - First".into()),
                    JsonVal::Str("Element 1".into()),
                    JsonVal::Str("Element 2 - Last".into()),
                ]);
                out_messages.push((tab_id, rpc_ok_json(ctx_id, rpc_id, list)));
            }
            "methodReturnsListOfLists" => {
                let list = JsonVal::Arr(vec![
                    JsonVal::Arr(vec![
                        JsonVal::Str("Element 0, 0".into()),
                        JsonVal::Str("Element 0, 1".into()),
                    ]),
                    JsonVal::Arr(vec![
                        JsonVal::Str("Element 1, 0".into()),
                        JsonVal::Str("Element 1, 1".into()),
                    ]),
                    JsonVal::Arr(vec![
                        JsonVal::Str("Element 2, 0".into()),
                        JsonVal::Str("Element 2, 1".into()),
                    ]),
                ]);
                out_messages.push((tab_id, rpc_ok_json(ctx_id, rpc_id, list)));
            }
            "methodReturnsDictionary1" => {
                let dict1 = JsonVal::Obj(vec![
                    ("five".into(), JsonVal::Num(5.0)),
                    ("ten".into(), JsonVal::Num(10.0)),
                ]);
                out_messages.push((tab_id, rpc_ok_json(ctx_id, rpc_id, dict1)));
            }
            "methodReturnsDictionary2" => {
                out_messages.push((tab_id, rpc_ok_json(ctx_id, rpc_id, dict2_value())));
            }
            "methodReturnsDictionary3" => {
                let dict3 = JsonVal::Obj(vec![("data".into(), dict2_value())]);
                out_messages.push((tab_id, rpc_ok_json(ctx_id, rpc_id, dict3)));
            }
            "passSimpleClassAsArgument" => {
                let res = match args.first() {
                    Some(obj @ JsonVal::Obj(_)) => {
                        let ts = obj
                            .obj_get("TestString")
                            .and_then(JsonVal::as_str)
                            .unwrap_or("");
                        let p1 = obj
                            .obj_get("SubClasses")
                            .and_then(JsonVal::as_arr)
                            .and_then(|a| a.first())
                            .and_then(|s| s.obj_get("PropertyOne"))
                            .and_then(JsonVal::as_str)
                            .unwrap_or("");
                        JsonVal::Obj(vec![
                            ("Item1".into(), JsonVal::Bool(true)),
                            (
                                "Item2".into(),
                                JsonVal::Str(format!(
                                    "TestString:{ts};SubClasses[0].PropertyOne:{p1}"
                                )),
                            ),
                        ])
                    }
                    _ => JsonVal::Obj(vec![
                        ("Item1".into(), JsonVal::Bool(false)),
                        (
                            "Item2".into(),
                            JsonVal::Str("PassSimpleClassAsArgument dictionary param is null".into()),
                        ),
                    ]),
                };
                out_messages.push((tab_id, rpc_ok_json(ctx_id, rpc_id, res)));
            }
            "returnTaskStringAsync" => {
                out_messages.push((
                    tab_id,
                    rpc_ok_json(ctx_id, rpc_id, JsonVal::Str("ReturnTaskStringAsync".into())),
                ));
            }
            "waitBeforeReturnAsync" => {
                let ms = arg_i64(args, 0).unwrap_or(0).clamp(0, 10_000) as u64;
                schedule_delayed_message(
                    cx,
                    root,
                    tab_id,
                    ms,
                    rpc_ok_json(ctx_id, rpc_id, JsonVal::Null),
                );
            }
            "voidReturnAsync" => {
                schedule_delayed_message(
                    cx,
                    root,
                    tab_id,
                    1000,
                    rpc_ok_json(ctx_id, rpc_id, JsonVal::Null),
                );
            }
            "asyncWaitTwoSeconds" => {
                let echoed = args.first().cloned().unwrap_or(JsonVal::Null);
                schedule_delayed_message(cx, root, tab_id, 2000, rpc_ok_json(ctx_id, rpc_id, echoed));
            }
            "asyncThrowException" => {
                let reply = rpc_err_json(ctx_id, rpc_id, "Expected Exception");
                schedule_delayed_message(cx, root, tab_id, 2000, reply);
            }
            "asyncDownloadFileAndSplitOnNewLines" => {
                let url = arg_str(args, 0).unwrap_or_default().to_owned();
                let Some(runtime) = rt else {
                    out_messages.push((
                        tab_id,
                        rpc_err_json(ctx_id, rpc_id, "Runtime unavailable"),
                    ));
                    return;
                };
                let mut spec = UrlRequestSpec::get(UrlRequestTarget::DefaultProfile, url);
                spec.timeout_ms = 8_000;
                match runtime.start_url_request(spec) {
                    Ok(ticket) => {
                        self.pending_downloads.push(PendingUrlDownload {
                            tab_id,
                            ctx_id: ctx_id.to_owned(),
                            rpc_id,
                            ticket,
                        });
                    }
                    Err(err) => {
                        out_messages.push((
                            tab_id,
                            rpc_err_json(ctx_id, rpc_id, &format!("start_url_request: {err:?}")),
                        ));
                    }
                }
            }
            "javascriptCallbackEvalPromise" => {
                let msg_text = arg_str(args, 0).unwrap_or_default().to_owned();
                let Some(cb_id) = args
                    .get(1)
                    .and_then(|v| v.obj_get("__cefCbId"))
                    .and_then(JsonVal::as_str)
                else {
                    out_messages.push((tab_id, rpc_err_json(ctx_id, rpc_id, "Missing callback")));
                    return;
                };
                let reply_id = self.next_reply_id();
                self.pending_callbacks.insert(
                    reply_id,
                    PendingCallbackKind::RpcPromiseEcho {
                        tab_id,
                        ctx_id: ctx_id.to_owned(),
                        rpc_id,
                    },
                );
                out_messages.push((
                    tab_id,
                    invoke_callback_json(
                        ctx_id,
                        cb_id,
                        Some(reply_id),
                        vec![JsonVal::Str(msg_text)],
                        true,
                    ),
                ));
            }
            "javascriptOptionalCallbackEvalPromise" => {
                let invoke = args.first().and_then(JsonVal::as_bool).unwrap_or(false);
                let msg_text = arg_str(args, 1).unwrap_or_default().to_owned();
                if !invoke {
                    out_messages.push((tab_id, rpc_ok_json(ctx_id, rpc_id, JsonVal::Str(msg_text))));
                    return;
                }
                let Some(cb_id) = args
                    .get(2)
                    .and_then(|v| v.obj_get("__cefCbId"))
                    .and_then(JsonVal::as_str)
                else {
                    out_messages.push((tab_id, rpc_err_json(ctx_id, rpc_id, "Missing callback")));
                    return;
                };
                let reply_id = self.next_reply_id();
                self.pending_callbacks.insert(
                    reply_id,
                    PendingCallbackKind::RpcPromiseEcho {
                        tab_id,
                        ctx_id: ctx_id.to_owned(),
                        rpc_id,
                    },
                );
                out_messages.push((
                    tab_id,
                    invoke_callback_json(
                        ctx_id,
                        cb_id,
                        Some(reply_id),
                        vec![JsonVal::Str(msg_text)],
                        true,
                    ),
                ));
            }
            "testCallback" => {
                if let Some(cb_id) = args
                    .first()
                    .and_then(|v| v.obj_get("__cefCbId"))
                    .and_then(JsonVal::as_str)
                {
                    let reply = invoke_callback_json(
                        ctx_id,
                        cb_id,
                        None,
                        vec![JsonVal::Obj(vec![(
                            "Response".into(),
                            JsonVal::Str("This callback from C# was delayed 1500ms".into()),
                        )])],
                        true,
                    );
                    schedule_delayed_message(cx, root, tab_id, 1500, reply);
                }
            }
            "testCallbackFromObject" => {
                if let Some(obj) = args.first()
                    && let Some(cb_id) = obj
                        .obj_get("Callback")
                        .and_then(|v| v.obj_get("__cefCbId"))
                        .and_then(JsonVal::as_str)
                {
                    let ts = obj
                        .obj_get("TestString")
                        .and_then(JsonVal::as_str)
                        .unwrap_or("");
                    let p1 = obj
                        .obj_get("SubClasses")
                        .and_then(JsonVal::as_arr)
                        .and_then(|a| a.first())
                        .and_then(|s| s.obj_get("PropertyOne"))
                        .and_then(JsonVal::as_str)
                        .unwrap_or("");
                    let msg_out = format!("message from C# {ts} - {p1}");
                    let reply = invoke_callback_json(
                        ctx_id,
                        cb_id,
                        None,
                        vec![JsonVal::Str(msg_out)],
                        true,
                    );
                    schedule_delayed_message(cx, root, tab_id, 1500, reply);
                }
            }
            "testCallbackWithDateTime"
            | "testCallbackWithDateTime1900"
            | "testCallbackWithDateTime1970"
            | "testCallbackWithDateTime1985" => {
                let year = match method {
                    "testCallbackWithDateTime1900" => 1900.0,
                    "testCallbackWithDateTime1970" => 1970.0,
                    "testCallbackWithDateTime1985" => 1985.0,
                    _ => 2019.0,
                };
                if let Some(cb_id) = args
                    .first()
                    .and_then(|v| v.obj_get("__cefCbId"))
                    .and_then(JsonVal::as_str)
                {
                    let arr = JsonVal::Arr(vec![
                        JsonVal::Num(year),
                        JsonVal::Num(1.0),
                        JsonVal::Num(1.0),
                        JsonVal::Num(12.0),
                        JsonVal::Num(0.0),
                        JsonVal::Num(0.0),
                    ]);
                    let dt = JsonVal::Obj(vec![("__cefLocalDate".into(), arr.clone())]);
                    out_messages.push((
                        tab_id,
                        invoke_callback_json(ctx_id, cb_id, None, vec![dt, arr], true),
                    ));
                }
            }
            "testCallbackException" => {
                if let (Some(err_cb), Some(res_cb)) = (
                    args.first()
                        .and_then(|v| v.obj_get("__cefCbId"))
                        .and_then(JsonVal::as_str),
                    args.get(1)
                        .and_then(|v| v.obj_get("__cefCbId"))
                        .and_then(JsonVal::as_str),
                ) {
                    let reply_id = self.next_reply_id();
                    self.pending_callbacks.insert(
                        reply_id,
                        PendingCallbackKind::ExceptionTestReport {
                            tab_id,
                            ctx_id: ctx_id.to_owned(),
                            result_cb_id: res_cb.to_owned(),
                        },
                    );
                    let reply = invoke_callback_json(
                        ctx_id,
                        err_cb,
                        Some(reply_id),
                        vec![JsonVal::Str("This callback from C# was delayed 500ms".into())],
                        true,
                    );
                    schedule_delayed_message(cx, root, tab_id, 500, reply);
                }
            }
            "setCallBack" => {
                out_messages.push((tab_id, rpc_ok_json(ctx_id, rpc_id, JsonVal::Null)));
            }
            other => {
                out_messages.push((
                    tab_id,
                    rpc_err_json(ctx_id, rpc_id, &format!("Unknown RPC method: {other}")),
                ));
            }
        }
    }
}

fn dict2_value() -> JsonVal {
    JsonVal::Obj(vec![
        ("onepointfive".into(), JsonVal::Num(1.5)),
        ("five".into(), JsonVal::Num(5.0)),
        ("ten".into(), JsonVal::Str("ten".into())),
        (
            "twotwo".into(),
            JsonVal::Arr(vec![JsonVal::Num(2.0), JsonVal::Num(2.0)]),
        ),
    ])
}

fn arg_str(args: &[JsonVal], idx: usize) -> Option<&str> {
    args.get(idx).and_then(JsonVal::as_str)
}

fn arg_i64(args: &[JsonVal], idx: usize) -> Option<i64> {
    args.get(idx).and_then(JsonVal::as_i64)
}

fn rpc_ok_json(ctx_id: &str, id: i64, result: JsonVal) -> String {
    JsonVal::Obj(vec![
        ("__cefKind".into(), JsonVal::Str("rpcResult".into())),
        ("ctxId".into(), JsonVal::Str(ctx_id.to_owned())),
        ("id".into(), JsonVal::Num(id as f64)),
        ("ok".into(), JsonVal::Bool(true)),
        ("result".into(), result),
    ])
    .to_json_string()
}

fn rpc_err_json(ctx_id: &str, id: i64, error: &str) -> String {
    JsonVal::Obj(vec![
        ("__cefKind".into(), JsonVal::Str("rpcResult".into())),
        ("ctxId".into(), JsonVal::Str(ctx_id.to_owned())),
        ("id".into(), JsonVal::Num(id as f64)),
        ("ok".into(), JsonVal::Bool(false)),
        ("error".into(), JsonVal::Str(error.to_owned())),
    ])
    .to_json_string()
}

fn invoke_callback_json(
    ctx_id: &str,
    cb_id: &str,
    reply_id: Option<u64>,
    args: Vec<JsonVal>,
    dispose: bool,
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

fn schedule_delayed_message(
    cx: &mut AsyncApp,
    root: WindowHandle<super::ui::CefExampleRoot>,
    tab_id: u64,
    delay_ms: u64,
    payload_json: String,
) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        cx.background_executor()
            .timer(std::time::Duration::from_millis(delay_ms))
            .await;
        let _ = root.update(cx, |r, _, _| {
            r.emit(ExampleAction::PostJsToTab {
                tab_id,
                payload_json,
            });
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trip_handles_escapes_arrays_and_numbers() {
        let raw = r#"{"a":123,"b":-4.5,"c":true,"d":null,"e":["hi\n\"x\"",{"nested":[]}],"u":"\u4e2d\ud83d\ude00"}"#;
        let val = parse_json(raw).expect("parse");
        assert_eq!(val.obj_get("a").and_then(JsonVal::as_i64), Some(123));
        assert_eq!(val.obj_get("b").and_then(JsonVal::as_f64), Some(-4.5));
        assert_eq!(val.obj_get("c").and_then(JsonVal::as_bool), Some(true));
        assert_eq!(val.obj_get("u").and_then(JsonVal::as_str), Some("中😀"));
        let reencoded = val.to_json_string();
        let reparsed = parse_json(&reencoded).expect("reparse");
        assert_eq!(val, reparsed);
    }
}
