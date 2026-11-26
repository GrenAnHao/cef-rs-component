//! Thin example RPC host built on top of [`rust_cef_gpui::js_repository`].
//!
//! Registers `bound`, `boundAsync`, lazy `boundAsync2` / `boundObject`, `PostMessage`,
//! and `UrlRequestTicket` downloads for the `--verify-tests` QUnit compatibility suites
//! while preserving `CefRsGpuiRpcHost`, `QUnitSummary`, `JsonVal`, and `parse_json`
//! entrypoints expected by `mod.rs` and `verify.rs`.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
};

use gpui::{AsyncApp, WindowHandle};
use rust_cef_gpui::{
    BoundObject, CallbackReplyKind, JsBindingOptions, JsBindingSettings, JsCallContext, JsDate,
    JsInvocationOutcome, JsInvocationToken, JsObjectRepository, JsValue, Runtime, UrlRequestState,
    UrlRequestTicket,
    js_repository::{encode_rpc_err_json, encode_rpc_ok_json},
    url_request_contracts::{UrlRequestSpec, UrlRequestStatus, UrlRequestTarget},
};

pub use rust_cef_gpui::js_repository::{JsonVal, parse_json};

use super::ExampleAction;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QUnitSummary {
    pub total: u64,
    pub passed: u64,
    pub failed: u64,
    pub runtime_ms: u64,
}

#[derive(Default)]
struct RpcHostState {
    delayed: Vec<(u64, u64, String)>,
    downloads: Vec<(JsInvocationToken, String)>,
    qunit_results: Vec<(u64, QUnitSummary)>,
    qunit_failures: Vec<(u64, String)>,
}

struct PendingUrlDownload {
    token: JsInvocationToken,
    ticket: UrlRequestTicket,
}

pub(super) struct CefRsGpuiRpcHost {
    repo: JsObjectRepository,
    state: Arc<Mutex<RpcHostState>>,
    pending_downloads: Vec<PendingUrlDownload>,
    pub qunit_results: BTreeMap<u64, QUnitSummary>,
    pub qunit_failures: Vec<(u64, String)>,
}

impl Default for CefRsGpuiRpcHost {
    fn default() -> Self {
        Self::new()
    }
}

impl CefRsGpuiRpcHost {
    pub(super) fn new() -> Self {
        let state = Arc::new(Mutex::new(RpcHostState::default()));
        let mut repo = JsObjectRepository::new(JsBindingSettings {
            legacy_binding_enabled: true,
            ..Default::default()
        });

        repo.register(build_example_object("bound", false, Arc::clone(&state)));
        repo.register(build_example_object("boundAsync", true, Arc::clone(&state)));

        let resolve_state = Arc::clone(&state);
        repo.set_resolve_object(move |name| match name {
            "bound" | "boundAsync" | "boundAsync2" | "boundObject" | "test" => Some(
                build_example_object(name, name != "bound", Arc::clone(&resolve_state)),
            ),
            _ => None,
        });

        let post_state = Arc::clone(&state);
        repo.set_post_message_handler(move |ev, out_messages| {
            let payload = &ev.raw_payload_json;
            if let Some(n) = payload.as_i64() {
                let reply = JsonVal::Obj(vec![
                    ("__cefKind".into(), JsonVal::Str("postMessageInt".into())),
                    ("ctxId".into(), JsonVal::Str(ev.ctx_id.clone())),
                    ("value".into(), JsonVal::Num(n as f64)),
                ])
                .to_json_string();
                out_messages.push((ev.tab_id, reply));
                return;
            }
            let Some(msg_type) = payload.obj_get("Type").and_then(JsonVal::as_str) else {
                return;
            };
            match msg_type {
                "Update" => {
                    if let Some(cb) = ev
                        .payload
                        .obj_get("Callback")
                        .and_then(JsValue::as_callback)
                    {
                        let msg = rust_cef_gpui::js_repository::encode_invoke_callback_json(
                            &cb.ctx_id,
                            &cb.cb_id,
                            vec![JsonVal::Str("Update".into())],
                            true,
                            None,
                        );
                        out_messages.push((cb.tab_id, msg));
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
                        post_state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .qunit_results
                            .push((ev.tab_id, summary));
                    }
                }
                "QUnitTestFailed" => {
                    if let Some(out) = payload.obj_get("Output").and_then(JsonVal::as_str) {
                        post_state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .qunit_failures
                            .push((ev.tab_id, out.to_owned()));
                    }
                }
                _ => {}
            }
        });

        Self {
            repo,
            state,
            pending_downloads: Vec::new(),
            qunit_results: BTreeMap::new(),
            qunit_failures: Vec::new(),
        }
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
            let _ = self.repo.handle_incoming(tab_id, &raw_json, out_messages);
        }

