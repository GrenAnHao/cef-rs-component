//! Lightweight, GPUI-free CEF subprocess App for consistent custom scheme
//! registration and opt-in, origin-gated cross-process JS bridging.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::sync::{Arc, Mutex};

use cef::*;
use rust_cef_core::contracts::truncate_utf8_bytes;
use rust_cef_core::custom_schemes::{
    CUSTOM_SCHEMES_SWITCH, CustomSchemeConfig, decode_custom_schemes_switch,
};
use rust_cef_core::js_bridge::{
    MAX_EVAL_ERROR_MESSAGE_BYTES, MAX_JS_JSON_DEPTH, MAX_JS_PAYLOAD_BYTES, validate_bounded_json,
};

use crate::client::read_cef_string_list;
use crate::policy::canonical_origin;

pub(crate) const EXTRA_RUNTIME_GEN: &str = "rust_cef.runtime";
pub(crate) const EXTRA_BROWSER_LOCAL: &str = "rust_cef.browser";
pub(crate) const EXTRA_JS_BRIDGE_ENABLED: &str = "rust_cef.js_bridge.enabled";
pub(crate) const EXTRA_JS_BRIDGE_MAX_BYTES: &str = "rust_cef.js_bridge.max_bytes";
pub(crate) const EXTRA_JS_BRIDGE_ORIGINS: &str = "rust_cef.js_bridge.origins";

pub(crate) const MSG_POST_MESSAGE: &str = "rust_cef.post_message";
pub(crate) const MSG_HOST_MESSAGE: &str = "rust_cef.host_message";
pub(crate) const MSG_EVAL_REQUEST: &str = "rust_cef.eval_request";
pub(crate) const MSG_EVAL_RESPONSE: &str = "rust_cef.eval_response";

const MAX_V8_LISTENERS_PER_BROWSER: usize = 16;

struct ContextEntryGuard {
    ctx: V8Context,
    entered: bool,
}

impl ContextEntryGuard {
    fn enter(ctx: V8Context) -> Self {
        let entered = ctx.enter() != 0;
        Self { ctx, entered }
    }
}

impl Drop for ContextEntryGuard {
    fn drop(&mut self) {
        if self.entered {
            let _ = self.ctx.exit();
        }
    }
}

pub(crate) fn parse_custom_schemes_from_args(args: &[OsString]) -> Vec<CustomSchemeConfig> {
    let prefix = format!("--{CUSTOM_SCHEMES_SWITCH}=");
    let flag = format!("--{CUSTOM_SCHEMES_SWITCH}");
    let mut selected: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let Some(text) = arg.to_str() else {
            continue;
        };
        if let Some(value) = text.strip_prefix(&prefix) {
            selected = Some(value.to_owned());
        } else if text == flag {
            selected = iter.next().and_then(|v| v.to_str()).map(ToOwned::to_owned);
        }
    }
    selected
        .as_deref()
        .and_then(|raw| decode_custom_schemes_switch(raw).ok())
        .unwrap_or_default()
}

struct HelperBrowserBridge {
    runtime_gen: u64,
    browser_local: u64,
    js_bridge_enabled: bool,
    max_bytes: usize,
    allowed_origins: BTreeSet<String>,
    listeners: Vec<(V8Context, V8Value)>,
}

pub(crate) struct HelperRenderState {
    custom_schemes: BTreeSet<String>,
    browsers: Mutex<BTreeMap<i32, HelperBrowserBridge>>,
}

impl HelperRenderState {
    fn new(schemes: &[CustomSchemeConfig]) -> Arc<Self> {
        Arc::new(Self {
            custom_schemes: schemes
                .iter()
                .map(|s| s.scheme.to_ascii_lowercase())
                .collect(),
            browsers: Mutex::new(BTreeMap::new()),
        })
    }

