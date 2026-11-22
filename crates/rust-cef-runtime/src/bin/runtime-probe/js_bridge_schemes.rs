//! Real CEF JS Bridge & Custom Schemes integration probe scenario.
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rust_cef_core::contracts::{BrowserOptions, RuntimeSettings, SessionState, ThreadMode};
use rust_cef_core::custom_schemes::{
    CustomSchemeConfig, CustomSchemeOptions, InterceptAction, InterceptRequest, MemoryAsset,
    ProfileSchemeBinding, ResourceInterceptor, SchemeAssetSource,
};
use rust_cef_core::js_bridge::{
    EvalError, EvalOutcome, EvalRequestId, JsBridgeConfig, JsFrameScope, MAX_JS_PAYLOAD_BYTES,
};
use rust_cef_core::ports::{BrowserCommand, BrowserEvent, BrowserHandle, CloseMode, PolicyAction};
use rust_cef_runtime::runtime::Runtime;
use rust_cef_runtime::test_support::JsBridgeSchemesReport;

type Tick = fn(&mut Runtime, ThreadMode) -> Result<(), String>;

const APP_INDEX_HTML: &[u8] = br#"<!doctype html><body style="margin:0;background:#1a4c7e"><script>
window.__lastAck = null;
if (window.rustCef) {
  window.rustCef.onMessage((raw) => {
    try {
      const msg = JSON.parse(raw);
      window.__lastAck = msg;
      window.rustCef.postMessage(JSON.stringify({ kind: 'ack', echo: msg.ping ?? null }));
    } catch (e) {
      window.rustCef.postMessage(JSON.stringify({ kind: 'ack_err', error: String(e) }));
    }
  });
}
document.title = 'app:ready';
</script></body>"#;

const APP_PAGE2_HTML: &[u8] = br#"<!doctype html><body style="margin:0;background:#1a4c7e"><script>
document.title = 'app:page2';
</script></body>"#;

const APP_DATA_JSON: &[u8] = br#"{"scheme":"app","ok":true}"#;

const DIR_INDEX_HTML: &[u8] = br#"<!doctype html><body style="margin:0;background:#1a4c7e"><script>
document.title = 'dir-assets:ready';
</script></body>"#;

pub struct DirFixture {
    base: PathBuf,
    pub root: PathBuf,
    junction: PathBuf,
}

impl DirFixture {
    pub fn create() -> Result<Self, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| format!("clock: {e}"))?
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "gpui-cef-js-schemes-{}-{nanos}",
            std::process::id()
        ));
        let root = base.join("bundle");
        let outside = base.join("outside");
        fs::create_dir_all(&root).map_err(|e| format!("mkdir bundle: {e}"))?;
        fs::create_dir_all(&outside).map_err(|e| format!("mkdir outside: {e}"))?;
        fs::write(root.join("index.html"), DIR_INDEX_HTML)
            .map_err(|e| format!("write index.html: {e}"))?;
        fs::write(root.join("hello.txt"), b"hello-from-confined-dir")
            .map_err(|e| format!("write hello.txt: {e}"))?;
        fs::write(outside.join("secret.txt"), b"OUTSIDE_SECRET_DO_NOT_LEAK")
            .map_err(|e| format!("write secret.txt: {e}"))?;
        let junction = root.join("escape_junction");
        let status = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &junction.to_string_lossy(),
                &outside.to_string_lossy(),
            ])
            .output()
            .map_err(|e| format!("spawn mklink: {e}"))?;
        if !status.status.success() {
            return Err(format!(
                "mklink /J failed: {}",
                String::from_utf8_lossy(&status.stderr)
            ));
        }
        Ok(Self {
            base,
            root,
            junction,
        })
    }

    pub fn dir_binding(&self) -> ProfileSchemeBinding {
        ProfileSchemeBinding {
            scheme: "dir".into(),
            domain: Some("assets".into()),
            source: SchemeAssetSource::Directory {
                root: self.root.clone(),
                default_page: "index.html".into(),
            },
        }
    }
}

impl Drop for DirFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.junction);
        let _ = fs::remove_dir_all(&self.base);
    }
}

