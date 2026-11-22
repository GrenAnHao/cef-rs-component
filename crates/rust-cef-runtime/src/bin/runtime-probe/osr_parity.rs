//! Real CEF OSR parity scenario exercising:
//! 1. Outbound drag default-deny vs opt-in `AllowAsync` + redacted `Debug` + non-blocking pump + stale rejection;
//! 2. `IAccessibilityHandler` AXTree capture + redacted `Debug` + clear on `SetAccessibilityState(Disabled)`;
//! 3. Profile-scoped & Frame-scoped `CefURLRequest` (cookie sharing, POST body, auth default-deny vs opt-in + redaction, untrusted redirect denial with 0 disallowed network hits, explicit cancel, and shutdown drain);
//! 4. Multi-touch `BrowserCommand::TouchEvent` dispatch & active-point cancellation on `ResetInput`.
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use rust_cef_core::{
    contracts::{BrowserId, BrowserOptions, CefFeatureState, SessionState, ThreadMode},
    host_capabilities::{
        CookiePriorityKind, CookieRecord, CookieSameSiteKind, HostCapabilityError, RedactedSecret,
    },
    input::{
        BrowserTouchEvent, InputEvent, InputKind, InputResetReason, MouseButton, TouchEventType,
        TouchPointerType,
    },
    outbound_drag::{
        DragOperationMask, DragSessionId, OutboundDragFinish, OutboundDragPolicy, OutboundDragStart,
    },
    ports::{BrowserCommand, BrowserHandle, MouseModifiers, PolicyAction},
    url_request::{UrlRequestError, UrlRequestSpec, UrlRequestTarget},
};
use rust_cef_runtime::{
    CookieMutationStatus, UrlRequestState, UrlRequestTicket, runtime::Runtime,
    test_support::OsrParityReport,
};

type Tick = fn(&mut Runtime, ThreadMode) -> Result<(), String>;

pub struct OsrHttpFixture {
    pub origin: String,
    #[allow(dead_code)]
    pub disallowed_origin: String,
    pub disallowed_hits: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    workers: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
    acceptors: Vec<thread::JoinHandle<()>>,
}