    fn lock_browsers(&self) -> std::sync::MutexGuard<'_, BTreeMap<i32, HelperBrowserBridge>> {
        self.browsers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn record_browser(&self, native_id: i32, extra_info: Option<&mut DictionaryValue>) {
        let Some(info) = extra_info else {
            self.lock_browsers().remove(&native_id);
            return;
        };
        let rt_str =
            CefString::from(&info.string(Some(&CefString::from(EXTRA_RUNTIME_GEN)))).to_string();
        let br_str =
            CefString::from(&info.string(Some(&CefString::from(EXTRA_BROWSER_LOCAL)))).to_string();
        let (Ok(runtime_gen), Ok(browser_local)) = (rt_str.parse::<u64>(), br_str.parse::<u64>())
        else {
            return;
        };
        let js_bridge_enabled = info.bool(Some(&CefString::from(EXTRA_JS_BRIDGE_ENABLED))) != 0;
        let max_raw = info.int(Some(&CefString::from(EXTRA_JS_BRIDGE_MAX_BYTES)));
        let max_bytes = usize::try_from(max_raw)
            .unwrap_or(MAX_JS_PAYLOAD_BYTES)
            .clamp(1, MAX_JS_PAYLOAD_BYTES);
        let mut allowed_origins = BTreeSet::new();
        if let Some(list) = info.list(Some(&CefString::from(EXTRA_JS_BRIDGE_ORIGINS))) {
            let count = list.size().min(64);
            for i in 0..count {
                let s = CefString::from(&list.string(i)).to_string();
                if let Some(canonical) = canonical_origin(&s, &self.custom_schemes) {
                    allowed_origins.insert(canonical);
                }
            }
        }
        self.lock_browsers().insert(
            native_id,
            HelperBrowserBridge {
                runtime_gen,
                browser_local,
                js_bridge_enabled,
                max_bytes,
                allowed_origins,
                listeners: Vec::new(),
            },
        );
    }

    fn remove_browser(&self, native_id: i32) {
        let removed = self.lock_browsers().remove(&native_id);
        drop(removed);
    }

    fn release_context_listeners(&self, native_id: i32, context: &mut V8Context) {
        let mut dropped = Vec::new();
        if let Some(entry) = self.lock_browsers().get_mut(&native_id) {
            let mut keep = Vec::with_capacity(entry.listeners.len());
            for (stored_ctx, cb) in entry.listeners.drain(..) {
                if stored_ctx.is_same(Some(context)) != 0 {
                    dropped.push((stored_ctx, cb));
                } else {
                    keep.push((stored_ctx, cb));
                }
            }
            entry.listeners = keep;
        }
        drop(dropped);
    }

    fn origin_for_url(&self, url: &str) -> Option<String> {
        canonical_origin(url, &self.custom_schemes)
    }

    fn is_frame_bridge_permitted(
        &self,
        native_id: i32,
        frame: &Frame,
    ) -> Option<(u64, u64, usize, String)> {
        if frame.is_valid() == 0 || frame.is_main() == 0 {
            return None;
        }
        let url = CefString::from(&frame.url()).to_string();
        let origin = self.origin_for_url(&url)?;
        let guard = self.lock_browsers();
        let bridge = guard.get(&native_id)?;
        if !bridge.js_bridge_enabled || !bridge.allowed_origins.contains(&origin) {
            return None;
        }
        Some((
            bridge.runtime_gen,
            bridge.browser_local,
            bridge.max_bytes,
            origin,
        ))
    }

    fn identity_for_browser(&self, native_id: i32) -> Option<(u64, u64, usize)> {
        let guard = self.lock_browsers();
        let b = guard.get(&native_id)?;
        Some((b.runtime_gen, b.browser_local, b.max_bytes))
    }

    fn push_listener(
        &self,
        native_id: i32,
        ctx: V8Context,
        func: V8Value,
    ) -> Result<(), &'static str> {
        let mut guard = self.lock_browsers();
        let Some(entry) = guard.get_mut(&native_id) else {
            return Err("unknown browser bridge");
        };
        if entry.listeners.len() >= MAX_V8_LISTENERS_PER_BROWSER {
            return Err("rustCef listener limit reached");
        }
        entry.listeners.push((ctx, func));
        Ok(())
    }

    fn listeners_for(&self, native_id: i32) -> Vec<(V8Context, V8Value)> {
        self.lock_browsers()
            .get(&native_id)
            .map(|entry| entry.listeners.clone())
            .unwrap_or_default()
    }
}