pub fn runtime_settings() -> RuntimeSettings {
    let mut app_assets = BTreeMap::new();
    app_assets.insert(
        "index.html".into(),
        MemoryAsset::html(APP_INDEX_HTML.to_vec()),
    );
    app_assets.insert(
        "page2.html".into(),
        MemoryAsset::html(APP_PAGE2_HTML.to_vec()),
    );
    app_assets.insert(
        "data.json".into(),
        MemoryAsset::json(APP_DATA_JSON.to_vec()),
    );
    RuntimeSettings {
        custom_schemes: vec![
            CustomSchemeConfig {
                scheme: "app".into(),
                options: CustomSchemeOptions::standard_app(),
                domain: Some("local".into()),
                asset_source: Some(SchemeAssetSource::Memory(app_assets)),
            },
            CustomSchemeConfig {
                scheme: "dir".into(),
                options: CustomSchemeOptions::standard_app(),
                domain: Some("assets".into()),
                asset_source: None,
            },
        ],
        ..Default::default()
    }
}

pub fn browser_options(_index: u64) -> BrowserOptions {
    // Enable JS bridge strictly for `app://local` on BOTH browsers so Browser 2
    // (`dir://assets`) verifies origin gating keeps `window.rustCef` absent.
    BrowserOptions::default().with_js_bridge(JsBridgeConfig {
        enabled: true,
        allowed_origins: vec!["app://local".into()],
        max_message_bytes: MAX_JS_PAYLOAD_BYTES,
        ..Default::default()
    })
}

pub fn initial_url(index: u64) -> String {
    if index == 0 {
        "app://local/index.html".into()
    } else {
        "dir://assets/index.html".into()
    }
}

#[derive(Default)]
pub struct ProbeInterceptor {
    pub disallowed_calls: AtomicU64,
    pub header_added_hits: AtomicU64,
    pub intercepted_hits: AtomicU64,
}

impl ResourceInterceptor for ProbeInterceptor {
    fn intercept(&self, request: &InterceptRequest) -> InterceptAction {
        if request.url.contains("evil.invalid") {
            self.disallowed_calls.fetch_add(1, Ordering::Relaxed);
            return InterceptAction::Cancel;
        }
        if request.url == "app://local/intercepted.json" {
            self.intercepted_hits.fetch_add(1, Ordering::Relaxed);
            return InterceptAction::Respond {
                status_code: 201,
                mime_type: "application/json".into(),
                headers: vec![
                    ("X-Intercepted".into(), "yes".into()),
                    ("Host".into(), "evil.invalid".into()),
                ],
                body: br#"{"intercepted":true,"code":201}"#.to_vec(),
            };
        }
        if request.url == "app://local/data.json" {
            self.header_added_hits.fetch_add(1, Ordering::Relaxed);
            return InterceptAction::AddHeaders(vec![("X-Host-Tag".into(), "ok".into())]);
        }
        InterceptAction::Continue
    }
}

