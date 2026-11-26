//! Isolated 8.3 policy/input probe. Never a CEF subprocess entry point.
//! Controlled labels only; no page URL, title, body or data URI enters reports.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod windows {
    use rust_cef_runtime::{
        contracts::*,
        input_fixture::{FrameInputSender, InputFixture, InputMessage},
        ports::*,
        runtime::{Runtime, RuntimeDiagnostics, RuntimeServices, ShutdownStatus},
        test_support::{DialogProbeAction, ProbeOptions, publish_observer_ready},
    };
    use serde_json::{Value, json};
    use std::{
        collections::{BTreeMap, BTreeSet},
        io::{Read, Write},
        net::TcpStream,
        sync::{Arc, Mutex, MutexGuard},
        thread,
        time::{Duration, Instant},
    };
    const CHECKS: [&str; 22] = [
        "runtime_ready",
        "own_frame_and_observer",
        "four_click_activation",
        "main_subresource_redirect_zero",
        "service_worker_zero",
        "file_dialog_refused",
        "permissions_refused",
        "certificate_refused",
        "js_dialogs_suppressed",
        "popup_download_protocol_refused",
        "native_windows_observed_clear",
        "graceful_denial_alive",
        "cancelled_ticket_retained",
        "pending_force_once",
        "reload_reset_stale_reply",
        "timeout_denial_alive",
        "data_reload_history",
        "data_cross_browser_denied",
        "data_unapproved_denied",
        "complete_browser_ownership",
        "native_shutdown_and_tasks",
        "mode_threads",
    ];
    fn lock<T>(v: &Mutex<T>) -> MutexGuard<'_, T> {
        v.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn require(ok: bool, label: &str) -> Result<(), String> {
        if ok { Ok(()) } else { Err(label.into()) }
    }
    fn check(report: &mut Value, id: &str, passed: bool) -> Result<(), String> {
        let checks = report["checks"].as_array_mut().unwrap();
        require(CHECKS.get(checks.len()).copied() == Some(id), "check-order")?;
        checks.push(json!({"id":id,"passed":passed}));
        require(passed, id)
    }
    fn http(origin: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let authority = origin
            .strip_prefix("http://127.0.0.1:")
            .ok_or("control-origin")?;
        let port: u16 = authority.parse().map_err(|_| "control-port")?;
        let mut socket =
            TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(2))
                .map_err(|_| "control-connect")?;
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .map_err(|_| "control-read-timeout")?;
        socket
            .set_write_timeout(Some(Duration::from_secs(2)))
            .map_err(|_| "control-write-timeout")?;
        let payload = body.map(Value::to_string).unwrap_or_default();
        let request = format!(
            "{} {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            if body.is_some() { "POST" } else { "GET" },
            path,
            port,
            payload.len(),
            payload
        );
        socket
            .write_all(request.as_bytes())
            .map_err(|_| "control-write")?;
        let mut bytes = Vec::new();
        socket
            .take(65537)
            .read_to_end(&mut bytes)
            .map_err(|_| "control-read")?;
        require(bytes.len() <= 65536, "control-body-limit")?;
        let split = bytes
            .windows(4)
            .position(|b| b == b"\r\n\r\n")
            .ok_or("control-headers")?;
        require(bytes.starts_with(b"HTTP/1.1 200 "), "control-status")?;
        serde_json::from_slice(&bytes[split + 4..]).map_err(|_| "control-json".into())
    }
    // Native TitleChanged witness emitted by the controlled document's own JS.
    // Opaque data contexts cannot reliably POST to loopback (Chromium PNA/CORS).
    fn data_witness<'a>(title: &'a str, expected: &str) -> Option<(&'a str, &'a str)> {
        let mut parts = title.split('|');
        if parts.next()? != expected {
            return None;
        }
        let phase = parts.next()?;
        let document = parts.next()?;
        if parts.next().is_some()
            || !matches!(phase, "loaded" | "attempted" | "survived")
            || document.len() > 32
            || document.is_empty()
            || !document.bytes().all(|b| b.is_ascii_digit() || b == b'.')
            || !document
                .parse::<f64>()
                .is_ok_and(|n| n.is_finite() && n > 0.)
        {
            return None;
        }
        Some((phase, document))
    }
    fn fixture_title_matches(title: &str, expected: &str) -> bool {
        if matches!(expected, "Data Probe" | "Data Attempt") {
            data_witness(title, expected).is_some()
        } else {
            title == expected
        }
    }
    fn data_uri(html: &str) -> String {
        format!(
            "data:text/html;charset=utf-8,{}",
            html.bytes()
                .map(|b| format!("%{b:02X}"))
                .collect::<String>()
        )
    }
    #[derive(Default)]
    struct Seen {
        expected: BTreeSet<BrowserId>,
        created: BTreeSet<BrowserId>,
        closed: BTreeSet<BrowserId>,
        cancelled: BTreeSet<BrowserId>,
        navigation_denials: BTreeMap<BrowserId, u64>,
        sequences: BTreeMap<BrowserId, u64>,
        requests: Vec<(BrowserId, RequestId, bool)>,
        latest: BTreeMap<BrowserId, OwnedFrame>,
        valid: BTreeSet<BrowserId>,
        painted: BTreeSet<BrowserId>,
        selected: Option<BrowserId>,
        frames: u64,
        center: Vec<u8>,
        error: Option<&'static str>,
    }
    struct Observations {
        seen: Mutex<Seen>,
        sender: FrameInputSender,
    }
    impl BrowserEventSink for Observations {
        fn publish(&self, event: BrowserEventEnvelope) -> PublishResult {
            let mut s = lock(&self.seen);
            let previous = s
                .sequences
                .insert(event.browser, event.sequence)
                .unwrap_or(0);
            if event.sequence <= previous {
                s.error.get_or_insert("event-sequence");
            }
            match event.event {
                BrowserEvent::Created => {
                    if !s.created.insert(event.browser) {
                        s.error.get_or_insert("duplicate-created");
                    }
                }
                BrowserEvent::Closed => {
                    if !s.closed.insert(event.browser) {
                        s.error.get_or_insert("duplicate-closed");
                    }
                }
                BrowserEvent::CloseCancelled => {
                    s.cancelled.insert(event.browser);
                }
                BrowserEvent::PolicyDenied {
                    action: PolicyAction::Navigation,
                } => {
                    *s.navigation_denials.entry(event.browser).or_default() += 1;
                }
                BrowserEvent::BeforeUnloadRequested { request, is_reload } => {
                    if s.requests.len() >= 64 || request.browser != event.browser {
                        s.error.get_or_insert("request-ownership");
                    } else {
                        s.requests.push((event.browser, request, is_reload));
                    }
                }
                BrowserEvent::CreateFailed(_)
                | BrowserEvent::CaptureFailed(_)
                | BrowserEvent::RendererTerminated { .. } => {
                    s.error.get_or_insert("native-browser-failure");
                }
                _ => {}
            }
            PublishResult::Published
        }
    }
    impl FrameSink for Observations {
        fn publish(&self, frame: OwnedFrame) -> PublishResult {
            if frame.info.surface != SurfaceKind::View {
                return PublishResult::Rejected;
            }
            let mut s = lock(&self.seen);
            s.frames += 1;
            let Some(pixels) = frame.cpu_pixels() else {
                s.error.get_or_insert("frame-layout");
                return PublishResult::Rejected;
            };
            if frame.info.width != 320 || frame.info.height != 240 || pixels.len() != 320 * 240 * 4
            {
                s.error.get_or_insert("frame-layout");
                return PublishResult::Rejected;
            }
            if s.latest.len() >= 8 && !s.latest.contains_key(&frame.info.browser) {
                s.error.get_or_insert("frame-browser-budget");
                return PublishResult::Rejected;
            }
            let pixel = (4 * 320 + 4) * 4; // Visible button covers center. Sample fixed background instead.
            if pixels[pixel..pixel + 4] == [126, 76, 26, 255] {
                s.valid.insert(frame.info.browser);
            }
            let center = (120 * 320 + 160) * 4;
            s.center = pixels[center..center + 4].to_vec();
            let show = s.selected == Some(frame.info.browser);
            s.latest.insert(frame.info.browser, frame.clone());
            drop(s);
            if show {
                self.sender.publish(frame)
            } else {
                PublishResult::Coalesced
            }
        }
    }
    impl UiDispatcher for Observations {
        fn wake(&self) -> Result<(), EngineError> {
            Ok(())
        }
    }
    impl Observations {
        fn select(&self, id: BrowserId) {
            let mut s = lock(&self.seen);
            s.selected = Some(id);
            let frame = s.latest.get(&id).cloned();
            drop(s);
            if let Some(frame) = frame {
                self.sender.publish(frame);
            }
        }
        fn request_after(&self, id: BrowserId, mark: usize) -> Option<(RequestId, bool)> {
            lock(&self.seen)
                .requests
                .iter()
                .skip(mark)
                .find(|v| v.0 == id)
                .map(|v| (v.1, v.2))
        }
        fn request_count(&self) -> usize {
            lock(&self.seen).requests.len()
        }
        fn navigation_denials(&self, browser: BrowserId) -> u64 {
            lock(&self.seen)
                .navigation_denials
                .get(&browser)
                .copied()
                .unwrap_or(0)
        }
    }
    struct Harness<'a> {
        runtime: &'a mut Runtime,
        fixture: &'a InputFixture,
        obs: &'a Observations,
        origin: &'a str,
        case: &'a str,
        mode: ThreadMode,
        active: Option<BrowserHandle>,
        state: Value,
        next_state: Instant,
        deadline: Instant,
        clicks: u64,
        mouse_messages: u64,
        input_diagnostics: Vec<Value>,
        windows_peak: usize,
    }
    impl Harness<'_> {
        fn tick(&mut self) -> Result<(), String> {
            if self.mode == ThreadMode::Integrated {
                self.runtime.pump().map_err(|_| "pump")?;
            }
            for message in self.fixture.poll()? {
                if let Some(handle) = &self.active {
                    match message {
                        InputMessage::Focus(focus) => {
                            if handle.snapshot().is_ready() {
                                handle
                                    .submit(BrowserCommand::SetFocus(focus))
                                    .map_err(|_| "focus-command")?;
                            }
                        }
                        InputMessage::MouseClick {
                            x,
                            y,
                            up,
                            os_injected,
                            low32_marker_match,
                            extra_info_zero,
                            injected_origin,
                        } => {
                            require(
                                self.input_diagnostics.len() < 32,
                                "input-observation-budget",
                            )?;
                            self.input_diagnostics.push(json!({"up":up,"marker_match":os_injected,
                                "low32_marker_match":low32_marker_match,"extra_info_zero":extra_info_zero,
                                "source_is_injected_mouse":injected_origin}));
                            // Exact full-value marker AND system provenance;
                            // neither low-word diagnostics nor physical events
                            // are an attribution fallback.
                            require(os_injected && injected_origin, "unattributed-win32-input")?;
                            self.mouse_messages += 1;
                            self.runtime
                                .forward_probe_mouse(handle.snapshot().browser, x, y, up)
                                .map_err(|_| "input-post")?;
                        }
                    }
                }
            }
            require(!self.fixture.close_requested(), "fixture-closed-by-user")?;
            self.windows_peak = self
                .windows_peak
                .max(self.fixture.unexpected_visible_windows()?);
            {
                let s = lock(&self.obs.seen);
                if let Some(error) = s.error {
                    return Err(error.into());
                }
                require(
                    s.sequences
                        .keys()
                        .chain(s.latest.keys())
                        .all(|id| s.expected.contains(id)),
                    "unexpected-browser-id",
                )?;
            }
            if Instant::now() >= self.next_state {
                self.state = http(self.origin, &format!("/state?case={}", self.case), None)?;
                self.next_state = Instant::now() + Duration::from_millis(75);
                require(
                    self.state["records"].is_array() && self.state["counts"].is_object(),
                    "fixture-state-shape",
                )?;
                require(
                    self.state["errors"].as_array().is_some_and(Vec::is_empty),
                    "fixture-sticky-errors",
                )?;
                require(
                    self.state["forbidden_requests"] == 0,
                    "forbidden-network-request",
                )?;
                require(
                    !self.records().iter().any(|r| r["label"] == "worker_error"),
                    "worker-fixture-error",
                )?;
            }
            thread::sleep(Duration::from_millis(5));
            Ok(())
        }
        fn wait(&mut self, label: &str, mut done: impl FnMut(&Self) -> bool) -> Result<(), String> {
            let phase = (Instant::now() + Duration::from_secs(20)).min(self.deadline);
            loop {
                self.tick()?;
                if done(self) {
                    return Ok(());
                }
                if Instant::now() >= phase {
                    return Err(format!("timeout:{label}"));
                }
            }
        }
        fn records(&self) -> &Vec<Value> {
            self.state["records"]
                .as_array()
                .expect("validated fixture records")
        }
        fn count(&self, label: &str) -> usize {
            self.records()
                .iter()
                .filter(|r| r["label"] == label)
                .count()
        }
        fn row(&self, label: &str) -> Option<&Value> {
            self.records().iter().rev().find(|r| r["label"] == label)
        }
        fn url(&self, path: &str) -> String {
            format!("{}{path}?case={}", self.origin, self.case)
        }
        fn open(&mut self, url: String, title: &str) -> Result<BrowserHandle, String> {
            let handle = self
                .runtime
                .create_browser(BrowserConfig {
                    initial_url: url,
                    profile: None,
                    render_preference: RenderPreference::SoftwareRequired,
                    viewport: Viewport {
                        width: 320,
                        height: 240,
                        scale: 1.0,
                    },
                    options: Default::default(),
                })
                .map_err(|_| "create-browser")?;
            let id = handle.snapshot().browser;
            lock(&self.obs.seen).expected.insert(id);
            self.obs.select(id);
            self.active = Some(handle.clone());
            self.wait("page-ready", |h| {
                let snap = handle.snapshot();
                snap.is_ready()
                    && fixture_title_matches(&snap.navigation.title, title)
                    && !snap.navigation.is_loading
                    && lock(&h.obs.seen).valid.contains(&id)
                    && h.painted_current(id)
            })?;
            lock(&self.obs.seen).painted.insert(id);
            Ok(handle)
        }
        fn painted_current(&self, id: BrowserId) -> bool {
            self.fixture.painted_frame().is_some_and(|frame| {
                frame.info.browser == id
                    && frame.info.width == 320
                    && frame.info.height == 240
                    && frame.cpu_pixels().is_some_and(|px| {
                        px.len() == 320 * 240 * 4
                            && px[(4 * 320 + 4) * 4..(4 * 320 + 4) * 4 + 4] == [126, 76, 26, 255]
                    })
            })
        }
        fn click(&mut self, handle: &BrowserHandle) -> Result<(), String> {
            require(
                self.painted_current(handle.snapshot().browser),
                "wrong-browser-paint-before-input",
            )?;
            self.active = Some(handle.clone());
            self.obs.select(handle.snapshot().browser);
            handle
                .submit(BrowserCommand::SetFocus(true))
                .map_err(|_| "click-focus")?;
            self.fixture.inject_click(160, 120)?;
            self.clicks += 1;
            Ok(())
        }
        fn force_close(&mut self, handle: &BrowserHandle) -> Result<(), String> {
            let ticket = handle.request_close(CloseMode::Force);
            let id = handle.snapshot().browser;
            self.wait("force-closed", |h| {
                ticket.result() == CloseResult::Closed
                    && handle.snapshot().state == SessionState::Closed
                    && lock(&h.obs.seen).closed.contains(&id)
            })?;
            self.active = None;
            Ok(())
        }
        fn armed(&mut self) -> Result<BrowserHandle, String> {
            let handle = self.open(self.url("/unload"), "Unload Probe")?;
            self.arm_existing(handle)
        }
        fn arm_existing(&mut self, handle: BrowserHandle) -> Result<BrowserHandle, String> {
            let mark = self.count("activation");
            let armed = self.count("unload_armed");
            self.wait("activation-content-painted", |h| {
                h.painted_current(handle.snapshot().browser)
            })?;
            self.click(&handle)?;
            self.wait("trusted-activation", |h| {
                h.count("activation") > mark && h.count("unload_armed") > armed
            })?;
            let row = self.row("activation").ok_or("missing-activation")?;
            require(
                ["is_trusted", "has_been_active", "is_active"]
                    .iter()
                    .all(|key| row[key] == true),
                "activation-not-trusted",
            )?;
            Ok(handle)
        }
        fn pending(
            &mut self,
            handle: &BrowserHandle,
            mark: usize,
        ) -> Result<(RequestId, bool), String> {
            let id = handle.snapshot().browser;
            self.wait("before-unload-request", |h| {
                h.obs.request_after(id, mark).is_some()
            })?;
            self.obs
                .request_after(id, mark)
                .ok_or("missing-request".into())
        }
        fn navigate(
            &mut self,
            handle: &BrowserHandle,
            command: BrowserCommand,
            title: &str,
        ) -> Result<(), String> {
            handle.submit(command).map_err(|_| "navigation-command")?;
            self.wait("navigation-result", |_| {
                let s = handle.snapshot();
                fixture_title_matches(&s.navigation.title, title)
                    && !s.navigation.is_loading
                    && s.is_ready()
            })
        }
    }
    fn scenario(
        h: &mut Harness<'_>,
        options: &ProbeOptions,
        report: &mut Value,
    ) -> Result<(), String> {
        h.wait("runtime-ready", |h| h.runtime.is_ready())?;
        check(
            report,
            "runtime_ready",
            h.runtime.profile_state(h.runtime.default_profile_handle())
                == Ok(rust_cef_runtime::runtime::ProfileState::Ready),
        )?;
        let a = h.open(h.url("/page"), "Policy Probe")?;
        let gate = options
            .observer_gate
            .as_ref()
            .ok_or("observer-gate-required")?;
        publish_observer_ready(gate, std::process::id()).map_err(|_| "observer-publish")?;
        h.wait("observer-release", |_| gate.is_file())?;
        check(
            report,
            "own_frame_and_observer",
            h.fixture.painted_frames() > 0,
        )?;
        for (step, label) in [
            (1, "network_done"),
            (2, "file_attempt"),
            (3, "tls_rejected"),
            (4, "unload_armed"),
        ] {
            h.click(&a)?;
            h.wait(label, |h| {
                h.count(label) > 0
                    && h.records()
                        .iter()
                        .any(|r| r["label"] == "click" && r["step"] == step)
            })?;
        }
        let clicks: Vec<_> = h
            .records()
            .iter()
            .filter(|r| r["label"] == "click")
            .collect();
        check(
            report,
            "four_click_activation",
            clicks.len() == 4
                && clicks.iter().all(|r| {
                    ["is_trusted", "has_been_active", "is_active"]
                        .iter()
                        .all(|key| r[key] == true)
                })
                && h.mouse_messages == 8,
        )?;
        let network = h.row("network_done").ok_or("missing-network-record")?;
        check(
            report,
            "main_subresource_redirect_zero",
            [
                "attempted",
                "popupAttempted",
                "externalAttempted",
                "fetchAttempted",
                "iframeAttempted",
                "imageAttempted",
                "redirectAttempted",
                "downloadAttempted",
                "mainAttempted",
                "fetchRejected",
                "redirectRejected",
                "imageRejected",
            ]
            .iter()
            .all(|key| network[key] == true)
                && h.state["forbidden_requests"] == 0
                && h.runtime.diagnostics().policy_denied[PolicyAction::Redirect as usize] > 0,
        )?;
        h.wait("service-worker", |h| h.count("worker") > 0)?;
        check(
            report,
            "service_worker_zero",
            h.row("worker")
                .is_some_and(|r| r["attempted"] == true && r["blocked"] == true)
                && h.state["forbidden_requests"] == 0,
        )?;
        check(
            report,
            "file_dialog_refused",
            h.runtime.diagnostics().policy_denied[PolicyAction::FileDialog as usize] > 0
                && h.count("file_change") == 0,
        )?;
        h.wait("permission-records", |h| {
            h.count("geolocation") > 0 && h.count("media") > 0 && h.count("notification") > 0
        })?;
        report["permission_records"] = json!(
            h.records()
                .iter()
                .filter(|r| matches!(
                    r["label"].as_str(),
                    Some("geolocation" | "media" | "notification")
                ))
                .collect::<Vec<_>>()
        );
        check(
            report,
            "permissions_refused",
            h.row("geolocation")
                .is_some_and(|r| r["supported"] == true && r["status"] == "denied")
                && h.runtime.diagnostics().policy_denied[PolicyAction::Permission as usize] > 0
                && h.row("notification")
                    .is_some_and(|r| r["status"] == "denied"),
        )?;
        // The subresource fetch can fail in Chromium without reaching the
        // Browser certificate handler. Exercise an actual main-frame request
        // on B before installing any beforeunload handler, then reuse B for
        // the close scenario; no extra Browser or fake callback is counted.
        let b = h.open(h.url("/plain"), "Plain Probe")?;
        let tls = http(h.origin, "/register", Some(&json!({"case":h.case})))?["tls_origin"]
            .as_str()
            .ok_or("certificate-origin")?
            .to_owned();
        let cert_before = h.runtime.diagnostics().policy_denied[PolicyAction::Certificate as usize];
        b.submit(BrowserCommand::Navigate(format!(
            "{tls}/certificate?case={}",
            h.case
        )))
        .map_err(|_| "certificate-navigation")?;
        h.wait("main-certificate-handler", |h| {
            let s = b.snapshot();
            h.runtime.diagnostics().policy_denied[PolicyAction::Certificate as usize] > cert_before
                && s.navigation.last_main_frame_error == Some(-202)
                && !s.navigation.is_loading
        })?;
        report["certificate_main_error"] = json!(b.snapshot().navigation.last_main_frame_error);
        check(
            report,
            "certificate_refused",
            h.row("tls_rejected")
                .is_some_and(|r| r["attempted"] == true && r["rejected"] == true)
                && h.state["tls_requests"] == 0
                && h.runtime.diagnostics().policy_denied[PolicyAction::Certificate as usize] > 0,
        )?;
        check(
            report,
            "js_dialogs_suppressed",
            h.row("js_dialogs")
                .is_some_and(|r| r["confirm"] == false && r["prompt_null"] == true)
                && h.runtime.diagnostics().policy_denied[PolicyAction::JsDialog as usize] >= 3,
        )?;
        let d = h.runtime.diagnostics();
        check(
            report,
            "popup_download_protocol_refused",
            [
                PolicyAction::Popup,
                PolicyAction::Download,
                PolicyAction::ExternalProtocol,
            ]
            .iter()
            .all(|p| d.policy_denied[*p as usize] > 0),
        )?;
        check(report, "native_windows_observed_clear", h.windows_peak == 0)?;
        h.force_close(&a)?;

        h.active = Some(b.clone());
        h.obs.select(b.snapshot().browser);
        h.navigate(
            &b,
            BrowserCommand::Navigate(h.url("/unload")),
            "Unload Probe",
        )?;
        let b = h.arm_existing(b)?;
        let mark = h.obs.request_count();
        let before = h.count("before_unload");
        let cancelled = b.request_close(CloseMode::Graceful);
        let (request, reload) = h.pending(&b, mark)?;
        require(
            !reload
                && cancelled.result() == CloseResult::Pending
                && b.snapshot().state == SessionState::Closing,
            "close-not-pending",
        )?;
        require(
            !lock(&h.obs.seen).closed.contains(&b.snapshot().browser),
            "premature-closed",
        )?;
        b.submit(BrowserCommand::ResolveBeforeUnload {
            request,
            allow: false,
        })
        .map_err(|_| "deny-reply")?;
        h.wait("cancelled-alive", |h| {
            cancelled.result() == CloseResult::Cancelled
                && b.snapshot().is_ready()
                && lock(&h.obs.seen).cancelled.contains(&b.snapshot().browser)
                && h.count("before_unload") > before
        })?;
        check(
            report,
            "graceful_denial_alive",
            h.row("before_unload")
                .is_some_and(|r| r["has_been_active"] == true && r["is_trusted"] == true)
                && !lock(&h.obs.seen).closed.contains(&b.snapshot().browser),
        )?;
        h.force_close(&b)?;
        check(
            report,
            "cancelled_ticket_retained",
            cancelled.result() == CloseResult::Cancelled,
        )?;

        let c = h.armed()?;
        let mark = h.obs.request_count();
        let ticket = c.request_close(CloseMode::Graceful);
        let (request, _) = h.pending(&c, mark)?;
        h.force_close(&c)?;
        check(
            report,
            "pending_force_once",
            ticket.result() == CloseResult::Closed
                && c.submit(BrowserCommand::ResolveBeforeUnload {
                    request,
                    allow: true,
                })
                .is_err(),
        )?;

        let d = h.armed()?;
        let did = d.snapshot().browser;
        h.runtime
            .probe_dialog_control(did, DialogProbeAction::Enable)
            .map_err(|_| "reset-enable-post")?;
        h.wait("reset-enable-ack", |h| {
            h.runtime
                .probe_dialog_status(did, DialogProbeAction::Enable)
                .is_some()
        })?;
        require(
            h.runtime
                .probe_dialog_status(did, DialogProbeAction::Enable)
                == Some(true),
            "reset-enable-failed",
        )?;
        let mark = h.obs.request_count();
        d.submit(BrowserCommand::Reload)
            .map_err(|_| "reload-submit")?;
        let (old, is_reload) = h.pending(&d, mark)?;
        require(is_reload, "reload-request-kind")?;
        let invalid = h.runtime.diagnostics().before_unload_invalidated;
        let resets = h.runtime.probe_dialog_reset_count(did);
        let timeouts = h.runtime.diagnostics().before_unload_timeouts;
        h.runtime
            .probe_dialog_control(did, DialogProbeAction::Reset)
            .map_err(|_| "native-reset-post")?;
        h.wait("native-reset-ack", |h| {
            h.runtime
                .probe_dialog_status(did, DialogProbeAction::Reset)
                .is_some()
        })?;
        require(
            h.runtime.probe_dialog_status(did, DialogProbeAction::Reset) == Some(true),
            "native-reset-failed",
        )?;
        h.wait("reset-invalidated", |h| {
            h.runtime.diagnostics().before_unload_invalidated > invalid
                && h.runtime.probe_dialog_reset_count(did) > resets
        })?;
        require(
            h.runtime.diagnostics().before_unload_timeouts == timeouts,
            "reset-was-not-timeout",
        )?;
        require(
            d.submit(BrowserCommand::ResolveBeforeUnload {
                request: old,
                allow: true,
            })
            .is_err(),
            "stale-reset-reply",
        )?;
        h.runtime
            .probe_dialog_control(did, DialogProbeAction::Disable)
            .map_err(|_| "reset-disable-post")?;
        h.wait("reset-disable-ack", |h| {
            h.runtime
                .probe_dialog_status(did, DialogProbeAction::Disable)
                .is_some()
        })?;
        require(
            h.runtime
                .probe_dialog_status(did, DialogProbeAction::Disable)
                == Some(true),
            "reset-disable-failed",
        )?;
        report["native_reset_notifications"] =
            json!(h.runtime.probe_dialog_reset_count(did) - resets);
        report["native_reset_enable_ack"] = json!(true);
        report["native_reset_handle_ack"] = json!(true);
        report["native_reset_disable_ack"] = json!(true);
        check(report, "reload_reset_stale_reply", d.snapshot().is_ready())?;
        h.force_close(&d)?;

        let e = h.armed()?;
        let mark = h.obs.request_count();
        let timeouts = h.runtime.diagnostics().before_unload_timeouts;
        let ticket = e.request_close(CloseMode::Graceful);
        h.pending(&e, mark)?;
        let started = Instant::now();
        h.wait("default-timeout", |h| {
            h.runtime.diagnostics().before_unload_timeouts > timeouts
                && ticket.result() == CloseResult::Cancelled
                && e.snapshot().is_ready()
        })?;
        report["timeout_wait_ms"] = json!(started.elapsed().as_millis() as u64);
        check(
            report,
            "timeout_denial_alive",
            started.elapsed() >= Duration::from_secs(9)
                && !lock(&h.obs.seen).closed.contains(&e.snapshot().browser),
        )?;
        h.force_close(&e)?;

        let forbidden =
            http(h.origin, "/register", Some(&json!({"case":h.case})))?["forbidden_origin"]
                .as_str()
                .ok_or("forbidden-origin")?
                .to_owned();
        let html = format!(
            r#"<!doctype html><title>Data Probe</title><style>html,body{{margin:0;background:#1a4c7e}}</style><script>
const stamp=String(performance.timeOrigin),mark=phase=>document.title='Data Probe|'+phase+'|'+stamp;
mark('loaded');fetch({forbidden},{{mode:'no-cors'}}).catch(()=>{{}});</script>"#,
            forbidden = json!(format!("{forbidden}/data-network?case={}", h.case))
        );
        let uri = data_uri(&html);
        http(
            h.origin,
            "/register",
            Some(&json!({"case":h.case,"data_uri":uri})),
        )?;
        let f = h.open(uri.clone(), "Data Probe")?;
        let initial = f.snapshot().navigation.title;
        require(
            data_witness(&initial, "Data Probe").is_some_and(|(phase, _)| phase == "loaded"),
            "data-script",
        )?;
        f.submit(BrowserCommand::Reload)
            .map_err(|_| "data-reload")?;
        h.wait("data-reloaded", |_| {
            let s = f.snapshot();
            data_witness(&s.navigation.title, "Data Probe")
                .is_some_and(|(phase, _)| phase == "loaded")
                && s.navigation.title != initial
                && !s.navigation.is_loading
        })?;
        report["data_reload_document_changed"] = json!(true);
        h.navigate(&f, BrowserCommand::Navigate(h.url("/plain")), "Plain Probe")?;
        h.navigate(&f, BrowserCommand::Back, "Data Probe")?;
        require(
            f.snapshot().navigation.address == uri,
            "data-history-address",
        )?;
        h.navigate(&f, BrowserCommand::Forward, "Plain Probe")?;
        h.navigate(&f, BrowserCommand::Back, "Data Probe")?;
        check(
            report,
            "data_reload_history",
            f.snapshot().navigation.address == uri && h.state["forbidden_requests"] == 0,
        )?;
        // Renderer-initiated top data navigations can be blocked by Chromium
        // before CEF. Use a native CEF attempt without the ordinary host command's
        // approval step; require the actual per-Browser policy callback ACK.
        let wrapper = data_uri(
            r#"<!doctype html><title>Data Attempt</title><style>html,body{margin:0;background:#1a4c7e}</style><script>document.title='Data Attempt|loaded|'+String(performance.timeOrigin);</script>"#,
        );
        let g = h.open(wrapper, "Data Attempt")?;
        let original = g.snapshot().navigation.title;
        let denied = h.obs.navigation_denials(g.snapshot().browser);
        h.runtime
            .probe_unapproved_data_navigation(g.snapshot().browser, uri.clone())
            .map_err(|_| "cross-data-native-post")?;
        h.wait("cross-browser-data-denied", |h| {
            let s = g.snapshot();
            s.navigation.title == original
                && !s.navigation.is_loading
                && h.obs.navigation_denials(s.browser) > denied
        })?;
        report["data_cross_browser_native_denials"] =
            json!(h.obs.navigation_denials(g.snapshot().browser) - denied);
        check(
            report,
            "data_cross_browser_denied",
            fixture_title_matches(&g.snapshot().navigation.title, "Data Attempt")
                && !g.snapshot().navigation.is_loading
                && fixture_title_matches(&f.snapshot().navigation.title, "Data Probe"),
        )?;
        h.force_close(&g)?;
        h.active = Some(f.clone());
        h.obs.select(f.snapshot().browser);
        let other = uri.replace(
            "%44%61%74%61%20%50%72%6F%62%65",
            "%4E%6F%74%20%41%70%70%72%6F%76%65%64",
        );
        require(other != uri, "different-data-fixture")?;
        let denials = h.obs.navigation_denials(f.snapshot().browser);
        let original = f.snapshot().navigation.title;
        h.runtime
            .probe_unapproved_data_navigation(f.snapshot().browser, other)
            .map_err(|_| "unapproved-data-native-post")?;
        h.wait("unapproved-data-denied", |h| {
            let s = f.snapshot();
            s.navigation.title == original
                && !s.navigation.is_loading
                && h.obs.navigation_denials(s.browser) > denials
        })?;
        report["data_unapproved_native_denials"] =
            json!(h.obs.navigation_denials(f.snapshot().browser) - denials);
        check(
            report,
            "data_unapproved_denied",
            !f.snapshot().navigation.is_loading
                && fixture_title_matches(&f.snapshot().navigation.title, "Data Probe")
                && h.state["forbidden_requests"] == 0,
        )?;
        h.force_close(&f)?;
        let s = lock(&h.obs.seen);
        check(
            report,
            "complete_browser_ownership",
            s.expected.len() == 7
                && s.expected == s.created
                && s.created == s.closed
                && s.valid == s.expected
                && s.painted == s.expected
                && h.clicks == 8
                && h.mouse_messages == 16,
        )?;
        report["created"] = json!(s.created.len());
        report["closed"] = json!(s.closed.len());
        report["valid_frame_browsers"] = json!(s.valid.len());
        report["painted_frame_browsers"] = json!(s.painted.len());
        report["frames"] = json!(s.frames);
        report["last_center_bgra"] = json!(s.center);
        Ok(())
    }
    fn diagnostics(d: &RuntimeDiagnostics) -> Value {
        json!({"mode":format!("{:?}",d.mode),"main_thread_id":d.main_thread_id,"init_thread_id":d.init_thread_id,
        "cef_ui_thread_id":d.cef_ui_thread_id,"shutdown_thread_id":d.shutdown_thread_id,"manual_pump_count":d.manual_pump_count,
        "actual_backend":format!("{:?}",d.observed_backend),"tasks_posted":d.tasks.posted,"tasks_completed":d.tasks.completed,
        "task_post_failures":d.tasks.post_failed,"pending_tasks":d.tasks.pending,"pending_requests":d.pending_requests,
        "pending_profiles":d.pending_profiles,"pending_cookie_flushes":d.pending_cookie_flushes,"profile_references_retained":d.profile_references_retained,
        "cef_shutdown_calls":d.cef_shutdown_calls,"native_calls":d.native_calls,"native_calls_after_shutdown":d.native_calls_after_shutdown,
        "app_reference_retained":d.app_reference_retained,"root_lease_held":d.root_lease_held,
        "policy_denied":d.policy_denied,"policy_unattributed_denials":d.policy_unattributed_denials,"policy_factory_without_browser":d.policy_factory_without_browser,
        "before_unload_requested":d.before_unload_requested,"before_unload_answered":d.before_unload_answered,
        "before_unload_invalidated":d.before_unload_invalidated,"before_unload_timeouts":d.before_unload_timeouts})
    }
    fn validate(report: &Value) -> bool {
        let Some(checks) = report["checks"].as_array() else {
            return false;
        };
        report["schema"] == 1
            && report["kind"] == "policy"
            && report["error"].is_null()
            && checks.len() == CHECKS.len()
            && checks
                .iter()
                .zip(CHECKS)
                .all(|(c, id)| c["id"] == id && c["passed"] == true)
    }
    fn run(
        options: &ProbeOptions,
        origin: &str,
        storage: &str,
        case: &str,
        report: &mut Value,
    ) -> Result<(), String> {
        require(
            options.backend == RenderPreference::SoftwareRequired,
            "policy-software-only",
        )?;
        let registration = http(origin, "/register", Some(&json!({"case":case})))?;
        require(
            registration["allowed_origin"] == origin,
            "registered-control-origin",
        )?;
        let tls = registration["tls_origin"]
            .as_str()
            .ok_or("registered-tls-origin")?;
        let window = InputFixture::new(320, 240)?;
        let obs = Arc::new(Observations {
            seen: Mutex::new(Seen::default()),
            sender: window.frame_sender(),
        });
        let root = options
            .report
            .parent()
            .ok_or("report-parent")?
            .join("cache");
        let profile = if storage == "Memory" {
            ProfileStorage::Memory
        } else {
            ProfileStorage::Persistent {
                directory: root.clone(),
            }
        };
        let host = std::env::current_exe().map_err(|_| "host-path")?;
        let mut runtime = Runtime::start(
            RuntimeConfig {
                thread_mode: options.mode,
                files: RuntimeFiles {
                    directory: host.parent().ok_or("host-parent")?.into(),
                    helper_executable: options.helper.clone(),
                },
                root_cache_path: root,
                default_profile: profile,
                content_policy: TrustedContentPolicy {
                    allowed_origins: vec![origin.into(), tls.into()],
                    allow_embedded_data: true,
                },
                settings: Default::default(),
            },
            RuntimeServices {
                events: obs.clone(),
                frames: obs.clone(),
                ui: obs.clone(),
            },
        )
        .map_err(|e| format!("start:{e:?}"))?;
        let initial = http(origin, &format!("/state?case={case}"), None)?;
        let mut h = Harness {
            runtime: &mut runtime,
            fixture: &window,
            obs: &obs,
            origin,
            case,
            mode: options.mode,
            active: None,
            state: initial,
            next_state: Instant::now(),
            deadline: Instant::now() + Duration::from_secs(100),
            clicks: 0,
            mouse_messages: 0,
            input_diagnostics: Vec::new(),
            windows_peak: 0,
        };
        let result = scenario(&mut h, options, report);
        if let Err(error) = &result {
            report["error"] = json!(error);
        }
        report["injected_clicks"] = json!(h.clicks);
        report["os_mouse_messages"] = json!(h.mouse_messages);
        report["observed_mouse_messages"] = json!(h.input_diagnostics.len());
        report["input_diagnostics"] = json!(h.input_diagnostics);
        report["painted_frames"] = json!(window.painted_frames());
        report["unexpected_visible_windows_peak"] = json!(h.windows_peak);
        {
            let seen = lock(&obs.seen);
            report["observed_counts"] = json!({"expected":seen.expected.len(),"created":seen.created.len(),
                "closed_before_cleanup":seen.closed.len(),"valid_frames":seen.valid.len(),"painted_browsers":seen.painted.len()});
        }
        if let Some(frame) = window.painted_frame() {
            let sample = (4 * 320 + 4) * 4;
            report["last_paint"] = json!({"browser_local":frame.info.browser.local(),"width":frame.info.width,
                "height":frame.info.height,"sequence":frame.info.sequence,
                "background_bgra":frame.cpu_pixels().and_then(|px| px.get(sample..sample+4))});
        }
        if let Some(active) = &h.active {
            let s = active.snapshot();
            report["phase_snapshot"] = json!({"browser_local":s.browser.local(),"state":format!("{:?}",s.state),
                "loading":s.navigation.is_loading,"title_is_fixed_fixture":matches!(s.navigation.title.as_str(),
                    "Policy Probe"|"Unload Probe"|"Plain Probe"|"Data Probe"|"Data Attempt")});
        }
        h.active = None;
        let deadline = Instant::now() + Duration::from_secs(20);
        let ticket = h.runtime.request_shutdown(CloseMode::Force, deadline);
        let mut finished = false;
        loop {
            match h.runtime.poll_shutdown(&ticket) {
                ShutdownStatus::Ready => {
                    finished = h.runtime.finish_shutdown().is_ok();
                    break;
                }
                ShutdownStatus::Stopped | ShutdownStatus::ProcessExitRequired { .. } => break,
                _ => {}
            }
            if Instant::now() >= deadline {
                break;
            }
            // Failure cleanup pumps CEF and the fixture; it does not send input.
            if h.mode == ThreadMode::Integrated {
                let _ = h.runtime.pump();
            }
            let _ = window.poll();
            thread::sleep(Duration::from_millis(5));
        }
        let d = h.runtime.diagnostics();
        report["diagnostics"] = diagnostics(&d);
        report["probe_dialog_registrations"] = json!(h.runtime.probe_dialog_references());
        report["fixture_state"] = http(origin, &format!("/state?case={case}"), None)?;
        require(finished, "normal-shutdown-incomplete")?;
        result?;
        check(
            report,
            "native_shutdown_and_tasks",
            d.cef_shutdown_calls == 1
                && d.native_calls > 0
                && d.native_calls_after_shutdown == 0
                && h.runtime.probe_dialog_references() == 0
                && d.pending_requests == 0
                && d.pending_profiles == 0
                && d.pending_cookie_flushes == 0
                && d.profile_references_retained == 0
                && !d.app_reference_retained
                && !d.root_lease_held
                && d.tasks.pending == 0
                && d.tasks.posted > 0
                && d.tasks.completed + d.tasks.post_failed == d.tasks.posted
                && d.observed_backend == Some(RenderPreference::SoftwareRequired),
        )?;
        check(
            report,
            "mode_threads",
            d.init_thread_id == d.main_thread_id
                && d.shutdown_thread_id == Some(d.main_thread_id)
                && match options.mode {
                    ThreadMode::Integrated => {
                        d.cef_ui_thread_id == Some(d.main_thread_id) && d.manual_pump_count > 0
                    }
                    ThreadMode::Threaded => {
                        d.cef_ui_thread_id.is_some_and(|id| id != d.main_thread_id)
                            && d.manual_pump_count == 0
                    }
                },
        )?;
        require(
            report["fixture_state"]["forbidden_requests"] == 0
                && report["fixture_state"]["errors"]
                    .as_array()
                    .is_some_and(Vec::is_empty),
            "final-fixture-state",
        )?;
        Ok(())
    }
    pub fn main() -> i32 {
        let mut base = Vec::new();
        let mut custom = BTreeMap::new();
        let mut args = std::env::args().skip(1);
        while let Some(key) = args.next() {
            let Some(value) = args.next() else {
                eprintln!("missing-option-value");
                return 2;
            };
            if matches!(key.as_str(), "--fixture-origin" | "--storage" | "--case") {
                if custom.insert(key, value).is_some() {
                    return 2;
                }
            } else {
                base.push(key);
                base.push(value);
            }
        }
        let Ok(options) = ProbeOptions::parse(base) else {
            eprintln!("invalid-probe-options");
            return 2;
        };
        let Some(origin) = custom.remove("--fixture-origin") else {
            return 2;
        };
        let Some(storage) = custom.remove("--storage") else {
            return 2;
        };
        let Some(case) = custom.remove("--case") else {
            return 2;
        };
        if !matches!(storage.as_str(), "Memory" | "Persistent")
            || case.is_empty()
            || case.len() > 64
            || !case
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        {
            return 2;
        }
        let mut report = json!({"schema":1,"kind":"policy","requested_mode":options.mode_name(),"storage":storage,"pid":std::process::id(),
            "success":false,"error":null,"exit_code":1,"checks":[]});
        if let Err(error) = run(&options, &origin, &storage, &case, &mut report) {
            if report["error"].is_null() {
                report["error"] = json!(error);
            }
        }
        let success = validate(&report);
        report["success"] = json!(success);
        let exit = if success { 0 } else { 1 };
        report["exit_code"] = json!(exit);
        if let Some(parent) = options.report.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&options.report, serde_json::to_vec_pretty(&report).unwrap()).is_err() {
            return 1;
        }
        exit
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn data_witness_requires_script_phase_and_positive_document_stamp() {
            assert!(fixture_title_matches(
                "Data Probe|loaded|123.5",
                "Data Probe"
            ));
            assert!(fixture_title_matches(
                "Data Attempt|survived|456",
                "Data Attempt"
            ));
            for value in [
                "Data Probe",
                "Data Probe|loaded|NaN",
                "Data Probe|other|123",
                "Data Probe|loaded|0",
                "Data Probe|loaded|123|extra",
            ] {
                assert!(!fixture_title_matches(value, "Data Probe"));
            }
            assert!(!fixture_title_matches(
                "Data Attempt|loaded|123",
                "Data Probe"
            ));
        }
        #[test]
        fn fixed_checks_reject_missing_unknown_failed_or_error_rows() {
            let mut r = json!({"schema":1,"kind":"policy","error":null,"checks":CHECKS.map(|id|json!({"id":id,"passed":true}))});
            assert!(validate(&r));
            let all = r["checks"].clone();
            r["checks"] = json!([]);
            assert!(!validate(&r));
            r["checks"] = all;
            r["checks"][0]["id"] = json!("unknown");
            assert!(!validate(&r));
            r["checks"][0]["id"] = json!(CHECKS[0]);
            r["checks"][0]["passed"] = json!(false);
            assert!(!validate(&r));
            r["checks"][0]["passed"] = json!(true);
            r["error"] = json!("sticky");
            assert!(!validate(&r));
        }
    }
}
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn main() {
    std::process::exit(windows::main());
}
#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
fn main() {
    eprintln!("policy-probe requires Windows x64");
    std::process::exit(2);
}
