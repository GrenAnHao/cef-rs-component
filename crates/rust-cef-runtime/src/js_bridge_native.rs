//! Browser-process helpers for bounded JS execution, `EvaluateScript`, and
//! origin-verified bidirectional IPC messages with `HelperApp`.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::collections::BTreeSet;

use cef::*;
use rust_cef_core::contracts::{
    BrowserId, BrowserOptions, FocusedDomNodeInfo, JsUncaughtExceptionInfo,
    MAX_DOM_IDENTIFIER_BYTES, MAX_EXCEPTION_SOURCE_BYTES, MAX_FRAME_ID_BYTES, truncate_utf8_bytes,
};
use rust_cef_core::js_bridge::{
    EvalError, EvalOutcome, EvalRequestId, JsFrameScope, MAX_EVAL_ERROR_MESSAGE_BYTES,
    MAX_JS_JSON_DEPTH, MAX_JS_PAYLOAD_BYTES, validate_bounded_json, validate_script,
};
use rust_cef_core::ports::{
    BrowserEvent, EXT_EVENT_FOCUSED_NODE, EXT_EVENT_UNCAUGHT_EXCEPTION, EXT_EVENT_V8_CONTEXT,
    EngineError,
};

use crate::client::BrowserClientState;
use crate::executor::NativeAudit;
use crate::helper_app::{
    EXTRA_BROWSER_LOCAL, EXTRA_JS_BRIDGE_ENABLED, EXTRA_JS_BRIDGE_LEGACY,
    EXTRA_JS_BRIDGE_MAX_BYTES, EXTRA_JS_BRIDGE_ORIGINS, EXTRA_JS_BRIDGE_SUBFRAMES,
    EXTRA_RUNTIME_GEN, MSG_CONTEXT_CREATED, MSG_CONTEXT_RELEASED, MSG_EVAL_REQUEST,
    MSG_EVAL_RESPONSE, MSG_FOCUSED_NODE_CHANGED, MSG_HOST_MESSAGE, MSG_POST_MESSAGE,
    MSG_UNCAUGHT_EXCEPTION,
};
use crate::policy::CompiledPolicy;

pub(crate) enum RendererIpcAction {
    PostMessage {
        origin: String,
        payload_json: String,
    },
    EvalResponse {
        id: EvalRequestId,
        outcome: EvalOutcome,
    },
    RenderEvent {
        event: BrowserEvent,
        required_ext_bit: u32,
    },
}

pub(crate) fn allowed_bridge_origins(
    options: &BrowserOptions,
    policy: &CompiledPolicy,
) -> BTreeSet<String> {
    if !options.js_bridge.enabled {
        return BTreeSet::new();
    }
    let mut set = BTreeSet::new();
    for raw in &options.js_bridge.allowed_origins {
        if raw.trim() == raw && raw.eq_ignore_ascii_case("https://*") {
            if policy.allows_https_wildcard() {
                set.insert("https://*".into());
            }
            continue;
        }
        if raw.trim() == raw && raw.eq_ignore_ascii_case("http://*") {
            if policy.allows_http_wildcard() {
                set.insert("http://*".into());
            }
            continue;
        }
        if let Some(canonical) = policy.canonical_origin_for(raw)
            && policy.allows_canonical_origin(&canonical)
        {
            set.insert(canonical);
        }
    }
    set
}

fn origin_is_bridge_permitted(permitted: &BTreeSet<String>, origin: &str) -> bool {
    permitted.contains(origin)
        || (origin.starts_with("https://") && permitted.contains("https://*"))
        || (origin.starts_with("http://") && permitted.contains("http://*"))
}

pub(crate) fn build_browser_extra_info(
    browser: BrowserId,
    options: &BrowserOptions,
    policy: &CompiledPolicy,
    allow_subframes: bool,
    legacy_binding: bool,
) -> Option<DictionaryValue> {
    let dict = dictionary_value_create()?;
    dict.set_string(
        Some(&CefString::from(EXTRA_RUNTIME_GEN)),
        Some(&CefString::from(
            browser.runtime().value().to_string().as_str(),
        )),
    );
    dict.set_string(
        Some(&CefString::from(EXTRA_BROWSER_LOCAL)),
        Some(&CefString::from(browser.local().to_string().as_str())),
    );
    let permitted = allowed_bridge_origins(options, policy);
    let enabled = options.js_bridge.enabled && !permitted.is_empty();
    dict.set_bool(
        Some(&CefString::from(EXTRA_JS_BRIDGE_ENABLED)),
        i32::from(enabled),
    );
    dict.set_bool(
        Some(&CefString::from(EXTRA_JS_BRIDGE_SUBFRAMES)),
        i32::from(enabled && allow_subframes),
    );
    dict.set_bool(
        Some(&CefString::from(EXTRA_JS_BRIDGE_LEGACY)),
        i32::from(enabled && legacy_binding),
    );
    let max_bytes = i32::try_from(
        options
            .js_bridge
            .max_message_bytes
            .min(MAX_JS_PAYLOAD_BYTES),
    )
    .unwrap_or(MAX_JS_PAYLOAD_BYTES as i32);
    dict.set_int(Some(&CefString::from(EXTRA_JS_BRIDGE_MAX_BYTES)), max_bytes);
    if let Some(list) = list_value_create() {
        for (idx, origin) in permitted.iter().enumerate() {
            list.set_string(idx, Some(&CefString::from(origin.as_str())));
        }
        dict.set_list(
            Some(&CefString::from(EXTRA_JS_BRIDGE_ORIGINS)),
            Some(&mut list.clone()),
        );
    }
    Some(dict)
}