fn drain_events(
    handle: &BrowserHandle,
    evals: &mut BTreeMap<u64, EvalOutcome>,
    messages: &mut Vec<(String, String)>,
) -> Result<(), String> {
    for ev in handle.take_js_events() {
        match ev {
            BrowserEvent::JsMessageReceived {
                origin,
                payload_json,
            } => messages.push((origin, payload_json)),
            BrowserEvent::EvalCompleted { id, outcome } => {
                if evals.insert(id.sequence, outcome).is_some() {
                    return Err(format!(
                        "duplicate EvalCompleted for sequence {}",
                        id.sequence
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn wait_eval(
    rt: &mut Runtime,
    mode: ThreadMode,
    handle: &BrowserHandle,
    tick: Tick,
    id: EvalRequestId,
    evals: &mut BTreeMap<u64, EvalOutcome>,
    messages: &mut Vec<(String, String)>,
    deadline: Instant,
    label: &str,
) -> Result<EvalOutcome, String> {
    loop {
        drain_events(handle, evals, messages)?;
        if let Some(outcome) = evals.remove(&id.sequence) {
            return Ok(outcome);
        }
        if Instant::now() >= deadline {
            return Err(format!("js_bridge_schemes eval '{label}' timeout"));
        }
        tick(rt, mode)?;
    }
}

fn wait_title(
    rt: &mut Runtime,
    mode: ThreadMode,
    handle: &BrowserHandle,
    tick: Tick,
    expected: &str,
    deadline: Instant,
) -> Result<(), String> {
    loop {
        let snap = handle.snapshot();
        if !snap.navigation.is_loading && snap.navigation.title == expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "js_bridge_schemes wait_title '{expected}' timeout (got {:?}, loading={})",
                snap.navigation.title, snap.navigation.is_loading
            ));
        }
        tick(rt, mode)?;
    }
}

fn wait_message_kind(
    rt: &mut Runtime,
    mode: ThreadMode,
    handle: &BrowserHandle,
    tick: Tick,
    evals: &mut BTreeMap<u64, EvalOutcome>,
    messages: &mut Vec<(String, String)>,
    expected_kind: &str,
    deadline: Instant,
) -> Result<(String, serde_json::Value), String> {
    loop {
        drain_events(handle, evals, messages)?;
        for (idx, (origin, payload)) in messages.iter().enumerate() {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(payload)
                && val.get("kind").and_then(|v| v.as_str()) == Some(expected_kind)
            {
                let origin = origin.clone();
                messages.remove(idx);
                return Ok((origin, val));
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "js_bridge_schemes wait_message '{expected_kind}' timeout (seen={messages:?})"
            ));
        }
        tick(rt, mode)?;
    }
}

pub fn run(
    rt: &mut Runtime,
    mode: ThreadMode,
    handles: &[BrowserHandle],
    tick: Tick,
    interceptor: &ProbeInterceptor,
) -> Result<JsBridgeSchemesReport, String> {
    let mut report = JsBridgeSchemesReport::default();
    let deadline = Instant::now() + Duration::from_secs(25);
    let b1 = handles
        .first()
        .ok_or_else(|| "missing app browser handle".to_owned())?;
    let b2 = handles
        .get(1)
        .ok_or_else(|| "missing dir browser handle".to_owned())?;

    wait_title(rt, mode, b1, tick, "app:ready", deadline)?;
    wait_title(rt, mode, b2, tick, "dir-assets:ready", deadline)?;
    report.app_page_ready = true;

    let mut evals1 = BTreeMap::new();
    let mut msgs1 = Vec::new();
    let mut evals2 = BTreeMap::new();
    let mut msgs2 = Vec::new();

    // 1. Authorized origin has frozen `window.rustCef`.
    let bridge_id = b1
        .evaluate_script(
            "(() => ({ has: !!window.rustCef, frozen: Object.isFrozen(window.rustCef), post: typeof window.rustCef?.postMessage, on: typeof window.rustCef?.onMessage }))()".into(),
            JsFrameScope::MainFrame,
            5000,
        )
        .map_err(|e| format!("eval frozen check: {e:?}"))?;
    match wait_eval(
        rt,
        mode,
        b1,
        tick,
        bridge_id,
        &mut evals1,
        &mut msgs1,
        deadline,
        "bridge_frozen",
    )? {
        EvalOutcome::Value(json) => {
            let v: serde_json::Value =
                serde_json::from_str(&json).map_err(|e| format!("parse bridge_frozen: {e}"))?;
            report.authorized_bridge_frozen = v["has"] == true
                && v["frozen"] == true
                && v["post"] == "function"
                && v["on"] == "function";
            report.eval_json_ok = true;
        }
        other => return Err(format!("unexpected bridge_frozen outcome: {other:?}")),
    }

    // 2. Unauthorized origin (`dir://assets`) must NOT receive `window.rustCef`.
    let b2_bridge_id = b2
        .evaluate_script(
            "typeof window.rustCef".into(),
            JsFrameScope::MainFrame,
            5000,
        )
        .map_err(|e| format!("eval b2 bridge absent: {e:?}"))?;
    match wait_eval(
        rt,
        mode,
        b2,
        tick,
        b2_bridge_id,
        &mut evals2,
        &mut msgs2,
        deadline,
        "unauthorized_bridge_absent",
    )? {
        EvalOutcome::Value(json) => {
            report.unauthorized_bridge_absent = json == "\"undefined\"";
        }
        other => return Err(format!("unexpected b2 bridge outcome: {other:?}")),
    }

    // 3. Eval JS exception and unsupported return type.
    let exc_id = b1
        .evaluate_script(
            "(() => { throw new Error('boom-eval'); })()".into(),
            JsFrameScope::MainFrame,
            5000,
        )
        .map_err(|e| format!("eval exception: {e:?}"))?;
    match wait_eval(
        rt,
        mode,
        b1,
        tick,
        exc_id,
        &mut evals1,
        &mut msgs1,
        deadline,
        "eval_exception",
    )? {
        EvalOutcome::Err(EvalError::JsException { message, .. }) => {
            report.eval_exception_ok = message.contains("boom-eval");
        }
        other => return Err(format!("unexpected eval_exception outcome: {other:?}")),
    }

    let fn_id = b1
        .evaluate_script("(() => () => 123)()".into(), JsFrameScope::MainFrame, 5000)
        .map_err(|e| format!("eval unsupported: {e:?}"))?;
    match wait_eval(
        rt,
        mode,
        b1,
        tick,
        fn_id,
        &mut evals1,
        &mut msgs1,
        deadline,
        "eval_unsupported",
    )? {
        EvalOutcome::Err(EvalError::UnsupportedResultType) => {
            report.eval_unsupported_rejected = true;
        }
        other => return Err(format!("unexpected eval_unsupported outcome: {other:?}")),
    }

    // 4. ExecuteJavaScript running same-origin fetch + intercepted fetch + blocked fetch -> postMessage.
    let fetch_script = r#"
(async () => {
  let dataOk = false;
  let dataScheme = '';
  try {
    const r = await fetch('app://local/data.json');
    const j = await r.json();
    dataOk = r.status === 200 && j.ok === true;
    dataScheme = String(j.scheme || '');
  } catch (_) {}
  let interceptedStatus = 0;
  let interceptedOk = false;
  try {
    const r = await fetch('app://local/intercepted.json');
    interceptedStatus = r.status;
    const j = await r.json();
    interceptedOk = j.intercepted === true && j.code === 201;
  } catch (_) {}
  let evilBlocked = false;
  try {
    await fetch('https://evil.invalid/steal');
  } catch (_) {
    evilBlocked = true;
  }
  window.__executedMarker = 'ok';
  window.rustCef.postMessage(JSON.stringify({
    kind: 'fetch_probe',
    dataOk,
    dataScheme,
    interceptedStatus,
    interceptedOk,
    evilBlocked
  }));
})();
"#;
    b1.submit(BrowserCommand::ExecuteJavaScript {
        script: fetch_script.into(),
        frame_scope: JsFrameScope::MainFrame,
    })
    .map_err(|e| format!("execute_javascript fetch_probe: {e:?}"))?;

    let (origin, fetch_msg) = wait_message_kind(
        rt,
        mode,
        b1,
        tick,
        &mut evals1,
        &mut msgs1,
        "fetch_probe",
        deadline,
    )?;
    report.page_to_host_msg_ok = origin == "app://local";
    report.same_origin_fetch_ok = fetch_msg["dataOk"] == true && fetch_msg["dataScheme"] == "app";
    report.interceptor_status = fetch_msg["interceptedStatus"].as_u64().unwrap_or(0) as u16;
    report.interceptor_body_ok = fetch_msg["interceptedOk"] == true;
    report.external_fetch_blocked = fetch_msg["evilBlocked"] == true;
    report.interceptor_header_hits = interceptor.header_added_hits.load(Ordering::Acquire);
    report.interceptor_disallowed_calls = interceptor.disallowed_calls.load(Ordering::Acquire);

    // Check ExecuteJavaScript side-effect cleanly.
    let marker_id = b1
        .evaluate_script(
            "window.__executedMarker === 'ok'".into(),
            JsFrameScope::MainFrame,
            5000,
        )
        .map_err(|e| format!("eval marker: {e:?}"))?;
    match wait_eval(
        rt,
        mode,
        b1,
        tick,
        marker_id,
        &mut evals1,
        &mut msgs1,
        deadline,
        "execute_js_marker",
    )? {
        EvalOutcome::Value(json) => {
            report.execute_js_ok = json == "true";
        }
        other => return Err(format!("unexpected marker outcome: {other:?}")),
    }

    // 5. Bidirectional Host -> Page `PostJsMessage`.
    b1.submit(BrowserCommand::PostJsMessage {
        payload_json: r#"{"ping":99}"#.into(),
    })
    .map_err(|e| format!("post_js_message: {e:?}"))?;
    let (ack_origin, ack_msg) =
        wait_message_kind(rt, mode, b1, tick, &mut evals1, &mut msgs1, "ack", deadline)?;
    report.host_to_page_ack_ok = ack_origin == "app://local" && ack_msg["echo"] == 99;

    // 6. Confined Directory asset fetch + traversal (%2e%2e) + NTFS junction escape on Browser 2.
    let dir_script = r#"
(async () => {
  let okStatus = 0;
  let okText = '';
  try {
    const r = await fetch('dir://assets/hello.txt');
    okStatus = r.status;
    okText = await r.text();
  } catch (_) {}
  let travStatus = 0;
  try {
    const r = await fetch('dir://assets/%2e%2e/outside/secret.txt');
    travStatus = r.status;
  } catch (_) { travStatus = 404; }
  let juncStatus = 0;
  try {
    const r = await fetch('dir://assets/escape_junction/secret.txt');
    juncStatus = r.status;
  } catch (_) { juncStatus = 404; }
  window.__dirCheck = { okStatus, okText, travStatus, juncStatus };
  document.title = 'dir-assets:checked';
})();
"#;
    b2.submit(BrowserCommand::ExecuteJavaScript {
        script: dir_script.into(),
        frame_scope: JsFrameScope::MainFrame,
    })
    .map_err(|e| format!("execute dir_script: {e:?}"))?;
    wait_title(rt, mode, b2, tick, "dir-assets:checked", deadline)?;
    let dir_eval_id = b2
        .evaluate_script("window.__dirCheck".into(), JsFrameScope::MainFrame, 5000)
        .map_err(|e| format!("eval __dirCheck: {e:?}"))?;
    match wait_eval(
        rt,
        mode,
        b2,
        tick,
        dir_eval_id,
        &mut evals2,
        &mut msgs2,
        deadline,
        "dir_check",
    )? {
        EvalOutcome::Value(json) => {
            let v: serde_json::Value =
                serde_json::from_str(&json).map_err(|e| format!("parse __dirCheck: {e}"))?;
            report.dir_file_ok = v["okStatus"] == 200 && v["okText"] == "hello-from-confined-dir";
            report.dir_traversal_status = v["travStatus"].as_u64().unwrap_or(0) as u16;
            report.dir_junction_status = v["juncStatus"].as_u64().unwrap_or(0) as u16;
        }
        other => return Err(format!("unexpected dir_check outcome: {other:?}")),
    }

    // 7. Unlisted custom scheme origin navigation (`app://unlisted/nope.html`) must be denied by TrustedContentPolicy.
    let before_nav_denials = rt.diagnostics().policy_denied[PolicyAction::Navigation as usize];
    b1.submit(BrowserCommand::ExecuteJavaScript {
        script: "window.location.href = 'app://unlisted/nope.html';".into(),
        frame_scope: JsFrameScope::MainFrame,
    })
    .map_err(|e| format!("execute unlisted nav: {e:?}"))?;
    let nav_deny_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if rt.diagnostics().policy_denied[PolicyAction::Navigation as usize] > before_nav_denials {
            report.unlisted_scheme_nav_denied = true;
            break;
        }
        if Instant::now() >= nav_deny_deadline {
            return Err("unlisted custom scheme navigation was not denied by policy".into());
        }
        tick(rt, mode)?;
    }

    // 8. Pending EvaluateScript cancelled cleanly by main-frame navigation.
    let nav_eval_id = b1
        .evaluate_script(
            "(() => { const end = Date.now() + 800; while (Date.now() < end) {} return 'late'; })()"
                .into(),
            JsFrameScope::MainFrame,
            5000,
        )
        .map_err(|e| format!("eval before nav: {e:?}"))?;
    let warmup = Instant::now() + Duration::from_millis(30);
    while Instant::now() < warmup {
        tick(rt, mode)?;
    }
    b1.submit(BrowserCommand::Navigate("app://local/page2.html".into()))
        .map_err(|e| format!("navigate page2: {e:?}"))?;
    match wait_eval(
        rt,
        mode,
        b1,
        tick,
        nav_eval_id,
        &mut evals1,
        &mut msgs1,
        deadline,
        "nav_cancel",
    )? {
        EvalOutcome::Err(EvalError::Navigated) => {
            report.eval_nav_cancelled = true;
        }
        other => return Err(format!("expected EvalError::Navigated, got {other:?}")),
    }
    wait_title(rt, mode, b1, tick, "app:page2", deadline)?;

    // 9. Pending EvaluateScript cancelled cleanly by Force close on Browser 1.
    let close_eval_id = b1
        .evaluate_script(
            "(() => { const end = Date.now() + 800; while (Date.now() < end) {} return 'late2'; })()"
                .into(),
            JsFrameScope::MainFrame,
            5000,
        )
        .map_err(|e| format!("eval before force close: {e:?}"))?;
    let warmup2 = Instant::now() + Duration::from_millis(30);
    while Instant::now() < warmup2 {
        tick(rt, mode)?;
    }
    b1.request_close(CloseMode::Force);
    match wait_eval(
        rt,
        mode,
        b1,
        tick,
        close_eval_id,
        &mut evals1,
        &mut msgs1,
        deadline,
        "close_cancel",
    )? {
        EvalOutcome::Err(EvalError::Closed) => {
            report.eval_close_cancelled = true;
        }
        other => return Err(format!("expected EvalError::Closed, got {other:?}")),
    }
    while b1.snapshot().state != SessionState::Closed {
        if Instant::now() >= deadline {
            return Err("browser 1 force close timeout".into());
        }
        tick(rt, mode)?;
    }
    report.pending_evals_after_close = rt.js_bridge_diagnostics().4;

    Ok(report)
}
