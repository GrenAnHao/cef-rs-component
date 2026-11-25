//! Real CEF integration scenario exercising default-deny regression, opt-in
//! host capabilities (download + force-close cancel, popup handoff, JS dialog,
//! HTTP 401 Basic Auth with redaction), profile cookies + access filter +
//! guarded preferences, PrintToPdf, and headless CDP clean Registration teardown.
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use rust_cef_core::{
    contracts::{BrowserOptions, SessionState, ThreadMode},
    host_capabilities::{
        AuthCredentials, AuthDecision, AuthRequest, AuthService, CdpEvent, CdpMethodResult,
        CdpObserver, CookieAccessPolicy, CookieFilterDecision, CookieFilterRequest,
        CookiePriorityKind, CookieRecord, CookieSameSiteKind, DownloadProgress, DownloadRequest,
        DownloadService, DownloadState, HostCapabilities, HostCapabilityError, JsDialogDecision,
        JsDialogRequest, JsDialogService, PdfExportOptions, PopupRequest, PopupService,
        PreferenceValue, RedactedSecret,
    },
    ports::{
        BrowserCommand, BrowserHandle, CloseMode, MouseClick, MouseModifiers, PixelFormat,
        PolicyAction, SurfaceKind,
    },
};
use rust_cef_runtime::{
    CookieMutationStatus, CookieVisitStatus, PdfExportStatus, ProfileActionStatus,
    runtime::Runtime, test_support::HostCapabilitiesReport,
};

type Tick = fn(&mut Runtime, ThreadMode) -> Result<(), String>;

const BGRA_1A4C7E: [u8; 4] = [0x7e, 0x4c, 0x1a, 0xff];
const BGRA_2A5C8E: [u8; 4] = [0x8e, 0x5c, 0x2a, 0xff];

pub struct HttpFixture {
    pub origin: String,
    stop: Arc<AtomicBool>,
    workers: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
    acceptor: Option<thread::JoinHandle<()>>,
}