fn resolve_verified_frame(
    native: &Browser,
    audit: &NativeAudit,
    scope: &JsFrameScope,
) -> Option<Frame> {
    audit.record();
    if native.is_valid() == 0 {
        return None;
    }
    match scope {
        JsFrameScope::MainFrame => {
            audit.record();
            let frame = native.main_frame()?;
            (frame.is_valid() != 0 && frame.is_main() != 0).then_some(frame)
        }
        JsFrameScope::FrameId(expected_id) => {
            audit.record();
            let frame = native.frame_by_identifier(Some(&CefString::from(expected_id.as_str())))?;
            if frame.is_valid() == 0 {
                return None;
            }
            let mut owner = frame.browser()?;
            let native_clone = native.clone();
            if native_clone.is_same(Some(&mut owner)) == 0 {
                return None;
            }
            crate::menu_model::read_userfree(&frame.identifier(), expected_id.len())
                .ok()
                .filter(|actual| actual == expected_id)
                .map(|_| frame)
        }
    }
}

fn frame_origin_allowed(state: &BrowserClientState, frame: &Frame) -> Option<String> {
    let url = CefString::from(&frame.url()).to_string();
    let is_main = frame.is_main() != 0;
    let allowed = state.allows_navigation_for_script(&url, is_main);
    if !allowed {
        return None;
    }
    Some(
        state
            .policy
            .rules
            .canonical_origin_for(&url)
            .unwrap_or_default(),
    )
}

pub(crate) fn execute_javascript(
    native: &Browser,
    audit: &NativeAudit,
    state: Option<&BrowserClientState>,
    script: &str,
    frame_scope: &JsFrameScope,
) -> Result<(), EngineError> {
    validate_script(script).map_err(|_| EngineError::InvalidRequest)?;
    let frame =
        resolve_verified_frame(native, audit, frame_scope).ok_or(EngineError::InvalidRequest)?;
    if let Some(client) = state
        && frame_origin_allowed(client, &frame).is_none()
    {
        return Err(EngineError::PolicyDenied);
    }
    audit.record();
    frame.execute_java_script(
        Some(&CefString::from(script)),
        Some(&CefString::from("rust-cef://host")),
        1,
    );
    Ok(())
}

pub(crate) fn send_post_js_message(
    browser: BrowserId,
    native: &Browser,
    audit: &NativeAudit,
    state: Option<&BrowserClientState>,
    payload_json: &str,
) -> Result<(), EngineError> {
    let client = state.ok_or(EngineError::Closed)?;
    if !client.options.js_bridge.enabled {
        return Err(EngineError::PolicyDenied);
    }
    validate_bounded_json(
        payload_json,
        client.options.js_bridge.max_message_bytes,
        MAX_JS_JSON_DEPTH,
    )
    .map_err(|_| EngineError::InvalidRequest)?;
    let frame = resolve_verified_frame(native, audit, &JsFrameScope::MainFrame)
        .ok_or(EngineError::NotReady)?;
    let url = CefString::from(&frame.url()).to_string();
    let origin = client
        .policy
        .rules
        .canonical_origin_for(&url)
        .ok_or(EngineError::PolicyDenied)?;
    let permitted = allowed_bridge_origins(&client.options, &client.policy.rules);
    if !origin_is_bridge_permitted(&permitted, &origin) {
        return Err(EngineError::PolicyDenied);
    }
    let Some(mut msg) = process_message_create(Some(&CefString::from(MSG_HOST_MESSAGE))) else {
        return Err(EngineError::Backend { code: None });
    };
    let Some(list) = msg.argument_list() else {
        return Err(EngineError::Backend { code: None });
    };
    list.set_string(
        0,
        Some(&CefString::from(
            browser.runtime().value().to_string().as_str(),
        )),
    );
    list.set_string(
        1,
        Some(&CefString::from(browser.local().to_string().as_str())),
    );
    let allow_subframes = client.policy.js_bridge_allow_subframes();
    let expected_origin = if allow_subframes {
        String::new()
    } else {
        origin
    };
    list.set_string(2, Some(&CefString::from(expected_origin.as_str())));
    list.set_string(3, Some(&CefString::from(payload_json)));
    audit.record();
    frame.send_process_message(ProcessId::RENDERER, Some(&mut msg));
    Ok(())
}