        let (delayed, downloads, results, failures) = {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                std::mem::take(&mut guard.delayed),
                std::mem::take(&mut guard.downloads),
                std::mem::take(&mut guard.qunit_results),
                std::mem::take(&mut guard.qunit_failures),
            )
        };

        for (tab_id, delay_ms, payload_json) in delayed {
            schedule_delayed_message(cx, root, tab_id, delay_ms, payload_json);
        }
        for (token, url) in downloads {
            let Some(runtime) = rt else {
                out_messages.push((
                    token.tab_id,
                    encode_rpc_err_json(&token.ctx_id, token.rpc_id, "Runtime unavailable"),
                ));
                continue;
            };
            let mut spec = UrlRequestSpec::get(UrlRequestTarget::DefaultProfile, url);
            spec.timeout_ms = 8_000;
            match runtime.start_url_request(spec) {
                Ok(ticket) => self
                    .pending_downloads
                    .push(PendingUrlDownload { token, ticket }),
                Err(err) => out_messages.push((
                    token.tab_id,
                    encode_rpc_err_json(
                        &token.ctx_id,
                        token.rpc_id,
                        &format!("start_url_request: {err:?}"),
                    ),
                )),
            }
        }
        for (tab_id, summary) in results {
            self.qunit_results.insert(tab_id, summary);
        }
        self.qunit_failures.extend(failures);
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
                    if ok {
                        let text = String::from_utf8_lossy(&resp.body);
                        let lines: Vec<JsValue> = text
                            .split('\n')
                            .filter(|line| !line.trim().is_empty())
                            .map(|line| JsValue::from(line.trim_end_matches('\r')))
                            .collect();
                        self.repo.finish_rpc(&item.token, JsValue::Array(lines));
                    } else {
                        self.repo.fail_rpc(
                            &item.token,
                            &format!(
                                "Download failed (status={:?}, http={:?}, err={})",
                                resp.status, resp.http_status, resp.error_code
                            ),
                        );
                    }
                }
                UrlRequestState::Failed(err) => {
                    let item = self.pending_downloads.swap_remove(idx);
                    self.repo
                        .fail_rpc(&item.token, &format!("UrlRequest failed: {err:?}"));
                }
            }
        }
        out_messages.extend(self.repo.drain_outbound());
    }
}

fn push_delayed(
    state: &Arc<Mutex<RpcHostState>>,
    tab_id: u64,
    delay_ms: u64,
    payload_json: String,
) {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .delayed
        .push((tab_id, delay_ms, payload_json));
}