impl OsrHttpFixture {
    pub fn start() -> Result<Self, String> {
        let allowed = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind allowed: {e}"))?;
        let allowed_addr: SocketAddr = allowed
            .local_addr()
            .map_err(|e| format!("allowed local_addr: {e}"))?;
        allowed
            .set_nonblocking(true)
            .map_err(|e| format!("allowed nonblocking: {e}"))?;

        let disallowed =
            TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind disallowed: {e}"))?;
        let disallowed_addr: SocketAddr = disallowed
            .local_addr()
            .map_err(|e| format!("disallowed local_addr: {e}"))?;
        disallowed
            .set_nonblocking(true)
            .map_err(|e| format!("disallowed nonblocking: {e}"))?;

        let origin = format!("http://{allowed_addr}");
        let disallowed_origin = format!("http://{disallowed_addr}");
        let disallowed_hits = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let workers: Arc<Mutex<Vec<thread::JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));

        let stop_a = stop.clone();
        let workers_a = workers.clone();
        let dis_origin_a = disallowed_origin.clone();
        let accept_allowed = thread::spawn(move || {
            while !stop_a.load(Ordering::Acquire) {
                match allowed.accept() {
                    Ok((stream, _)) => {
                        let s = stop_a.clone();
                        let d = dis_origin_a.clone();
                        let h = thread::spawn(move || handle_client(stream, s, &d));
                        workers_a.lock().unwrap().push(h);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let stop_d = stop.clone();
        let hits_d = disallowed_hits.clone();
        let accept_disallowed = thread::spawn(move || {
            while !stop_d.load(Ordering::Acquire) {
                match disallowed.accept() {
                    Ok((mut stream, _)) => {
                        hits_d.fetch_add(1, Ordering::AcqRel);
                        write_http(&mut stream, "200 OK", "text/plain", &[], b"DISALLOWED_HIT");
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            origin,
            disallowed_origin,
            disallowed_hits,
            stop,
            workers,
            acceptors: vec![accept_allowed, accept_disallowed],
        })
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.origin, path)
    }
}

impl Drop for OsrHttpFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for h in std::mem::take(&mut self.acceptors) {
            let _ = h.join();
        }
        let handles = std::mem::take(&mut *self.workers.lock().unwrap());
        for h in handles {
            let _ = h.join();
        }
    }
}

fn handle_client(mut stream: TcpStream, stop: Arc<AtomicBool>, disallowed_origin: &str) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .unwrap_or_else(|_| panic!("clone stream")),
    );
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let mut auth_header = String::new();
    let mut cookie_header = String::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if line == "\r\n" || line == "\n" {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if lower.starts_with("authorization:") {
                    auth_header = line.trim().to_string();
                } else if lower.starts_with("cookie:") {
                    cookie_header = line.trim().to_string();
                } else if let Some(rest) = lower.strip_prefix("content-length:") {
                    content_length = rest.trim().parse::<usize>().unwrap_or(0).min(1024 * 1024);
                }
            }
        }
    }
    let mut body_bytes = vec![0u8; content_length];
    if content_length > 0 && reader.read_exact(&mut body_bytes).is_err() {
        return;
    }

    match path.as_str() {
        "/default-deny" => {
            let html = concat!(
                "<!doctype html><title>deny:boot</title>",
                "<body style='margin:0;width:100vw;height:100vh;background:#1a4c7e'>",
                "<div id='drag' draggable='true' style='position:absolute;left:10px;top:10px;width:120px;height:40px;background:#aac'>Drag</div>",
                "<script>",
                "document.getElementById('drag').addEventListener('dragstart', e => {",
                "e.dataTransfer.setData('text/plain', 'default-deny-drag');",
                "document.title = 'deny-start:1';",
                "});",
                "</script></body>"
            );
            write_http(&mut stream, "200 OK", "text/html", &[], html.as_bytes());
        }
        "/parity" => {
            let html = concat!(
                "<!doctype html><title>parity:ready</title>",
                "<body style='margin:0;width:100vw;height:100vh;background:#1a4c7e'>",
                "<div id='drag' draggable='true' style='position:absolute;left:10px;top:10px;width:120px;height:40px;background:#aac'>Drag</div>",
                "<h1 style='position:absolute;left:10px;top:54px;margin:0;font-size:12px;color:#fff'>OSR Parity</h1>",
                "<button id='recover' style='position:absolute;left:10px;top:185px;width:220px;height:36px'>OSR-PARITY-SECRET-BUTTON</button>",
                "<script>",
                "window.__ts=0;window.__tm=0;window.__te=0;window.__tc=0;",
                "window.__ps=0;window.__pm=0;window.__pe=0;window.__pc=0;",
                "function updateTouch(){document.title='touch:'+__ts+':'+__tm+':'+__te+':'+__tc+':'+__ps+':'+__pm+':'+__pe+':'+__pc;}",
                "document.addEventListener('touchstart',()=>{window.__ts++;updateTouch();},{passive:true});",
                "document.addEventListener('touchmove',()=>{window.__tm++;updateTouch();},{passive:true});",
                "document.addEventListener('touchend',()=>{window.__te++;updateTouch();},{passive:true});",
                "document.addEventListener('touchcancel',()=>{window.__tc++;updateTouch();},{passive:true});",
                "document.addEventListener('pointerdown',e=>{if(e.pointerType==='touch'){window.__ps++;updateTouch();}});",
                "document.addEventListener('pointermove',e=>{if(e.pointerType==='touch'){window.__pm++;updateTouch();}});",
                "document.addEventListener('pointerup',e=>{if(e.pointerType==='touch'){window.__pe++;updateTouch();}});",
                "document.addEventListener('pointercancel',e=>{if(e.pointerType==='touch'){window.__pc++;updateTouch();}});",
                "document.getElementById('drag').addEventListener('dragstart', e => {",
                "e.dataTransfer.setData('text/plain', 'osr-parity-drag');",
                "document.title = 'optin-start:1';",
                "});",
                "document.getElementById('recover').addEventListener('click', () => {",
                "document.title = 'drag:recovered';",
                "});",
                "</script></body>"
            );
            write_http(&mut stream, "200 OK", "text/html", &[], html.as_bytes());
        }
        "/cookie-check" => {
            write_http(
                &mut stream,
                "200 OK",
                "text/plain",
                &[],
                cookie_header.as_bytes(),
            );
        }
        "/echo-post" => {
            let body_text = String::from_utf8_lossy(&body_bytes);
            let resp = format!("cookie={cookie_header}|body={body_text}");
            write_http(&mut stream, "200 OK", "text/plain", &[], resp.as_bytes());
        }
        "/auth-check" => {
            // base64("alice:parity-secret") == "YWxpY2U6cGFyaXR5LXNlY3JldA=="
            if auth_header.contains("Basic YWxpY2U6cGFyaXR5LXNlY3JldA==") {
                write_http(&mut stream, "200 OK", "text/plain", &[], b"auth-ok");
            } else {
                write_http(
                    &mut stream,
                    "401 Unauthorized",
                    "text/plain",
                    &[("WWW-Authenticate", "Basic realm=\"OsrParity\"")],
                    b"Unauthorized",
                );
            }
        }
        "/redirect-evil" => {
            let target = format!("{disallowed_origin}/leak");
            write_http(
                &mut stream,
                "302 Found",
                "text/plain",
                &[("Location", target.as_str())],
                b"",
            );
        }
        "/slow-stream" => {
            let header = "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 4194304\r\nConnection: close\r\n\r\n";
            if stream.write_all(header.as_bytes()).is_err() {
                return;
            }
            if stream.write_all(&[0x55u8; 4096]).is_err() {
                return;
            }
            let _ = stream.flush();
            let started = Instant::now();
            while !stop.load(Ordering::Acquire) && started.elapsed() < Duration::from_secs(20) {
                if stream.write_all(&[0x55u8; 256]).is_err() {
                    break;
                }
                let _ = stream.flush();
                thread::sleep(Duration::from_millis(25));
            }
        }
        _ => {
            write_http(
                &mut stream,
                "404 Not Found",
                "text/plain",
                &[],
                b"not found",
            );
        }
    }
}