pub(crate) fn send_eval_request(
    browser: BrowserId,
    native: &Browser,
    audit: &NativeAudit,
    state: Option<&BrowserClientState>,
    id: EvalRequestId,
    script: &str,
    frame_scope: &JsFrameScope,
) -> Result<(), EvalError> {
    validate_script(script)?;
    let frame =
        resolve_verified_frame(native, audit, frame_scope).ok_or(EvalError::FrameNotFound)?;
    let expected_origin = if let Some(client) = state {
        frame_origin_allowed(client, &frame).ok_or(EvalError::PolicyDenied)?
    } else {
        String::new()
    };
    let Some(mut msg) = process_message_create(Some(&CefString::from(MSG_EVAL_REQUEST))) else {
        return Err(EvalError::InvalidScript);
    };
    let Some(list) = msg.argument_list() else {
        return Err(EvalError::InvalidScript);
    };
    list.set_string(
        0,
        Some(&CefString::from(
            browser.runtime().value().to_string().as_str(),
        )),
    );
    list.set_string(
        1,
        Some(&CefString::from(browser.local().to_string().as_str())),
    );
    list.set_string(2, Some(&CefString::from(id.sequence.to_string().as_str())));
    list.set_string(3, Some(&CefString::from(expected_origin.as_str())));
    list.set_string(4, Some(&CefString::from(script)));
    audit.record();
    frame.send_process_message(ProcessId::RENDERER, Some(&mut msg));
    Ok(())
}