pub(crate) fn build_helper_app(args: &[OsString]) -> App {
    let schemes = parse_custom_schemes_from_args(args);
    let state = HelperRenderState::new(&schemes);
    let render = HelperRenderProcessHandler::new(state);
    HelperApp::new(schemes, render)
}

wrap_app! {
    struct HelperApp {
        schemes: Vec<CustomSchemeConfig>,
        render: RenderProcessHandler,
    }

    impl App {
        fn on_register_custom_schemes(&self, registrar: Option<&mut SchemeRegistrar>) {
            let Some(registrar) = registrar else {
                return;
            };
            for cfg in &self.schemes {
                let name = CefString::from(cfg.scheme.as_str());
                let _ = registrar.add_custom_scheme(Some(&name), cfg.options.to_cef_flags());
            }
        }

        fn render_process_handler(&self) -> Option<RenderProcessHandler> {
            Some(self.render.clone())
        }
    }
}

wrap_v8_handler! {
    struct HelperBridgeV8Handler {
        native_id: i32,
        state: Arc<HelperRenderState>,
    }

    impl V8Handler {
        fn execute(
            &self,
            name: Option<&CefString>,
            _object: Option<&mut V8Value>,
            arguments: Option<&[Option<V8Value>]>,
            retval: Option<&mut Option<V8Value>>,
            exception: Option<&mut CefString>,
        ) -> ::std::os::raw::c_int {
            let Some(method) = name.map(ToString::to_string) else {
                return 0;
            };
            let Some(ctx) = v8_context_get_current_context() else {
                if let Some(ex) = exception {
                    ex.try_set("missing V8 context");
                }
                return 1;
            };
            let Some(frame) = ctx.frame() else {
                if let Some(ex) = exception {
                    ex.try_set("missing frame");
                }
                return 1;
            };
            let Some((runtime_gen, browser_local, max_bytes, origin)) =
                self.state.is_frame_bridge_permitted(self.native_id, &frame)
            else {
                if let Some(ex) = exception {
                    ex.try_set("rustCef is not permitted in this origin");
                }
                return 1;
            };

            let args = arguments.unwrap_or(&[]);
            match method.as_str() {
                "postMessage" => {
                    if args.len() != 1 {
                        if let Some(ex) = exception {
                            ex.try_set("rustCef.postMessage requires exactly 1 JSON string argument");
                        }
                        return 1;
                    }
                    let Some(arg) = args[0].as_ref().filter(|v| v.is_string() != 0) else {
                        if let Some(ex) = exception {
                            ex.try_set("rustCef.postMessage argument must be a JSON string");
                        }
                        return 1;
                    };
                    let payload = CefString::from(&arg.string_value()).to_string();
                    if let Err(err) = validate_bounded_json(&payload, max_bytes, MAX_JS_JSON_DEPTH) {
                        if let Some(ex) = exception {
                            let msg = format!("rustCef.postMessage rejected payload: {err:?}");
                            ex.try_set(&msg);
                        }
                        return 1;
                    }
                    let Some(mut msg) = process_message_create(Some(&CefString::from(MSG_POST_MESSAGE)))
                    else {
                        if let Some(ex) = exception {
                            ex.try_set("failed to allocate IPC message");
                        }
                        return 1;
                    };
                    if let Some(list) = msg.argument_list() {
                        list.set_string(0, Some(&CefString::from(runtime_gen.to_string().as_str())));
                        list.set_string(1, Some(&CefString::from(browser_local.to_string().as_str())));
                        list.set_string(2, Some(&CefString::from(origin.as_str())));
                        list.set_string(3, Some(&CefString::from(payload.as_str())));
                        frame.send_process_message(ProcessId::BROWSER, Some(&mut msg));
                    }
                    if let Some(out) = retval {
                        *out = v8_value_create_bool(1);
                    }
                    1
                }
                "onMessage" => {
                    if args.len() != 1 {
                        if let Some(ex) = exception {
                            ex.try_set("rustCef.onMessage requires exactly 1 callback function");
                        }
                        return 1;
                    }
                    let Some(func) = args[0].as_ref().filter(|v| v.is_function() != 0) else {
                        if let Some(ex) = exception {
                            ex.try_set("rustCef.onMessage argument must be a function");
                        }
                        return 1;
                    };
                    if let Err(msg) = self.state.push_listener(self.native_id, ctx, func.clone()) {
                        if let Some(ex) = exception {
                            ex.try_set(msg);
                        }
                        return 1;
                    }
                    if let Some(out) = retval {
                        *out = v8_value_create_bool(1);
                    }
                    1
                }
                _ => 0,
            }
        }
    }
}