fn write_http(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    extra_headers: &[(&str, &str)],
    body: &[u8],
) {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in extra_headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    if !body.is_empty() {
        let _ = stream.write_all(body);
    }
    let _ = stream.flush();
}

pub fn initial_url(fixture: &OsrHttpFixture, index: u64) -> String {
    if index == 0 {
        fixture.url("/default-deny")
    } else {
        fixture.url("/parity")
    }
}

pub fn browser_options(index: u64) -> BrowserOptions {
    if index == 0 {
        BrowserOptions::default()
    } else {
        BrowserOptions::default()
            .with_outbound_drag_policy(OutboundDragPolicy::AllowAsync)
            .with_accessibility_state(CefFeatureState::Enabled)
    }
}

fn wait_until(
    rt: &mut Runtime,
    mode: ThreadMode,
    tick: Tick,
    deadline: Instant,
    label: &str,
    mut pred: impl FnMut(&mut Runtime) -> Result<bool, String>,
) -> Result<(), String> {
    loop {
        if pred(rt)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("osr_parity timeout waiting for {label}"));
        }
        tick(rt, mode)?;
    }
}

fn send_input(handle: &BrowserHandle, kind: InputKind) -> Result<(), String> {
    let epoch = handle
        .input_snapshot()
        .map_err(|e| format!("input epoch: {e:?}"))?
        .epoch;
    handle
        .submit(BrowserCommand::Input(InputEvent { epoch, kind }))
        .map_err(|e| format!("submit input: {e:?}"))?;
    Ok(())
}