pub(crate) fn parse_renderer_process_message(
    expected_browser: BrowserId,
    state: &BrowserClientState,
    frame: Option<&mut Frame>,
    message: &ProcessMessage,
) -> Option<RendererIpcAction> {
    let name = CefString::from(&message.name()).to_string();
    let args = message.argument_list()?;
    match name.as_str() {
        MSG_POST_MESSAGE => {
            if args.size() < 4 || !state.options.js_bridge.enabled {
                return None;
            }
            let rt = CefString::from(&args.string(0))
                .to_string()
                .parse::<u64>()
                .ok()?;
            let br = CefString::from(&args.string(1))
                .to_string()
                .parse::<u64>()
                .ok()?;
            if rt != expected_browser.runtime().value() || br != expected_browser.local() {
                return None;
            }
            let frame = frame?;
            if frame.is_valid() == 0
                || (frame.is_main() == 0 && !state.policy.js_bridge_allow_subframes())
            {
                return None;
            }
            let ipc_origin = CefString::from(&args.string(2)).to_string();
            let current_url = CefString::from(&frame.url()).to_string();
            let actual_origin = state.policy.rules.canonical_origin_for(&current_url)?;
            if actual_origin != ipc_origin {
                return None;
            }
            let permitted = allowed_bridge_origins(&state.options, &state.policy.rules);
            if !origin_is_bridge_permitted(&permitted, &actual_origin) {
                return None;
            }
            let payload_json = CefString::from(&args.string(3)).to_string();
            let max_bytes = state
                .options
                .js_bridge
                .max_message_bytes
                .min(MAX_JS_PAYLOAD_BYTES);
            validate_bounded_json(&payload_json, max_bytes, MAX_JS_JSON_DEPTH).ok()?;
            Some(RendererIpcAction::PostMessage {
                origin: actual_origin,
                payload_json,
            })
        }
        MSG_EVAL_RESPONSE => {
            if args.size() < 8 {
                return None;
            }
            let rt = CefString::from(&args.string(0))
                .to_string()
                .parse::<u64>()
                .ok()?;
            let br = CefString::from(&args.string(1))
                .to_string()
                .parse::<u64>()
                .ok()?;
            if rt != expected_browser.runtime().value() || br != expected_browser.local() {
                return None;
            }
            let seq = CefString::from(&args.string(2))
                .to_string()
                .parse::<u64>()
                .ok()?;
            let _origin = CefString::from(&args.string(3)).to_string();
            let ok = args.bool(4) != 0;
            let kind = CefString::from(&args.string(5)).to_string();
            let body = CefString::from(&args.string(6)).to_string();
            let line = args.int(7);
            let max_bytes = state
                .options
                .js_bridge
                .max_message_bytes
                .min(MAX_JS_PAYLOAD_BYTES);
            let outcome = if ok {
                match validate_bounded_json(&body, max_bytes, MAX_JS_JSON_DEPTH) {
                    Ok(()) => EvalOutcome::Value(body),
                    Err(err) => EvalOutcome::Err(err),
                }
            } else {
                let err = match kind.as_str() {
                    "exception" => EvalError::JsException {
                        message: truncate_utf8_bytes(&body, MAX_EVAL_ERROR_MESSAGE_BYTES),
                        line,
                    },
                    "payload_too_large" => EvalError::PayloadTooLarge,
                    "depth_exceeded" => EvalError::DepthExceeded,
                    "unsupported_type" => EvalError::UnsupportedResultType,
                    "policy_denied" => EvalError::PolicyDenied,
                    "invalid_json" => EvalError::InvalidJson,
                    _ => EvalError::InvalidScript,
                };
                EvalOutcome::Err(err)
            };
            Some(RendererIpcAction::EvalResponse {
                id: EvalRequestId {
                    browser: expected_browser,
                    sequence: seq,
                },
                outcome,
            })
        }
        MSG_CONTEXT_CREATED | MSG_CONTEXT_RELEASED => {
            if args.size() < 4 {
                return None;
            }
            let rt = CefString::from(&args.string(0))
                .to_string()
                .parse::<u64>()
                .ok()?;
            let br = CefString::from(&args.string(1))
                .to_string()
                .parse::<u64>()
                .ok()?;
            if rt != expected_browser.runtime().value() || br != expected_browser.local() {
                return None;
            }
            let frame_id = truncate_utf8_bytes(
                &CefString::from(&args.string(2)).to_string(),
                MAX_FRAME_ID_BYTES,
            );
            let is_main_frame = args.bool(3) != 0;
            let event = if name == MSG_CONTEXT_CREATED {
                BrowserEvent::ContextCreated {
                    frame_id,
                    is_main_frame,
                }
            } else {
                BrowserEvent::ContextReleased {
                    frame_id,
                    is_main_frame,
                }
            };
            Some(RendererIpcAction::RenderEvent {
                event,
                required_ext_bit: EXT_EVENT_V8_CONTEXT,
            })
        }
        MSG_FOCUSED_NODE_CHANGED => {
            if args.size() < 9 {
                return None;
            }
            let rt = CefString::from(&args.string(0))
                .to_string()
                .parse::<u64>()
                .ok()?;
            let br = CefString::from(&args.string(1))
                .to_string()
                .parse::<u64>()
                .ok()?;
            if rt != expected_browser.runtime().value() || br != expected_browser.local() {
                return None;
            }
            let has_node = args.bool(2) != 0;
            let info = if has_node {
                Some(FocusedDomNodeInfo {
                    frame_id: truncate_utf8_bytes(
                        &CefString::from(&args.string(3)).to_string(),
                        MAX_FRAME_ID_BYTES,
                    ),
                    is_main_frame: args.bool(4) != 0,
                    tag_name: truncate_utf8_bytes(
                        &CefString::from(&args.string(5)).to_string(),
                        MAX_DOM_IDENTIFIER_BYTES,
                    ),
                    element_id: truncate_utf8_bytes(
                        &CefString::from(&args.string(6)).to_string(),
                        MAX_DOM_IDENTIFIER_BYTES,
                    ),
                    input_type: truncate_utf8_bytes(
                        &CefString::from(&args.string(7)).to_string(),
                        MAX_DOM_IDENTIFIER_BYTES,
                    ),
                    is_editable: args.bool(8) != 0,
                })
            } else {
                None
            };
            Some(RendererIpcAction::RenderEvent {
                event: BrowserEvent::FocusedNodeChanged(info),
                required_ext_bit: EXT_EVENT_FOCUSED_NODE,
            })
        }
        MSG_UNCAUGHT_EXCEPTION => {
            if args.size() < 8 {
                return None;
            }
            let rt = CefString::from(&args.string(0))
                .to_string()
                .parse::<u64>()
                .ok()?;
            let br = CefString::from(&args.string(1))
                .to_string()
                .parse::<u64>()
                .ok()?;
            if rt != expected_browser.runtime().value() || br != expected_browser.local() {
                return None;
            }
            let info = JsUncaughtExceptionInfo {
                frame_id: truncate_utf8_bytes(
                    &CefString::from(&args.string(2)).to_string(),
                    MAX_FRAME_ID_BYTES,
                ),
                is_main_frame: args.bool(3) != 0,
                message: truncate_utf8_bytes(
                    &CefString::from(&args.string(4)).to_string(),
                    MAX_EXCEPTION_SOURCE_BYTES,
                ),
                source_url: truncate_utf8_bytes(
                    &CefString::from(&args.string(5)).to_string(),
                    MAX_EXCEPTION_SOURCE_BYTES,
                ),
                line: args.int(6),
                column: args.int(7),
            };
            Some(RendererIpcAction::RenderEvent {
                event: BrowserEvent::UncaughtJsException(info),
                required_ext_bit: EXT_EVENT_UNCAUGHT_EXCEPTION,
            })
        }
        _ => None,
    }
}