wrap_render_process_handler! {
    struct HelperRenderProcessHandler {
        state: Arc<HelperRenderState>,
    }

    impl RenderProcessHandler {
        fn on_browser_created(
            &self,
            browser: Option<&mut Browser>,
            extra_info: Option<&mut DictionaryValue>,
        ) {
            let Some(browser) = browser else {
                return;
            };
            self.state.record_browser(browser.identifier(), extra_info);
        }

        fn on_browser_destroyed(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser else {
                return;
            };
            self.state.remove_browser(browser.identifier());
        }

        fn on_context_created(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            context: Option<&mut V8Context>,
        ) {
            let (Some(browser), Some(frame), Some(context)) = (browser, frame, context) else {
                return;
            };
            let native_id = browser.identifier();
            if self.state.is_frame_bridge_permitted(native_id, frame).is_none() {
                return;
            }
            let _guard = ContextEntryGuard::enter(context.clone());
            let Some(global) = context.global() else {
                return;
            };
            let Some(mut bridge_obj) = v8_value_create_object(None, None) else {
                return;
            };
            let mut handler = HelperBridgeV8Handler::new(native_id, self.state.clone());
            let ro = V8Propertyattribute::from(
                cef::sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_READONLY
                    | cef::sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_DONTDELETE,
            );
            let post_name = CefString::from("postMessage");
            let on_name = CefString::from("onMessage");
            if let Some(mut post_fn) = v8_value_create_function(Some(&post_name), Some(&mut handler))
            {
                bridge_obj.set_value_bykey(Some(&post_name), Some(&mut post_fn), ro);
            }
            if let Some(mut on_fn) = v8_value_create_function(Some(&on_name), Some(&mut handler)) {
                bridge_obj.set_value_bykey(Some(&on_name), Some(&mut on_fn), ro);
            }
            global.set_value_bykey(Some(&CefString::from("rustCef")), Some(&mut bridge_obj), ro);
            let mut eval_ret: Option<V8Value> = None;
            let mut eval_exc: Option<V8Exception> = None;
            let _ = context.eval(
                Some(&CefString::from(
                    "if (typeof globalThis !== 'undefined' && globalThis.rustCef && Object.freeze) { Object.freeze(globalThis.rustCef); }",
                )),
                Some(&CefString::from("rust-cef://bootstrap")),
                1,
                Some(&mut eval_ret),
                Some(&mut eval_exc),
            );
        }

        fn on_context_released(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            context: Option<&mut V8Context>,
        ) {
            if let (Some(browser), Some(context)) = (browser, context) {
                self.state.release_context_listeners(browser.identifier(), context);
            }
        }

        fn on_process_message_received(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> ::std::os::raw::c_int {
            if source_process != ProcessId::BROWSER {
                return 0;
            }
            let (Some(browser), Some(frame), Some(message)) = (browser, frame, message) else {
                return 0;
            };
            let native_id = browser.identifier();
            let msg_name = CefString::from(&message.name()).to_string();
            let Some(args) = message.argument_list() else {
                return 0;
            };
            match msg_name.as_str() {
                MSG_HOST_MESSAGE => {
                    if args.size() < 4 {
                        return 1;
                    }
                    let rt_ok = CefString::from(&args.string(0)).to_string().parse::<u64>().ok();
                    let br_ok = CefString::from(&args.string(1)).to_string().parse::<u64>().ok();
                    let expected_origin = CefString::from(&args.string(2)).to_string();
                    let payload = CefString::from(&args.string(3)).to_string();
                    let Some((runtime_gen, browser_local, max_bytes, origin)) =
                        self.state.is_frame_bridge_permitted(native_id, frame)
                    else {
                        return 1;
                    };
                    if rt_ok != Some(runtime_gen)
                        || br_ok != Some(browser_local)
                        || (!expected_origin.is_empty() && expected_origin != origin)
                        || validate_bounded_json(&payload, max_bytes, MAX_JS_JSON_DEPTH).is_err()
                    {
                        return 1;
                    }
                    let Some(mut ctx) = frame.v8_context() else {
                        return 1;
                    };
                    let _guard = ContextEntryGuard::enter(ctx.clone());
                    let Some(val) = v8_value_create_string(Some(&CefString::from(payload.as_str())))
                    else {
                        return 1;
                    };
                    for (listener_ctx, listener_fn) in self.state.listeners_for(native_id) {
                        if listener_ctx.is_same(Some(&mut ctx)) != 0 {
                            let _ = listener_fn.execute_function_with_context(
                                Some(&mut ctx),
                                None,
                                Some(&[Some(val.clone())]),
                            );
                        }
                    }
                    1
                }
                MSG_EVAL_REQUEST => {
                    if args.size() < 5 {
                        return 1;
                    }
                    let rt_ok = CefString::from(&args.string(0)).to_string().parse::<u64>().ok();
                    let br_ok = CefString::from(&args.string(1)).to_string().parse::<u64>().ok();
                    let seq_str = CefString::from(&args.string(2)).to_string();
                    let expected_origin = CefString::from(&args.string(3)).to_string();
                    let script = CefString::from(&args.string(4)).to_string();

                    let Some((runtime_gen, browser_local, max_bytes)) =
                        self.state.identity_for_browser(native_id)
                    else {
                        return 1;
                    };
                    if rt_ok != Some(runtime_gen) || br_ok != Some(browser_local) {
                        return 1;
                    }
                    let frame_url = CefString::from(&frame.url()).to_string();
                    let origin = self.state.origin_for_url(&frame_url).unwrap_or_default();
                    if !expected_origin.is_empty() && expected_origin != origin {
                        send_eval_error(
                            frame,
                            runtime_gen,
                            browser_local,
                            &seq_str,
                            &origin,
                            "policy_denied",
                            "frame origin changed before eval",
                            0,
                        );
                        return 1;
                    }
                    let Some(ctx) = frame.v8_context() else {
                        send_eval_error(
                            frame,
                            runtime_gen,
                            browser_local,
                            &seq_str,
                            &origin,
                            "invalid_request",
                            "frame V8 context unavailable",
                            0,
                        );
                        return 1;
                    };
                    let _guard = ContextEntryGuard::enter(ctx.clone());
                    let mut retval: Option<V8Value> = None;
                    let mut exception: Option<V8Exception> = None;
                    let ok = ctx.eval(
                        Some(&CefString::from(script.as_str())),
                        Some(&CefString::from("rust-cef://eval")),
                        1,
                        Some(&mut retval),
                        Some(&mut exception),
                    );
                    if ok == 0 {
                        let (msg, line) = if let Some(exc) = exception {
                            let raw_msg = CefString::from(&exc.message()).to_string();
                            (
                                truncate_utf8_bytes(&raw_msg, MAX_EVAL_ERROR_MESSAGE_BYTES),
                                exc.line_number(),
                            )
                        } else {
                            ("JavaScript evaluation threw an exception".into(), 0)
                        };
                        send_eval_error(
                            frame,
                            runtime_gen,
                            browser_local,
                            &seq_str,
                            &origin,
                            "exception",
                            &msg,
                            line,
                        );
                        return 1;
                    }
                    let serialized = match retval {
                        None => Ok("null".to_owned()),
                        Some(ref val) => v8_value_to_bounded_json(val, max_bytes),
                    };
                    match serialized {
                        Ok(json) => send_eval_ok(
                            frame,
                            runtime_gen,
                            browser_local,
                            &seq_str,
                            &origin,
                            &json,
                        ),
                        Err((kind, msg)) => send_eval_error(
                            frame,
                            runtime_gen,
                            browser_local,
                            &seq_str,
                            &origin,
                            kind,
                            msg,
                            0,
                        ),
                    }
                    1
                }
                _ => 0,
            }
        }
    }
}

fn send_eval_ok(
    frame: &Frame,
    runtime_gen: u64,
    browser_local: u64,
    seq: &str,
    origin: &str,
    json: &str,
) {
    let Some(mut msg) = process_message_create(Some(&CefString::from(MSG_EVAL_RESPONSE))) else {
        return;
    };
    if let Some(list) = msg.argument_list() {
        list.set_string(0, Some(&CefString::from(runtime_gen.to_string().as_str())));
        list.set_string(
            1,
            Some(&CefString::from(browser_local.to_string().as_str())),
        );
        list.set_string(2, Some(&CefString::from(seq)));
        list.set_string(3, Some(&CefString::from(origin)));
        list.set_bool(4, 1);
        list.set_string(5, Some(&CefString::from("ok")));
        list.set_string(6, Some(&CefString::from(json)));
        list.set_int(7, 0);
        frame.send_process_message(ProcessId::BROWSER, Some(&mut msg));
    }
}

#[allow(clippy::too_many_arguments)]
fn send_eval_error(
    frame: &Frame,
    runtime_gen: u64,
    browser_local: u64,
    seq: &str,
    origin: &str,
    kind: &str,
    text: &str,
    line: i32,
) {
    let Some(mut msg) = process_message_create(Some(&CefString::from(MSG_EVAL_RESPONSE))) else {
        return;
    };
    let bounded = truncate_utf8_bytes(text, MAX_EVAL_ERROR_MESSAGE_BYTES);
    if let Some(list) = msg.argument_list() {
        list.set_string(0, Some(&CefString::from(runtime_gen.to_string().as_str())));
        list.set_string(
            1,
            Some(&CefString::from(browser_local.to_string().as_str())),
        );
        list.set_string(2, Some(&CefString::from(seq)));
        list.set_string(3, Some(&CefString::from(origin)));
        list.set_bool(4, 0);
        list.set_string(5, Some(&CefString::from(kind)));
        list.set_string(6, Some(&CefString::from(bounded.as_str())));
        list.set_int(7, line);
        frame.send_process_message(ProcessId::BROWSER, Some(&mut msg));
    }
}

fn v8_value_to_bounded_json(
    value: &V8Value,
    max_bytes: usize,
) -> Result<String, (&'static str, &'static str)> {
    let json = v8_to_serde(value, 0)?;
    let encoded = serde_json::to_string(&json)
        .map_err(|_| ("unsupported_type", "JSON serialization failed"))?;
    match validate_bounded_json(&encoded, max_bytes, MAX_JS_JSON_DEPTH) {
        Ok(()) => Ok(encoded),
        Err(rust_cef_core::js_bridge::EvalError::PayloadTooLarge) => Err((
            "payload_too_large",
            "eval result exceeds payload byte limit",
        )),
        Err(rust_cef_core::js_bridge::EvalError::DepthExceeded) => {
            Err(("depth_exceeded", "eval result exceeds JSON depth limit"))
        }
        Err(_) => Err(("invalid_json", "eval result produced invalid JSON")),
    }
}

fn v8_to_serde(
    value: &V8Value,
    depth: usize,
) -> Result<serde_json::Value, (&'static str, &'static str)> {
    if value.is_valid() == 0 || value.is_undefined() != 0 || value.is_null() != 0 {
        return Ok(serde_json::Value::Null);
    }
    if value.is_bool() != 0 {
        return Ok(serde_json::Value::Bool(value.bool_value() != 0));
    }
    if value.is_int() != 0 {
        return Ok(serde_json::Value::Number(value.int_value().into()));
    }
    if value.is_uint() != 0 {
        return Ok(serde_json::Value::Number(value.uint_value().into()));
    }
    if value.is_double() != 0 {
        let num = value.double_value();
        let Some(number) = serde_json::Number::from_f64(num) else {
            return Err(("unsupported_type", "non-finite number cannot be serialized"));
        };
        return Ok(serde_json::Value::Number(number));
    }
    if value.is_string() != 0 {
        let text = CefString::from(&value.string_value()).to_string();
        return Ok(serde_json::Value::String(text));
    }
    if value.is_function() != 0
        || value.is_promise() != 0
        || value.is_array_buffer() != 0
        || value.is_date() != 0
    {
        return Err(("unsupported_type", "non-JSON V8 host/function/binary type"));
    }
    if value.is_array() != 0 {
        if depth >= MAX_JS_JSON_DEPTH {
            return Err(("depth_exceeded", "JSON array nesting depth exceeded"));
        }
        let len = usize::try_from(value.array_length().max(0)).unwrap_or(0);
        if len > 4096 {
            return Err(("payload_too_large", "array element count exceeded limit"));
        }
        let mut arr = Vec::with_capacity(len);
        for index in 0..len {
            let elem = match value.value_byindex(index as i32) {
                Some(v) => v8_to_serde(&v, depth + 1)?,
                None => serde_json::Value::Null,
            };
            arr.push(elem);
        }
        return Ok(serde_json::Value::Array(arr));
    }
    if value.is_object() != 0 {
        if depth >= MAX_JS_JSON_DEPTH {
            return Err(("depth_exceeded", "JSON object nesting depth exceeded"));
        }
        let mut keys = CefStringList::new();
        let _ = value.keys(Some(&mut keys));
        let key_names = read_cef_string_list(&mut keys, 1024, 1024);
        let mut map = serde_json::Map::with_capacity(key_names.len());
        for key in key_names {
            if let Some(prop) = value.value_bykey(Some(&CefString::from(key.as_str()))) {
                if prop.is_undefined() != 0 {
                    continue;
                }
                if prop.is_function() != 0 || prop.is_promise() != 0 || prop.is_array_buffer() != 0
                {
                    return Err(("unsupported_type", "object contains non-JSON property"));
                }
                map.insert(key, v8_to_serde(&prop, depth + 1)?);
            }
        }
        return Ok(serde_json::Value::Object(map));
    }
    Err(("unsupported_type", "unsupported V8 value kind"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_scheme_switch_parser_ignores_bad_inputs_and_parses_valid() {
        let valid = parse_custom_schemes_from_args(&[
            OsString::from("rust-cef-helper.exe"),
            OsString::from("--type=renderer"),
            OsString::from("--rust-cef-custom-schemes=app:89,asset:81"),
        ]);
        assert_eq!(valid.len(), 2);
        assert_eq!(valid[0].scheme, "app");
        assert_eq!(valid[1].scheme, "asset");

        let bad =
            parse_custom_schemes_from_args(&[OsString::from("--rust-cef-custom-schemes=http:1")]);
        assert!(bad.is_empty());

        let separate = parse_custom_schemes_from_args(&[
            OsString::from("--rust-cef-custom-schemes"),
            OsString::from("app:89"),
        ]);
        assert_eq!(separate.len(), 1);
        assert_eq!(separate[0].scheme, "app");
    }

    #[test]
    fn render_state_checks_origin_membership_before_permitting_bridge() {
        let state = HelperRenderState::new(&[CustomSchemeConfig {
            scheme: "app".into(),
            options: rust_cef_core::custom_schemes::CustomSchemeOptions::standard_app(),
            domain: None,
            asset_source: None,
        }]);
        assert_eq!(
            state.origin_for_url("app://LOCAL/page.html"),
            Some("app://local".into())
        );
        assert_eq!(state.origin_for_url("file:///c:/secret.txt"), None);
    }
}