impl HttpFixture {
    pub fn start() -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind: {e}"))?;
        let addr: SocketAddr = listener
            .local_addr()
            .map_err(|e| format!("local_addr: {e}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("nonblocking: {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let workers: Arc<Mutex<Vec<thread::JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let stop_c = stop.clone();
        let workers_c = workers.clone();
        let acceptor = thread::spawn(move || {
            while !stop_c.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let s = stop_c.clone();
                        let h = thread::spawn(move || handle_client(stream, s));
                        workers_c.lock().unwrap().push(h);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            origin: format!("http://{addr}"),
            stop,
            workers,
            acceptor: Some(acceptor),
        })
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.origin, path)
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.acceptor.take() {
            let _ = h.join();
        }
        let handles = std::mem::take(&mut *self.workers.lock().unwrap());
        for h in handles {
            let _ = h.join();
        }
    }
}

fn handle_client(mut stream: TcpStream, stop: Arc<AtomicBool>) {
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
                }
            }
        }
    }
    match path.as_str() {
        "/boot" => {
            let body = br#"<!doctype html><title>boot</title><body style="margin:0;background:#1a4c7e"></body>"#;
            write_http(&mut stream, "200 OK", "text/html", &[], body);
        }
        "/default-deny" => {
            let body = br#"<!doctype html><body style="margin:0;width:100vw;height:100vh;background:#1a4c7e" onclick="window.open('/popup-target','_blank','width=200,height=200'); const a=document.createElement('a'); a.href='/download.bin'; a.download='sample.bin'; a.click(); document.title='deny:clicked';"><script>alert('deny-me'); document.title='deny:ready';</script></body>"#;
            write_http(&mut stream, "200 OK", "text/html", &[], body);
        }
        "/opt-in" => {
            let body = br#"<!doctype html><body style="margin:0;width:100vw;height:100vh;background:#2a5c8e" onclick="window.open('/popup-target','_blank','width=200,height=200'); document.title='optin:popup';"><script>document.title='optin:ready';</script></body>"#;
            write_http(&mut stream, "200 OK", "text/html", &[], body);
        }
        "/popup-target" => {
            let body = br#"<!doctype html><body style="margin:0;background:#1a4c7e"><title>popup:target</title></body>"#;
            write_http(&mut stream, "200 OK", "text/html", &[], body);
        }
        "/download.bin" => {
            let body = b"HOST-CAP-DOWNLOAD-OK-0123456789";
            write_http(
                &mut stream,
                "200 OK",
                "application/octet-stream",
                &[("Content-Disposition", "attachment; filename=\"sample.bin\"")],
                body,
            );
        }
        "/slow-download.bin" => {
            let header = "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"slow.bin\"\r\nContent-Length: 4194304\r\nConnection: close\r\n\r\n";
            if stream.write_all(header.as_bytes()).is_err() {
                return;
            }
            let chunk = vec![0x55u8; 4096];
            if stream.write_all(&chunk).is_err() {
                return;
            }
            let _ = stream.flush();
            let started = Instant::now();
            while !stop.load(Ordering::Acquire) && started.elapsed() < Duration::from_secs(15) {
                if stream.write_all(&[0x55u8; 256]).is_err() {
                    break;
                }
                let _ = stream.flush();
                thread::sleep(Duration::from_millis(25));
            }
        }
        "/auth" => {
            // base64("alice:super-secret-pw") == "YWxpY2U6c3VwZXItc2VjcmV0LXB3"
            if auth_header.contains("Basic YWxpY2U6c3VwZXItc2VjcmV0LXB3") {
                let body = br#"<!doctype html><body style="margin:0;background:#1a4c7e"><script>document.title='auth:ok';</script></body>"#;
                write_http(&mut stream, "200 OK", "text/html", &[], body);
            } else {
                write_http(
                    &mut stream,
                    "401 Unauthorized",
                    "text/plain",
                    &[("WWW-Authenticate", "Basic realm=\"ProbeAuth\"")],
                    b"Unauthorized",
                );
            }
        }
        "/cookie-echo" => {
            let body = format!(
                "{{\"cookie_header\":\"{}\"}}",
                cookie_header.replace('"', "")
            );
            write_http(
                &mut stream,
                "200 OK",
                "application/json",
                &[
                    ("Set-Cookie", "blocked_cookie=evil; Path=/"),
                    ("Set-Cookie", "saved_cookie=ok; Path=/"),
                ],
                body.as_bytes(),
            );
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
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

#[derive(Default)]
struct ProbeCapsState {
    downloads: Mutex<Vec<DownloadRequest>>,
    download_progress: Mutex<Vec<DownloadProgress>>,
    js_dialogs: Mutex<Vec<JsDialogRequest>>,
    popups: Mutex<Vec<PopupRequest>>,
    auths: Mutex<Vec<AuthRequest>>,
    cdp_results: Mutex<Vec<CdpMethodResult>>,
    cdp_events: Mutex<Vec<CdpEvent>>,
    cookie_send_hits: AtomicU64,
    cookie_save_hits: AtomicU64,
}

struct ProbeCapsAdapter(Arc<ProbeCapsState>);

impl DownloadService for ProbeCapsAdapter {
    fn on_before_download(&self, request: DownloadRequest) {
        self.0.downloads.lock().unwrap().push(request);
    }

    fn on_download_updated(&self, progress: DownloadProgress) {
        self.0.download_progress.lock().unwrap().push(progress);
    }
}

impl JsDialogService for ProbeCapsAdapter {
    fn on_js_dialog(&self, request: JsDialogRequest) {
        self.0.js_dialogs.lock().unwrap().push(request);
    }
}

impl PopupService for ProbeCapsAdapter {
    fn on_popup_request(&self, request: PopupRequest) {
        self.0.popups.lock().unwrap().push(request);
    }
}

impl AuthService for ProbeCapsAdapter {
    fn on_auth_credentials(&self, request: AuthRequest) {
        self.0.auths.lock().unwrap().push(request);
    }
}

impl CookieAccessPolicy for ProbeCapsAdapter {
    fn can_send_cookie(&self, request: &CookieFilterRequest) -> CookieFilterDecision {
        self.0.cookie_send_hits.fetch_add(1, Ordering::AcqRel);
        if request.cookie_name == "blocked_cookie" {
            CookieFilterDecision::Block
        } else {
            CookieFilterDecision::Allow
        }
    }

    fn can_save_cookie(&self, request: &CookieFilterRequest) -> CookieFilterDecision {
        self.0.cookie_save_hits.fetch_add(1, Ordering::AcqRel);
        if request.cookie_name == "blocked_cookie" {
            CookieFilterDecision::Block
        } else {
            CookieFilterDecision::Allow
        }
    }
}

impl CdpObserver for ProbeCapsAdapter {
    fn on_method_result(&self, result: CdpMethodResult) {
        self.0.cdp_results.lock().unwrap().push(result);
    }

    fn on_event(&self, event: CdpEvent) {
        self.0.cdp_events.lock().unwrap().push(event);
    }
}

pub fn initial_url(fixture: &HttpFixture, index: u64) -> String {
    if index == 0 {
        fixture.url("/default-deny")
    } else {
        fixture.url("/boot")
    }
}

pub fn browser_options(index: u64) -> BrowserOptions {
    BrowserOptions::default().with_headless_cdp(index == 1)
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
            return Err(format!("host_capabilities timeout waiting for {label}"));
        }
        tick(rt, mode)?;
    }
}