fn parse_touch_title(title: &str) -> Option<[u64; 8]> {
    let rest = title.strip_prefix("touch:")?;
    let mut vals = [0u64; 8];
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() != 8 {
        return None;
    }
    for (i, part) in parts.iter().enumerate() {
        vals[i] = part.parse::<u64>().ok()?;
    }
    Some(vals)
}

pub fn run(
    rt: &mut Runtime,
    mode: ThreadMode,
    handles: &[BrowserHandle],
    tick: Tick,
    fixture: &OsrHttpFixture,
    outbound_drags: &Arc<Mutex<Vec<(BrowserId, OutboundDragStart)>>>,
    policy_denials: &Arc<Mutex<Vec<PolicyAction>>>,
) -> Result<(OsrParityReport, UrlRequestTicket), String> {
    let mut report = OsrParityReport::default();
    let deadline = Instant::now() + Duration::from_secs(28);
    let b0 = handles.first().ok_or("missing browser 0")?;
    let b1 = handles.get(1).ok_or("missing browser 1")?;
    let b0_id = b0.snapshot().browser;
    let b1_id = b1.snapshot().browser;

    wait_until(rt, mode, tick, deadline, "initial pages loaded", |_| {
        let s0 = b0.snapshot();
        let s1 = b1.snapshot();
        Ok(s0.state == SessionState::Alive
            && !s0.navigation.is_loading
            && s0.navigation.title == "deny:boot"
            && s1.state == SessionState::Alive
            && !s1.navigation.is_loading
            && s1.navigation.title == "parity:ready")
    })?;

    // 1. Outbound drag default-deny on Browser 0.
    let pointer = |x, y| InputKind::MouseMove {
        x,
        y,
        leaving: false,
        modifiers: MouseModifiers::default(),
    };
    let button = |x, y, up| InputKind::MouseButton {
        x,
        y,
        button: MouseButton::Left,
        up,
        click_count: 1,
        modifiers: MouseModifiers::default(),
    };
    send_input(b0, pointer(40, 25))?;
    send_input(b0, button(40, 25, false))?;
    send_input(b0, pointer(110, 25))?;
    wait_until(
        rt,
        mode,
        tick,
        deadline,
        "b0 default-deny StartDragging",
        |rt| {
            Ok(b0.snapshot().navigation.title == "deny-start:1"
                && rt
                    .probe_frame_state(b0_id)
                    .and_then(|s| s.outbound_drag_denial)
                    .is_some_and(|d| d.callbacks == 1))
        },
    )?;
    send_input(b0, button(110, 25, true))?;
    report.default_deny_outbound_drag_refused = outbound_drags
        .lock()
        .unwrap()
        .iter()
        .all(|(id, _)| *id != b0_id);

    // 2. Outbound drag opt-in on Browser 1 (AllowAsync + redacted Debug + pump without deadlock + FinishOutboundDrag + stale rejection + recovery click).
    send_input(b1, pointer(40, 25))?;
    send_input(b1, button(40, 25, false))?;
    send_input(b1, pointer(110, 25))?;
    wait_until(
        rt,
        mode,
        tick,
        deadline,
        "b1 opt-in StartOutboundDrag",
        |_| {
            Ok(b1.snapshot().navigation.title == "optin-start:1"
                && outbound_drags
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(id, _)| *id == b1_id))
        },
    )?;
    let drag_req = outbound_drags
        .lock()
        .unwrap()
        .iter()
        .find(|(id, _)| *id == b1_id)
        .map(|(_, req)| req.clone())
        .ok_or("missing b1 outbound drag event")?;
    report.optin_outbound_drag_started = drag_req.payload.text.as_deref()
        == Some("osr-parity-drag")
        && drag_req.allowed_ops.bits() != 0;
    let drag_dbg = format!("{drag_req:?}");
    report.optin_outbound_drag_payload_redacted = !drag_dbg.contains("osr-parity-drag");

    // Pump for 30ms before finishing drag to verify non-blocking pump in Integrated mode.
    for _ in 0..5 {
        tick(rt, mode)?;
        std::thread::sleep(Duration::from_millis(6));
    }

    let finish_ok = OutboundDragFinish {
        drag_id: drag_req.drag_id,
        x: 110,
        y: 25,
        operation: DragOperationMask::COPY,
    };
    // Wrong browser must reject synchronously before queuing.
    let wrong_browser_err = rt.finish_outbound_drag(b0_id, finish_ok).is_err();
    // Stale/wrong drag sequence posted while drag 1 is still active must not complete or corrupt drag 1.
    let stale_while_active_posted = rt
        .finish_outbound_drag(
            b1_id,
            OutboundDragFinish {
                drag_id: DragSessionId {
                    browser: b1_id,
                    sequence: drag_req.drag_id.sequence + 99,
                },
                x: 0,
                y: 0,
                operation: DragOperationMask::NONE,
            },
        )
        .is_ok();
    wait_until(rt, mode, tick, deadline, "stale-while-active drain", |rt| {
        Ok(rt.diagnostics().tasks.pending == 0)
    })?;

    rt.finish_outbound_drag(b1_id, finish_ok)
        .map_err(|e| format!("finish_outbound_drag b1: {e:?}"))?;
    wait_until(
        rt,
        mode,
        tick,
        deadline,
        "finish_outbound_drag drain",
        |rt| Ok(rt.diagnostics().tasks.pending == 0),
    )?;
    // Allow Blink renderer to process DragSourceEndedAt/DragSourceSystemDragEnded before recovery click in Integrated mode.
    for _ in 0..10 {
        tick(rt, mode)?;
        std::thread::sleep(Duration::from_millis(5));
    }
    send_input(b1, button(110, 25, true))?;
    send_input(b1, pointer(50, 200))?;
    for _ in 0..4 {
        tick(rt, mode)?;
        std::thread::sleep(Duration::from_millis(5));
    }
    send_input(b1, button(50, 200, false))?;
    send_input(b1, button(50, 200, true))?;
    wait_until(rt, mode, tick, deadline, "b1 drag recovery click", |_| {
        Ok(b1.snapshot().navigation.title == "drag:recovered")
    })?;
    report.optin_outbound_drag_finished_no_deadlock = true;

    // Duplicate/replayed completion after drag 1 retired must also be harmless on the CEF UI thread.
    let stale_after_cmd_ok = rt
        .finish_outbound_drag(
            b1_id,
            OutboundDragFinish {
                drag_id: DragSessionId {
                    browser: b1_id,
                    sequence: drag_req.drag_id.sequence,
                },
                x: 0,
                y: 0,
                operation: DragOperationMask::NONE,
            },
        )
        .is_ok();
    wait_until(rt, mode, tick, deadline, "stale replay task drain", |rt| {
        Ok(rt.diagnostics().tasks.pending == 0)
    })?;
    report.optin_outbound_drag_stale_rejected =
        wrong_browser_err && stale_while_active_posted && stale_after_cmd_ok;

    // 3. IAccessibilityHandler AXTree capture, redacted Debug, and clear on Disable.
    let ax_slot = rt
        .ax_tree_slot(b1_id)
        .map_err(|e| format!("ax_tree_slot: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "b1 AXTree populated", |_| {
        let Some(snap) = ax_slot.snapshot() else {
            return Ok(false);
        };
        Ok(snap.generation > 0
            && snap.root_id.is_some()
            && snap.nodes.len() >= 2
            && snap.nodes.values().any(|n| {
                n.name.contains("OSR-PARITY-SECRET-BUTTON")
                    || n.value.contains("OSR-PARITY-SECRET-BUTTON")
                    || n.name.contains("OSR Parity")
            }))
    })?;
    let ax_snap = ax_slot
        .snapshot()
        .ok_or("AXTree snapshot disappeared before read")?;
    report.a11y_node_count = ax_snap.nodes.len();
    report.a11y_tree_captured = ax_snap.root_id.is_some() && report.a11y_node_count >= 2;
    let ax_dbg = format!("{ax_snap:?}");
    report.a11y_debug_redacted =
        !ax_dbg.contains("OSR-PARITY-SECRET-BUTTON") && !ax_dbg.contains("OSR Parity");

    b1.submit(BrowserCommand::SetAccessibilityState(
        CefFeatureState::Disabled,
    ))
    .map_err(|e| format!("disable a11y: {e:?}"))?;
    wait_until(
        rt,
        mode,
        tick,
        deadline,
        "AXTree cleared on disable",
        |_| {
            Ok(ax_slot
                .snapshot()
                .is_none_or(|snap| snap.nodes.is_empty() && snap.root_id.is_none()))
        },
    )?;
    report.a11y_cleared_on_disable = true;

    // 4. Profile & Frame CefURLRequest: cookie sharing, POST, auth default-deny/opt-in, untrusted redirect denial, cancel, shutdown drain.
    let profile = rt.default_profile_handle();
    let cookie = CookieRecord {
        name: "osr_sess".into(),
        value: RedactedSecret::new("parity42"),
        domain: String::new(),
        path: "/".into(),
        secure: false,
        http_only: false,
        same_site: CookieSameSiteKind::Unspecified,
        priority: CookiePriorityKind::Medium,
    };
    let set_cookie = rt
        .set_profile_cookie(profile, fixture.url("/"), cookie)
        .map_err(|e| format!("set_profile_cookie: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "profile cookie set", |_| {
        Ok(!matches!(set_cookie.poll(), CookieMutationStatus::Pending))
    })?;
    if !matches!(set_cookie.poll(), CookieMutationStatus::Succeeded { .. }) {
        return Err("set_profile_cookie failed".into());
    }

    // 4a. Profile-scoped GET /cookie-check
    let profile_get = rt
        .start_url_request(UrlRequestSpec::get(
            UrlRequestTarget::DefaultProfile,
            fixture.url("/cookie-check"),
        ))
        .map_err(|e| format!("start profile url request: {e:?}"))?;
    wait_until(
        rt,
        mode,
        tick,
        deadline,
        "profile GET /cookie-check",
        |_| Ok(!matches!(profile_get.poll(), UrlRequestState::Pending(_))),
    )?;
    report.url_request_profile_cookie_ok = matches!(
        profile_get.poll(),
        UrlRequestState::Completed(resp)
            if resp.http_status == Some(200)
                && String::from_utf8_lossy(&resp.body).contains("osr_sess=parity42")
    );

    // 4b. Frame-scoped POST /echo-post + sensitive Debug redaction
    let mut post_spec = UrlRequestSpec::post(
        UrlRequestTarget::BrowserMainFrame(b1_id),
        fixture.url("/echo-post"),
        b"hello-frame-post".to_vec(),
        "text/plain",
    );
    post_spec
        .headers
        .push(("Authorization".into(), "Bearer top-secret-token".into()));
    let spec_dbg = format!("{post_spec:?}");
    let frame_post = rt
        .start_url_request(post_spec)
        .map_err(|e| format!("start frame POST: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "frame POST /echo-post", |_| {
        Ok(!matches!(frame_post.poll(), UrlRequestState::Pending(_)))
    })?;
    let post_state = frame_post.poll();
    let resp_dbg = format!("{post_state:?}");
    report.url_request_debug_redacted = !spec_dbg.contains("top-secret-token")
        && !spec_dbg.contains("hello-frame-post")
        && !resp_dbg.contains("top-secret-token");
    report.url_request_frame_post_ok = matches!(
        post_state,
        UrlRequestState::Completed(resp)
            if resp.http_status == Some(200)
                && String::from_utf8_lossy(&resp.body).contains("osr_sess=parity42")
                && String::from_utf8_lossy(&resp.body).contains("body=hello-frame-post")
    );

    // 4c. Auth default-deny vs opt-in resolver
    rt.set_url_request_auth_resolver(None);
    let auth_deny = rt
        .start_url_request(UrlRequestSpec::get(
            UrlRequestTarget::DefaultProfile,
            fixture.url("/auth-check"),
        ))
        .map_err(|e| format!("start auth default deny: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "auth default-deny", |_| {
        Ok(!matches!(auth_deny.poll(), UrlRequestState::Pending(_)))
    })?;
    report.url_request_auth_default_denied = match auth_deny.poll() {
        UrlRequestState::Completed(resp) => {
            resp.http_status == Some(401) && resp.body != b"auth-ok"
        }
        UrlRequestState::Failed(_) => true,
        UrlRequestState::Pending(_) => false,
    };

    rt.set_url_request_auth_resolver(Some(Arc::new(|_| {
        Some(("alice".into(), RedactedSecret::new("parity-secret")))
    })));
    let auth_optin = rt
        .start_url_request(UrlRequestSpec::get(
            UrlRequestTarget::DefaultProfile,
            fixture.url("/auth-check"),
        ))
        .map_err(|e| format!("start auth opt-in: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "auth opt-in", |_| {
        Ok(!matches!(auth_optin.poll(), UrlRequestState::Pending(_)))
    })?;
    rt.set_url_request_auth_resolver(None);
    report.url_request_auth_optin_ok = matches!(
        auth_optin.poll(),
        UrlRequestState::Completed(resp)
            if resp.http_status == Some(200) && resp.body == b"auth-ok"
    );

    // Clear auth credentials from profile cache afterward as good hygiene.
    let _ = rt.clear_profile_http_auth_credentials(profile);
    let _ = HostCapabilityError::CapabilityDisabled;

    // 4d. Untrusted redirect denial (must block before hitting disallowed listener)
    let policy_count = |rt: &Runtime| {
        let d = rt.diagnostics();
        d.policy_denied[PolicyAction::Resource as usize]
            + d.policy_denied[PolicyAction::Redirect as usize]
            + d.policy_denied[PolicyAction::Navigation as usize]
            + (policy_denials.lock().unwrap().len() as u64)
    };
    let denials_before = policy_count(rt);
    let redir = rt
        .start_url_request(UrlRequestSpec::get(
            UrlRequestTarget::DefaultProfile,
            fixture.url("/redirect-evil"),
        ))
        .map_err(|e| format!("start redirect-evil: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "redirect-evil denial", |_| {
        Ok(!matches!(redir.poll(), UrlRequestState::Pending(_)))
    })?;
    let denials_after = policy_count(rt);
    report.url_request_disallowed_hits = fixture.disallowed_hits.load(Ordering::Acquire);
    report.url_request_untrusted_redirect_denied = matches!(
        redir.poll(),
        UrlRequestState::Failed(UrlRequestError::PolicyDenied | UrlRequestError::Failed { .. })
    ) && report.url_request_disallowed_hits == 0
        && denials_after > denials_before;

    // 4e. Explicit cancel of streaming request
    let cancel_ticket = rt
        .start_url_request(UrlRequestSpec::get(
            UrlRequestTarget::DefaultProfile,
            fixture.url("/slow-stream"),
        ))
        .map_err(|e| format!("start slow-stream cancel: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "slow-stream bytes > 0", |_| {
        Ok(matches!(
            cancel_ticket.poll(),
            UrlRequestState::Pending(p) if p.downloaded_bytes > 0
        ))
    })?;
    if !cancel_ticket.cancel() {
        return Err("cancel_ticket.cancel() returned false".into());
    }
    wait_until(rt, mode, tick, deadline, "slow-stream cancelled", |_| {
        Ok(!matches!(cancel_ticket.poll(), UrlRequestState::Pending(_)))
    })?;
    report.url_request_explicit_cancel_ok = matches!(
        cancel_ticket.poll(),
        UrlRequestState::Failed(UrlRequestError::Cancelled)
    );

    // 4f. Start a second slow-stream to leave active across request_shutdown/finish_shutdown drain.
    let shutdown_ticket = rt
        .start_url_request(UrlRequestSpec::get(
            UrlRequestTarget::DefaultProfile,
            fixture.url("/slow-stream"),
        ))
        .map_err(|e| format!("start slow-stream shutdown: {e:?}"))?;
    wait_until(
        rt,
        mode,
        tick,
        deadline,
        "shutdown slow-stream active bytes > 0",
        |_| {
            Ok(matches!(
                shutdown_ticket.poll(),
                UrlRequestState::Pending(p) if p.downloaded_bytes > 0
            ))
        },
    )?;

    // 5. Multi-touch SendTouchEvent dispatch & active-point cancellation on ResetInput.
    let touch = |id: i32, x: f32, y: f32, event_type: TouchEventType| {
        BrowserCommand::TouchEvent(BrowserTouchEvent {
            id,
            x,
            y,
            radius_x: 12.0,
            radius_y: 12.0,
            rotation_angle: 0.0,
            pressure: if matches!(
                event_type,
                TouchEventType::Released | TouchEventType::Cancelled
            ) {
                0.0
            } else {
                0.5
            },
            event_type,
            modifiers: MouseModifiers::default(),
            pointer_type: TouchPointerType::Touch,
        })
    };
    b1.submit(touch(1, 200.0, 120.0, TouchEventType::Pressed))
        .map_err(|e| format!("touch pressed 1: {e:?}"))?;
    b1.submit(touch(1, 210.0, 125.0, TouchEventType::Moved))
        .map_err(|e| format!("touch moved 1: {e:?}"))?;
    b1.submit(touch(1, 210.0, 125.0, TouchEventType::Released))
        .map_err(|e| format!("touch released 1: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "b1 touch events title", |_| {
        Ok(
            parse_touch_title(&b1.snapshot().navigation.title).is_some_and(|v| {
                (v[0] >= 1 || v[4] >= 1) && (v[1] >= 1 || v[5] >= 1) && (v[2] >= 1 || v[6] >= 1)
            }),
        )
    })?;
    let baseline_presses = parse_touch_title(&b1.snapshot().navigation.title)
        .map(|v| (v[0], v[4]))
        .unwrap_or((0, 0));
    report.touch_events_dispatched_ok = true;

    b1.submit(touch(2, 200.0, 120.0, TouchEventType::Pressed))
        .map_err(|e| format!("touch pressed 2: {e:?}"))?;
    wait_until(
        rt,
        mode,
        tick,
        deadline,
        "b1 touch pressed 2 observed",
        |_| {
            Ok(parse_touch_title(&b1.snapshot().navigation.title)
                .is_some_and(|v| v[0] > baseline_presses.0 || v[4] > baseline_presses.1))
        },
    )?;
    b1.submit(BrowserCommand::ResetInput(InputResetReason::FocusLost))
        .map_err(|e| format!("ResetInput touch cancel: {e:?}"))?;
    wait_until(
        rt,
        mode,
        tick,
        deadline,
        "b1 active touch cancelled",
        |_| {
            Ok(parse_touch_title(&b1.snapshot().navigation.title)
                .is_some_and(|v| v[3] >= 1 || v[7] >= 1))
        },
    )?;
    report.touch_active_cancelled_on_reset = true;

    Ok((report, shutdown_ticket))
}