fn build_example_object(
    name: &str,
    is_async: bool,
    state: Arc<Mutex<RpcHostState>>,
) -> BoundObject {
    let mut obj = BoundObject::new(name).with_options(JsBindingOptions {
        is_async,
        ..Default::default()
    });

    obj = obj
        .with_method("error", |_ctx, _args| {
            Err("This is an exception coming from C#".into())
        })
        .with_method("div", |_ctx, args| {
            let a = arg_i64(&args, 0).unwrap_or(0);
            let b = arg_i64(&args, 1).unwrap_or(0);
            if b == 0 {
                Err("Attempted to divide by zero.".into())
            } else {
                Ok(JsValue::from(a / b))
            }
        })
        .with_method("divWithBlockingTaskCall", |_ctx, args| {
            let a = arg_i64(&args, 0).unwrap_or(0);
            let b = arg_i64(&args, 1).unwrap_or(0);
            if b == 0 {
                Err("Attempted to divide by zero.".into())
            } else {
                Ok(JsValue::from((a / b).to_string()))
            }
        })
        .with_method("uIntAddModel", |_ctx, args| {
            let model = args.first();
            let a = model
                .and_then(|m| m.obj_get("ParamA").or_else(|| m.obj_get("paramA")))
                .and_then(JsValue::as_i64)
                .unwrap_or(0);
            let b = model
                .and_then(|m| m.obj_get("ParamB").or_else(|| m.obj_get("paramB")))
                .and_then(JsValue::as_i64)
                .unwrap_or(0);
            Ok(JsValue::from(a + b))
        })
        .with_method("uIntAdd", |_ctx, args| {
            let a = arg_i64(&args, 0).unwrap_or(0);
            let b = arg_i64(&args, 1).unwrap_or(0);
            Ok(JsValue::from(a + b))
        })
        .with_method("hello", |_ctx, args| {
            let name = arg_str(&args, 0).unwrap_or_default();
            Ok(JsValue::from(format!("Hello {name}")))
        })
        .with_method("returnObject", return_value_obj)
        .with_method("returnClass", return_value_obj)
        .with_method("returnClassAsJsonString", return_value_obj)
        .with_method("returnStructArray", return_item_pair)
        .with_method("returnClassesArray", return_item_pair)
        .with_method("echoDateTime", |_ctx, args| {
            Ok(args.into_iter().next().unwrap_or(JsValue::Null))
        })
        .with_method("echoNullableDateTime", |_ctx, args| {
            Ok(args.into_iter().next().unwrap_or(JsValue::Null))
        })
        .with_method("echoArray", |_ctx, args| {
            Ok(args
                .into_iter()
                .next()
                .unwrap_or_else(|| JsValue::Array(Vec::new())))
        })
        .with_method("echoValueTypeArray", |_ctx, args| {
            let items = args
                .first()
                .and_then(JsValue::as_array)
                .map(|slice| {
                    slice
                        .iter()
                        .map(|v| JsValue::from(v.as_i64().unwrap_or(0)))
                        .collect()
                })
                .unwrap_or_default();
            Ok(JsValue::Array(items))
        })
        .with_method("echoMultidimensionalArray", |_ctx, args| {
            Ok(args
                .into_iter()
                .next()
                .unwrap_or_else(|| JsValue::Array(Vec::new())))
        })
        .with_method("dynamiObjectList", |_ctx, args| {
            let mut builder = String::new();
            if let Some(items) = args.first().and_then(JsValue::as_array) {
                for b in items {
                    let bname = b.obj_get("Name").and_then(JsValue::as_str).unwrap_or("");
                    let eng = b
                        .obj_get("Engine")
                        .and_then(|e| e.obj_get("Name"))
                        .and_then(JsValue::as_str)
                        .unwrap_or("");
                    builder.push_str(&format!("Browser(Name:{bname};Engine:{eng});"));
                }
            }
            Ok(JsValue::from(builder))
        })
        .with_method("methodReturnsList", |_ctx, _args| {
            Ok(JsValue::Array(vec![
                JsValue::from("Element 0 - First"),
                JsValue::from("Element 1"),
                JsValue::from("Element 2 - Last"),
            ]))
        })
        .with_method("methodReturnsListOfLists", |_ctx, _args| {
            Ok(JsValue::Array(vec![
                JsValue::Array(vec![
                    JsValue::from("Element 0, 0"),
                    JsValue::from("Element 0, 1"),
                ]),
                JsValue::Array(vec![
                    JsValue::from("Element 1, 0"),
                    JsValue::from("Element 1, 1"),
                ]),
                JsValue::Array(vec![
                    JsValue::from("Element 2, 0"),
                    JsValue::from("Element 2, 1"),
                ]),
            ]))
        })
        .with_method("methodReturnsDictionary1", |_ctx, _args| {
            Ok(JsValue::Object(vec![
                ("five".into(), JsValue::from(5)),
                ("ten".into(), JsValue::from(10)),
            ]))
        })
        .with_method("methodReturnsDictionary2", |_ctx, _args| {
            Ok(dict2_js_value())
        })
        .with_method("methodReturnsDictionary3", |_ctx, _args| {
            Ok(JsValue::Object(vec![("data".into(), dict2_js_value())]))
        })
        .with_method("passSimpleClassAsArgument", |_ctx, args| {
            let res = match args.first() {
                Some(obj @ JsValue::Object(_)) => {
                    let ts = obj
                        .obj_get("TestString")
                        .and_then(JsValue::as_str)
                        .unwrap_or("");
                    let p1 = obj
                        .obj_get("SubClasses")
                        .and_then(JsValue::as_array)
                        .and_then(|a| a.first())
                        .and_then(|s| s.obj_get("PropertyOne"))
                        .and_then(JsValue::as_str)
                        .unwrap_or("");
                    JsValue::Object(vec![
                        ("Item1".into(), JsValue::from(true)),
                        (
                            "Item2".into(),
                            JsValue::from(format!(
                                "TestString:{ts};SubClasses[0].PropertyOne:{p1}"
                            )),
                        ),
                    ])
                }
                _ => JsValue::Object(vec![
                    ("Item1".into(), JsValue::from(false)),
                    (
                        "Item2".into(),
                        JsValue::from("PassSimpleClassAsArgument dictionary param is null"),
                    ),
                ]),
            };
            Ok(res)
        })
        .with_method("returnTaskStringAsync", |_ctx, _args| {
            Ok(JsValue::from("ReturnTaskStringAsync"))
        })
        .with_method("setCallBack", |_ctx, _args| Ok(JsValue::Null));

    let st = Arc::clone(&state);
    obj = obj.with_raw_method("doSomething", move |ctx, _args| {
        let reply = encode_rpc_ok_json(
            &ctx.token.ctx_id,
            ctx.token.rpc_id,
            JsonVal::Str("Waited for 1000ms before returning".into()),
        );
        push_delayed(&st, ctx.token.tab_id, 1000, reply);
        JsInvocationOutcome::Deferred
    });

    let st = Arc::clone(&state);
    obj = obj.with_raw_method("waitBeforeReturnAsync", move |ctx, args| {
        let ms = arg_i64(&args, 0).unwrap_or(0).clamp(0, 10_000) as u64;
        let reply = encode_rpc_ok_json(&ctx.token.ctx_id, ctx.token.rpc_id, JsonVal::Null);
        push_delayed(&st, ctx.token.tab_id, ms, reply);
        JsInvocationOutcome::Deferred
    });

    let st = Arc::clone(&state);
    obj = obj.with_raw_method("voidReturnAsync", move |ctx, _args| {
        let reply = encode_rpc_ok_json(&ctx.token.ctx_id, ctx.token.rpc_id, JsonVal::Null);
        push_delayed(&st, ctx.token.tab_id, 1000, reply);
        JsInvocationOutcome::Deferred
    });

    let st = Arc::clone(&state);
    obj = obj.with_raw_method("asyncWaitTwoSeconds", move |ctx, args| {
        let echoed = args.first().map(JsValue::to_json).unwrap_or(JsonVal::Null);
        let reply = encode_rpc_ok_json(&ctx.token.ctx_id, ctx.token.rpc_id, echoed);
        push_delayed(&st, ctx.token.tab_id, 2000, reply);
        JsInvocationOutcome::Deferred
    });

    let st = Arc::clone(&state);
    obj = obj.with_raw_method("asyncThrowException", move |ctx, _args| {
        let reply = encode_rpc_err_json(&ctx.token.ctx_id, ctx.token.rpc_id, "Expected Exception");
        push_delayed(&st, ctx.token.tab_id, 2000, reply);
        JsInvocationOutcome::Deferred
    });

    let st = Arc::clone(&state);
    obj = obj.with_raw_method("asyncDownloadFileAndSplitOnNewLines", move |ctx, args| {
        let url = arg_str(&args, 0).unwrap_or_default().to_owned();
        st.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .downloads
            .push((ctx.token.clone(), url));
        JsInvocationOutcome::Deferred
    });

    obj = obj
        .with_raw_method("javascriptCallbackEvalPromise", |ctx, args| {
            let msg_text = arg_str(&args, 0).unwrap_or_default().to_owned();
            let Some(cb) = args.get(1).and_then(JsValue::as_callback).cloned() else {
                return JsInvocationOutcome::Immediate(Err("Missing callback".into()));
            };
            ctx.invoke_callback(
                &cb,
                vec![JsValue::from(msg_text)],
                true,
                Some(CallbackReplyKind::ForwardRpcResult {
                    reply_to: ctx.token.clone(),
                }),
            );
            JsInvocationOutcome::Deferred
        })
        .with_raw_method("javascriptOptionalCallbackEvalPromise", |ctx, args| {
            let invoke = args.first().and_then(JsValue::as_bool).unwrap_or(false);
            let msg_text = arg_str(&args, 1).unwrap_or_default().to_owned();
            if !invoke {
                return JsInvocationOutcome::Immediate(Ok(JsValue::from(msg_text)));
            }
            let Some(cb) = args.get(2).and_then(JsValue::as_callback).cloned() else {
                return JsInvocationOutcome::Immediate(Err("Missing callback".into()));
            };
            ctx.invoke_callback(
                &cb,
                vec![JsValue::from(msg_text)],
                true,
                Some(CallbackReplyKind::ForwardRpcResult {
                    reply_to: ctx.token.clone(),
                }),
            );
            JsInvocationOutcome::Deferred
        });

    let st = Arc::clone(&state);
    obj = obj.with_raw_method("testCallback", move |ctx, args| {
        if let Some(cb) = args.first().and_then(JsValue::as_callback).cloned() {
            let reply = ctx.build_callback_json(
                &cb,
                vec![JsValue::Object(vec![(
                    "Response".into(),
                    JsValue::from("This callback from C# was delayed 1500ms"),
                )])],
                true,
                None,
            );
            push_delayed(&st, cb.tab_id, 1500, reply);
        }
        JsInvocationOutcome::Immediate(Ok(JsValue::Null))
    });

    let st = Arc::clone(&state);
    obj = obj.with_raw_method("testCallbackFromObject", move |ctx, args| {
        if let Some(arg) = args.first()
            && let Some(cb) = arg
                .obj_get("Callback")
                .and_then(JsValue::as_callback)
                .cloned()
        {
            let ts = arg
                .obj_get("TestString")
                .and_then(JsValue::as_str)
                .unwrap_or("");
            let p1 = arg
                .obj_get("SubClasses")
                .and_then(JsValue::as_array)
                .and_then(|a| a.first())
                .and_then(|s| s.obj_get("PropertyOne"))
                .and_then(JsValue::as_str)
                .unwrap_or("");
            let msg_out = format!("message from C# {ts} - {p1}");
            let reply = ctx.build_callback_json(&cb, vec![JsValue::from(msg_out)], true, None);
            push_delayed(&st, cb.tab_id, 1500, reply);
        }
        JsInvocationOutcome::Immediate(Ok(JsValue::Null))
    });

    for (method_name, year) in [
        ("testCallbackWithDateTime", 2019i32),
        ("testCallbackWithDateTime1900", 1900),
        ("testCallbackWithDateTime1970", 1970),
        ("testCallbackWithDateTime1985", 1985),
    ] {
        obj = obj.with_raw_method(method_name, move |ctx, args| {
            if let Some(cb) = args.first().and_then(JsValue::as_callback).cloned() {
                let arr = JsValue::Array(vec![
                    JsValue::from(year),
                    JsValue::from(1),
                    JsValue::from(1),
                    JsValue::from(12),
                    JsValue::from(0),
                    JsValue::from(0),
                ]);
                let dt = JsValue::Date(JsDate::LocalComponents {
                    year,
                    month: 1,
                    day: 1,
                    hour: 12,
                    minute: 0,
                    second: 0,
                });
                ctx.invoke_callback(&cb, vec![dt, arr], true, None);
            }
            JsInvocationOutcome::Immediate(Ok(JsValue::Null))
        });
    }

    let st = state;
    obj = obj.with_raw_method("testCallbackException", move |ctx, args| {
        if let (Some(err_cb), Some(res_cb)) = (
            args.first().and_then(JsValue::as_callback).cloned(),
            args.get(1).and_then(JsValue::as_callback).cloned(),
        ) {
            let reply = ctx.build_callback_json(
                &err_cb,
                vec![JsValue::from("This callback from C# was delayed 500ms")],
                true,
                Some(CallbackReplyKind::ReportCallbackOutcome {
                    result_cb: res_cb,
                    on_unexpected_ok: "Fatal: No Exception thrown in error callback".into(),
                    error_prefix: "Exception Thrown: ".into(),
                }),
            );
            push_delayed(&st, err_cb.tab_id, 500, reply);
        }
        JsInvocationOutcome::Immediate(Ok(JsValue::Null))
    });

    obj
}