fn send_click(handle: &BrowserHandle, x: i32, y: i32) -> Result<(), String> {
    handle
        .submit(BrowserCommand::MouseClick(MouseClick {
            x,
            y,
            modifiers: MouseModifiers::default(),
            click_count: std::num::NonZeroU8::MIN,
        }))
        .map_err(|e| format!("click submit: {e:?}"))?;
    Ok(())
}

pub fn run(
    rt: &mut Runtime,
    mode: ThreadMode,
    handles: &[BrowserHandle],
    tick: Tick,
    fixture: &HttpFixture,
    cache_root: &Path,
    policy_denials: &Arc<Mutex<Vec<PolicyAction>>>,
) -> Result<HostCapabilitiesReport, String> {
    let mut report = HostCapabilitiesReport::default();
    let deadline = Instant::now() + Duration::from_secs(28);
    let b0 = handles.first().ok_or("missing browser 0")?;
    let b1 = handles.get(1).ok_or("missing browser 1")?;
    let b0_id = b0.snapshot().browser;
    let b1_id = b1.snapshot().browser;

    // 1. Default-deny verification on Browser 0 (headless_cdp_enabled=false, HostCapabilities unset).
    wait_until(rt, mode, tick, deadline, "b0 title", |_| {
        Ok(b0.snapshot().navigation.title == "deny:ready")
    })?;

    // Check pixel #1a4c7e at (160, 120) on real frame output.
    let src0 = rt
        .frame_source(b0_id)
        .map_err(|e| format!("frame_source b0: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "b0 pixel #1a4c7e", |_| {
        let Some(frame) = src0.latest(SurfaceKind::View) else {
            return Ok(false);
        };
        if frame.info.format != PixelFormat::PremultipliedBgra8
            || frame.info.width < 320
            || frame.info.height < 240
        {
            return Ok(false);
        }
        let off = (120 * frame.info.width as usize + 160) * 4;
        Ok(frame.cpu_pixels().and_then(|px| px.as_slice().get(off..off + 4)) == Some(&BGRA_1A4C7E))
    })?;
    report.pixel_verified = true;

    // Trigger popup + download via real user-gesture click on Browser 0.
    send_click(b0, 160, 120)?;
    wait_until(rt, mode, tick, deadline, "b0 default denials", |_| {
        let denials = policy_denials.lock().unwrap();
        Ok(denials.contains(&PolicyAction::JsDialog)
            && denials.contains(&PolicyAction::Popup)
            && denials.contains(&PolicyAction::Download))
    })?;
    report.default_deny_js_dialog_denied = true;
    report.default_deny_popup_denied = true;
    report.default_deny_download_denied = true;
    report.default_deny_cdp_rejected = matches!(
        rt.execute_cdp_method(b0_id, "Runtime.evaluate", None),
        Err(HostCapabilityError::CapabilityDisabled)
    );

    b0.request_close(CloseMode::Graceful);
    wait_until(rt, mode, tick, deadline, "b0 closed", |_| {
        Ok(b0.snapshot().state == SessionState::Closed)
    })?;

    // 2. Enable opt-in HostCapabilities on Browser 1 (headless_cdp_enabled = true).
    let work_dir = cache_root
        .parent()
        .unwrap_or(cache_root)
        .join(format!("hostcap-out-{}", std::process::id()));
    let _ = fs::remove_dir_all(&work_dir);
    fs::create_dir_all(&work_dir).map_err(|e| format!("mkdir work_dir: {e}"))?;

    let state = Arc::new(ProbeCapsState::default());
    let adapter = Arc::new(ProbeCapsAdapter(state.clone()));
    rt.set_host_capabilities(HostCapabilities {
        download: Some(adapter.clone()),
        js_dialog: Some(adapter.clone()),
        permission: None,
        popup: Some(adapter.clone()),
        auth: Some(adapter.clone()),
        certificate: None,
        cookie_access: Some(adapter.clone()),
        cdp_observer: Some(adapter.clone()),
        file_dialog: None,
        audio: None,
    })
    .map_err(|e| format!("set_host_capabilities: {e:?}"))?;

    rt.attach_cdp_observer(b1_id, adapter.clone())
        .map_err(|e| format!("attach_cdp_observer: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "cdp observer attached", |rt| {
        Ok(rt.cdp_diagnostics().attached >= 1)
    })?;

    b1.submit(BrowserCommand::Navigate(fixture.url("/opt-in")))
        .map_err(|e| format!("navigate opt-in: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "b1 opt-in ready", |_| {
        let snap = b1.snapshot();
        Ok(!snap.navigation.is_loading && snap.navigation.title == "optin:ready")
    })?;
    let src1 = rt
        .frame_source(b1_id)
        .map_err(|e| format!("frame_source b1: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "b1 opt-in pixel #2a5c8e", |_| {
        let Some(frame) = src1.latest(SurfaceKind::View) else {
            return Ok(false);
        };
        if frame.info.format != PixelFormat::PremultipliedBgra8
            || frame.info.width < 320
            || frame.info.height < 240
        {
            return Ok(false);
        }
        let off = (120 * frame.info.width as usize + 160) * 4;
        Ok(frame.cpu_pixels().and_then(|px| px.as_slice().get(off..off + 4)) == Some(&BGRA_2A5C8E))
    })?;

    // 2a. Headless CDP method call (`Runtime.evaluate` -> 42) & trigger JS prompt.
    let eval42 = rt
        .execute_cdp_method(
            b1_id,
            "Runtime.evaluate",
            Some(r#"{"expression":"6*7","returnByValue":true}"#.into()),
        )
        .map_err(|e| format!("cdp eval 6*7: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "cdp eval 42", |_| {
        let results = state.cdp_results.lock().unwrap();
        Ok(results
            .iter()
            .any(|r| r.id == eval42 && r.success && r.result_json.contains("42")))
    })?;
    report.cdp_method_eval_42_ok = true;

    // 2b. JS dialog (`prompt`) single-use resolution + duplicate rejection.
    rt.execute_cdp_method(
        b1_id,
        "Runtime.evaluate",
        Some(r#"{"expression":"setTimeout(() => { window.__promptAns = prompt('code?', 'x'); }, 0); true"}"#.into()),
    )
    .map_err(|e| format!("cdp trigger prompt: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "js dialog prompt", |_| {
        Ok(!state.js_dialogs.lock().unwrap().is_empty())
    })?;
    let prompt_req = state.js_dialogs.lock().unwrap().remove(0);
    rt.resolve_js_dialog(
        prompt_req.id,
        JsDialogDecision::Accept {
            prompt_input: Some("probe-42".into()),
        },
    )
    .map_err(|e| format!("resolve_js_dialog: {e:?}"))?;
    report.js_dialog_resolved_once = true;
    report.js_dialog_duplicate_rejected = matches!(
        rt.resolve_js_dialog(prompt_req.id, JsDialogDecision::Cancel),
        Err(HostCapabilityError::AlreadyConsumed)
    );
    let prompt_check = rt
        .execute_cdp_method(
            b1_id,
            "Runtime.evaluate",
            Some(r#"{"expression":"window.__promptAns","returnByValue":true}"#.into()),
        )
        .map_err(|e| format!("cdp check prompt answer: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "prompt answer probe-42", |_| {
        let results = state.cdp_results.lock().unwrap();
        Ok(results
            .iter()
            .any(|r| r.id == prompt_check && r.success && r.result_json.contains("probe-42")))
    })?;

    // 2c. Popup capture & block (returns 1, zero unmanaged native popup).
    send_click(b1, 160, 120)?;
    wait_until(rt, mode, tick, deadline, "popup capture", |_| {
        Ok(!state.popups.lock().unwrap().is_empty())
    })
    .map_err(|e| {
        format!(
            "{e} (title={}, denials={:?})",
            b1.snapshot().navigation.title,
            policy_denials.lock().unwrap()
        )
    })?;
    let popup_req = state.popups.lock().unwrap().remove(0);
    report.popup_captured_and_blocked = popup_req.target_url.ends_with("/popup-target")
        && popup_req.policy_allowed
        && popup_req.user_gesture;

    // 2d. Download: forbidden destination rejected, valid confirmed & bytes match.
    rt.start_download(b1_id, fixture.url("/download.bin"))
        .map_err(|e| format!("start_download sample: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "download request sample", |_| {
        Ok(!state.downloads.lock().unwrap().is_empty())
    })?;
    let dl_req = state.downloads.lock().unwrap().remove(0);
    let evil_dest = cache_root.join("cache-leak.bin");
    report.download_forbidden_dest_rejected = matches!(
        rt.confirm_download(dl_req.id, evil_dest),
        Err(HostCapabilityError::ForbiddenDestination)
    );
    let sample_dest = work_dir.join("sample.bin");
    rt.confirm_download(dl_req.id, sample_dest.clone())
        .map_err(|e| format!("confirm_download sample: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "download sample complete", |_| {
        let progress = state.download_progress.lock().unwrap();
        if let Some(p) = progress.iter().rfind(|p| p.id == dl_req.id) {
            match p.state {
                DownloadState::Complete => return Ok(true),
                DownloadState::Canceled | DownloadState::Interrupted => {
                    return Err(format!("download sample terminated: {p:?}"));
                }
                DownloadState::InProgress => {}
            }
        }
        Ok(false)
    })?;
    report.download_confirmed_bytes_ok =
        fs::read(&sample_dest).ok().as_deref() == Some(b"HOST-CAP-DOWNLOAD-OK-0123456789");

    // 2e. Profile cookie CRUD + CookieAccessPolicy + 0-cookie Drop + Preferences.
    let profile = rt.default_profile_handle();
    let cookie = CookieRecord {
        name: "host_cookie".into(),
        value: RedactedSecret::new("cookie-val-secret-999"),
        domain: String::new(),
        path: "/".into(),
        secure: false,
        http_only: true,
        same_site: CookieSameSiteKind::Unspecified,
        priority: CookiePriorityKind::Medium,
    };
    let set_ticket = rt
        .set_profile_cookie(profile, fixture.url("/"), cookie)
        .map_err(|e| format!("set_profile_cookie: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "cookie set complete", |_| {
        Ok(!matches!(set_ticket.poll(), CookieMutationStatus::Pending))
    })?;
    let visit_ticket = rt
        .visit_profile_cookies(profile, Some(fixture.url("/")), true)
        .map_err(|e| format!("visit_profile_cookies: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "cookie visit complete", |_| {
        Ok(!matches!(visit_ticket.poll(), CookieVisitStatus::Pending))
    })?;
    let cookies_found = match visit_ticket.poll() {
        CookieVisitStatus::Completed { cookies, .. } => cookies,
        other => return Err(format!("unexpected cookie visit: {other:?}")),
    };
    let cookie_dbg = format!("{cookies_found:?}");
    report.cookie_secrets_redacted =
        !cookie_dbg.contains("cookie-val-secret-999") && cookie_dbg.contains("[REDACTED]");

    // Trigger real HTTP request to /cookie-echo to verify CookieAccessPolicy hooks.
    rt.execute_cdp_method(
        b1_id,
        "Runtime.evaluate",
        Some(r#"{"expression":"fetch('/cookie-echo').then(r => r.text()).then(t => { window.__cookieEcho = t; })","awaitPromise":true}"#.into()),
    )
    .map_err(|e| format!("fetch cookie-echo: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "cookie filter hits", |_| {
        Ok(state.cookie_send_hits.load(Ordering::Acquire) >= 1
            && state.cookie_save_hits.load(Ordering::Acquire) >= 1)
    })?;
    report.cookie_filter_invoked = true;

    let del_ticket = rt
        .delete_profile_cookies(profile, None, None)
        .map_err(|e| format!("delete_profile_cookies: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "cookie delete complete", |_| {
        Ok(!matches!(del_ticket.poll(), CookieMutationStatus::Pending))
    })?;
    let zero_visit = rt
        .visit_profile_cookies(profile, None, true)
        .map_err(|e| format!("zero visit cookies: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "zero cookie visit", |_| {
        Ok(!matches!(zero_visit.poll(), CookieVisitStatus::Pending))
    })?;
    report.cookie_zero_visit_ok = matches!(
        zero_visit.poll(),
        CookieVisitStatus::Completed { cookies, .. } if cookies.is_empty()
    );
    report.cookie_set_visited_deleted_ok =
        cookies_found.iter().any(|c| c.name == "host_cookie") && report.cookie_zero_visit_ok;

    // Preferences: safe allowlist vs storage-contract blocklist.
    let pref_ticket = rt
        .set_profile_preference(
            profile,
            "intl.accept_languages",
            PreferenceValue::String("en-US,fr".into()),
        )
        .map_err(|e| format!("set_profile_preference: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "preference set", |_| {
        Ok(!matches!(pref_ticket.poll(), ProfileActionStatus::Pending))
    })?;
    report.pref_allow_ok = matches!(pref_ticket.poll(), ProfileActionStatus::Succeeded { .. });
    report.pref_blocked_rejected = matches!(
        rt.set_profile_preference(
            profile,
            "browser.cache_path",
            PreferenceValue::String(r"C:\evil-override".into()),
        ),
        Err(HostCapabilityError::PreferenceBlocked)
    );

    // 2f. Async PrintToPdf (distinct from physical Print 131).
    let pdf_path = work_dir.join("export.pdf");
    let pdf_ticket = rt
        .print_to_pdf(b1_id, pdf_path.clone(), PdfExportOptions::default())
        .map_err(|e| format!("print_to_pdf: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "print_to_pdf finished", |_| {
        Ok(!matches!(pdf_ticket.poll(), PdfExportStatus::Pending))
    })?;
    let pdf_bytes = fs::read(&pdf_path).unwrap_or_default();
    report.pdf_exported_header_ok = matches!(
        pdf_ticket.poll(),
        PdfExportStatus::Completed(out) if out.ok
    ) && pdf_bytes.starts_with(b"%PDF-");

    // 2g. HTTP 401 Basic Auth + credential redaction + clear cache.
    b1.submit(BrowserCommand::Navigate(fixture.url("/auth")))
        .map_err(|e| format!("navigate /auth: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "auth request", |_| {
        Ok(!state.auths.lock().unwrap().is_empty())
    })?;
    let auth_req = state.auths.lock().unwrap().remove(0);
    let decision = AuthDecision::Continue(AuthCredentials {
        username: RedactedSecret::new("alice"),
        password: RedactedSecret::new("super-secret-pw"),
    });
    let auth_dbg = format!("{auth_req:?} {decision:?}");
    report.auth_secrets_redacted =
        !auth_dbg.contains("alice") && !auth_dbg.contains("super-secret-pw");
    rt.resolve_auth(auth_req.id, decision)
        .map_err(|e| format!("resolve_auth: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "auth ok page", |_| {
        Ok(b1.snapshot().navigation.title == "auth:ok")
    })?;
    report.auth_callback_handled = true;

    let clear_auth = rt
        .clear_profile_http_auth_credentials(profile)
        .map_err(|e| format!("clear_profile_http_auth_credentials: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "clear auth cache", |_| {
        Ok(!matches!(clear_auth.poll(), ProfileActionStatus::Pending))
    })?;
    report.auth_cache_cleared = matches!(clear_auth.poll(), ProfileActionStatus::Succeeded { .. });

    // 2h. Start slow download, wait for InProgress, then Force-close Browser 1.
    rt.start_download(b1_id, fixture.url("/slow-download.bin"))
        .map_err(|e| format!("start_download slow: {e:?}"))?;
    wait_until(rt, mode, tick, deadline, "slow download request", |_| {
        Ok(!state.downloads.lock().unwrap().is_empty())
    })?;
    let slow_req = state.downloads.lock().unwrap().remove(0);
    rt.confirm_download(slow_req.id, work_dir.join("slow.bin"))
        .map_err(|e| format!("confirm slow download: {e:?}"))?;
    wait_until(
        rt,
        mode,
        tick,
        deadline,
        "slow download in_progress",
        |_| {
            Ok(state.download_progress.lock().unwrap().iter().any(|p| {
                p.id == slow_req.id && p.state == DownloadState::InProgress && p.full_path.is_some()
            }))
        },
    )?;

    b1.request_close(CloseMode::Force);
    let close_wait = wait_until(
        rt,
        mode,
        tick,
        deadline,
        "b1 closed + download cancelled",
        |rt| {
            let closed = b1.snapshot().state == SessionState::Closed;
            let dl_cancelled = state
                .download_progress
                .lock()
                .unwrap()
                .iter()
                .any(|p| p.id == slow_req.id && p.state == DownloadState::Canceled);
            Ok(closed && dl_cancelled && rt.host_capability_pending_count() == 0)
        },
    );
    if let Err(e) = close_wait {
        let p = state.download_progress.lock().unwrap().clone();
        let pending = rt.host_capability_pending_count();
        let cdp = rt.cdp_diagnostics();
        return Err(format!(
            "{e} (b1_state={:?}, pending={pending}, progress={p:?}, cdp={cdp:?})",
            b1.snapshot().state
        ));
    }
    report.force_close_download_cancelled = state
        .download_progress
        .lock()
        .unwrap()
        .iter()
        .any(|p| p.id == slow_req.id && p.state == DownloadState::Canceled);

    report.pending_host_reqs_after_close = rt.host_capability_pending_count();
    let cdp = rt.cdp_diagnostics();
    report.cdp_attached_before_shutdown = cdp.attached;
    report.cdp_dropped_on_ui_before_shutdown = cdp.dropped_on_ui;
    report.cdp_active_after_shutdown = cdp.active_registrations;

    let _ = fs::remove_dir_all(&work_dir);
    Ok(report)
}