fn return_value_obj(_ctx: &mut JsCallContext<'_>, args: Vec<JsValue>) -> Result<JsValue, String> {
    let name = arg_str(&args, 0).unwrap_or_default().to_owned();
    Ok(JsValue::Object(vec![("Value".into(), JsValue::from(name))]))
}

fn return_item_pair(_ctx: &mut JsCallContext<'_>, args: Vec<JsValue>) -> Result<JsValue, String> {
    let name = arg_str(&args, 0).unwrap_or_default();
    Ok(JsValue::Array(vec![
        JsValue::Object(vec![(
            "Value".into(),
            JsValue::from(format!("{name}Item1")),
        )]),
        JsValue::Object(vec![(
            "Value".into(),
            JsValue::from(format!("{name}Item2")),
        )]),
    ]))
}

fn dict2_js_value() -> JsValue {
    JsValue::Object(vec![
        ("onepointfive".into(), JsValue::from(1.5)),
        ("five".into(), JsValue::from(5)),
        ("ten".into(), JsValue::from("ten")),
        (
            "twotwo".into(),
            JsValue::Array(vec![JsValue::from(2), JsValue::from(2)]),
        ),
    ])
}

fn arg_str(args: &[JsValue], idx: usize) -> Option<&str> {
    args.get(idx).and_then(JsValue::as_str)
}

fn arg_i64(args: &[JsValue], idx: usize) -> Option<i64> {
    args.get(idx).and_then(JsValue::as_i64)
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
    fn json_parser_roundtrip() {
        let raw = r#"{"a":123,"b":-4.5,"c":true,"d":null,"arr":[1,"hi\n\t\"\\"],"u":"\u4e2d\uD83D\uDE00"}"#;
        let val = parse_json(raw).expect("parse");
        assert_eq!(val.obj_get("a").and_then(JsonVal::as_i64), Some(123));
        assert_eq!(val.obj_get("b").and_then(JsonVal::as_f64), Some(-4.5));
        assert_eq!(val.obj_get("c").and_then(JsonVal::as_bool), Some(true));
        assert_eq!(val.obj_get("u").and_then(JsonVal::as_str), Some("中😀"));
        let reencoded = val.to_json_string();
        let reparsed = parse_json(&reencoded).expect("reparse");
        assert_eq!(val, reparsed);
    }

    #[test]
    fn thin_rpc_host_resolves_bound_objects_and_dispatches_methods() {
        let mut host = CefRsGpuiRpcHost::default();
        let mut out = Vec::new();
        let bind_msg = JsonVal::Obj(vec![
            ("__cefKind".into(), JsonVal::Str("bindObject".into())),
            ("ctxId".into(), JsonVal::Str("c1".into())),
            ("id".into(), JsonVal::Num(1.0)),
            (
                "names".into(),
                JsonVal::Arr(vec![
                    JsonVal::Str("boundAsync2".into()),
                    JsonVal::Str("test".into()),
                ]),
            ),
        ])
        .to_json_string();
        assert!(host.repo.handle_incoming(7, &bind_msg, &mut out));
        let bind_resp = parse_json(&out.pop().unwrap().1).expect("bind resp");
        assert_eq!(
            bind_resp.obj_get("ok").and_then(JsonVal::as_bool),
            Some(true)
        );

        let hello_msg = JsonVal::Obj(vec![
            ("__cefKind".into(), JsonVal::Str("rpcCall".into())),
            ("ctxId".into(), JsonVal::Str("c1".into())),
            ("id".into(), JsonVal::Num(2.0)),
            ("target".into(), JsonVal::Str("boundAsync2".into())),
            ("method".into(), JsonVal::Str("hello".into())),
            (
                "args".into(),
                JsonVal::Arr(vec![JsonVal::Str("world".into())]),
            ),
        ])
        .to_json_string();
        assert!(host.repo.handle_incoming(7, &hello_msg, &mut out));
        let res = parse_json(&out.pop().unwrap().1).expect("hello result");
        assert_eq!(
            res.obj_get("result").and_then(JsonVal::as_str),
            Some("Hello world")
        );
    }
}
