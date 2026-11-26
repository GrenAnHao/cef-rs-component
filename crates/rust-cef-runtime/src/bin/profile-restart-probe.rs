//! Cross-process profile persistence host. The HTTP fixture lives outside this process.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod windows {
    use rust_cef_core::profiles::ProfileState;
    use rust_cef_runtime::{
        contracts::{
            BrowserConfig, ProfileConfig, ProfileHandle, ProfileStorage, RenderPreference,
            RuntimeConfig, RuntimeFiles, SessionState, ThreadMode, TrustedContentPolicy, Viewport,
        },
        ports::{
            BrowserEvent, BrowserEventEnvelope, BrowserEventSink, CloseMode, EngineError,
            FrameSink, OwnedFrame, PublishResult, SurfaceKind, UiDispatcher,
        },
        runtime::{CookieFlushStatus, Runtime, RuntimeServices, ShutdownStatus},
        test_support::{ProbeOptions, ProbeScenario, publish_observer_ready},
    };
    use serde_json::{Value, json};
    use std::{
        collections::HashSet,
        io::{Read, Write},
        net::TcpStream,
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    fn thread_id() -> u64 {
        unsafe { GetCurrentThreadId() as u64 }
    }
    struct Extra {
        storage: String,
        phase: String,
        root: PathBuf,
        origin: String,
        prefix: String,
    }
    fn options() -> Result<(ProbeOptions, Extra), String> {
        let mut rest = Vec::new();
        let mut extra = std::collections::HashMap::new();
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            if [
                "--storage",
                "--phase",
                "--root",
                "--fixture-origin",
                "--token-prefix",
            ]
            .contains(&arg.as_str())
            {
                let value = args
                    .next()
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                if extra.insert(arg.clone(), value).is_some() {
                    return Err(format!("duplicate {arg}"));
                }
            } else {
                rest.push(arg);
            }
        }
        let mut get = |key: &str| extra.remove(key).ok_or_else(|| format!("missing {key}"));
        let storage = get("--storage")?;
        let phase = get("--phase")?;
        let root = PathBuf::from(get("--root")?);
        let origin = get("--fixture-origin")?;
        let prefix = get("--token-prefix")?;
        if !["Memory", "Persistent"].contains(&storage.as_str())
            || !["Write", "Read"].contains(&phase.as_str())
            || !root.is_absolute()
            || prefix.is_empty()
            || prefix.len() > 250
            || !prefix
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        {
            return Err("invalid restart options".into());
        }
        let port = origin
            .strip_prefix("http://127.0.0.1:")
            .ok_or("invalid fixture origin")?;
        if port.parse::<u16>().ok().filter(|p| *p != 0).is_none() {
            return Err("invalid fixture port".into());
        }
        Ok((
            ProbeOptions::parse(rest)?,
            Extra {
                storage,
                phase,
                root,
                origin,
                prefix,
            },
        ))
    }
    fn request(
        origin: &str,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let address = origin.strip_prefix("http://").ok_or("bad origin")?;
        let mut stream =
            TcpStream::connect(address).map_err(|e| format!("fixture connect: {e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .map_err(|e| e.to_string())?;
        let data = body.map(|v| v.to_string()).unwrap_or_default();
        write!(stream, "{method} {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}", data.len()).map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        stream
            .take(65537)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("fixture read: {e}"))?;
        if bytes.len() > 65536 {
            return Err("fixture response too large".into());
        }
        let split = bytes
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or("fixture malformed response")?;
        let header = std::str::from_utf8(&bytes[..split]).map_err(|_| "fixture invalid headers")?;
        if !header.starts_with("HTTP/1.1 200 ") {
            return Err("fixture HTTP failure".into());
        }
        let length = header
            .lines()
            .skip(1)
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, v)| v.trim().parse::<usize>().ok())
            })
            .ok_or("fixture missing length")?;
        if bytes.len() - split - 4 != length {
            return Err("fixture incomplete response".into());
        }
        serde_json::from_slice(&bytes[split + 4..]).map_err(|_| "fixture invalid JSON".into())
    }
    fn health(origin: &str) -> Result<(), String> {
        let v = request(origin, "GET", "/health", None)?;
        let errors = v
            .get("errors")
            .and_then(Value::as_array)
            .ok_or("fixture health missing errors")?;
        if errors.is_empty() {
            Ok(())
        } else {
            Err(format!("fixture protocol errors: {errors:?}"))
        }
    }
    #[derive(Default)]
    struct Observed {
        created: AtomicU64,
        closed: AtomicU64,
        frames: Mutex<HashSet<rust_cef_runtime::contracts::BrowserId>>,
        error: Mutex<Option<String>>,
    }
    impl BrowserEventSink for Observed {
        fn publish(&self, e: BrowserEventEnvelope) -> PublishResult {
            let error = match e.event {
                BrowserEvent::Created => {
                    self.created.fetch_add(1, Ordering::Relaxed);
                    None
                }
                BrowserEvent::Closed => {
                    self.closed.fetch_add(1, Ordering::Relaxed);
                    None
                }
                BrowserEvent::CreateFailed(e) | BrowserEvent::CaptureFailed(e) => {
                    Some(format!("browser failure: {e:?}"))
                }
                BrowserEvent::LoadFailed {
                    is_main_frame: true,
                    code,
                } => Some(format!("load failure: {code}")),
                BrowserEvent::RendererTerminated { code, .. } => {
                    Some(format!("renderer failure: {code:?}"))
                }
                _ => None,
            };
            if let Some(e) = error {
                self.error.lock().unwrap().get_or_insert(e);
            }
            PublishResult::Published
        }
    }
    impl FrameSink for Observed {
        fn publish(&self, frame: OwnedFrame) -> PublishResult {
            if frame.info.surface != SurfaceKind::View {
                return PublishResult::Rejected;
            }
            let center = (120 * 320 + 160) * 4;
            if frame.info.width == 320
                && frame.info.height == 240
                && frame.cpu_pixels().and_then(|px| px.get(center..center + 4))
                    == Some(&[126, 76, 26, 255])
            {
                self.frames.lock().unwrap().insert(frame.info.browser);
            }
            PublishResult::Published
        }
    }
    impl UiDispatcher for Observed {
        fn wake(&self) -> Result<(), EngineError> {
            Ok(())
        }
    }
    fn tick(rt: &mut Runtime, mode: ThreadMode) -> Result<(), String> {
        if mode == ThreadMode::Integrated {
            rt.pump().map_err(|e| format!("pump: {e:?}"))?;
        }
        thread::sleep(Duration::from_millis(3));
        Ok(())
    }
    fn wait(
        rt: &mut Runtime,
        mode: ThreadMode,
        deadline: Instant,
        mut ready: impl FnMut(&Runtime) -> Result<bool, String>,
    ) -> Result<(), String> {
        loop {
            if ready(rt)? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("scenario timeout".into());
            }
            tick(rt, mode)?;
        }
    }
    fn observation_matches(result: &Value, expected: &Value) -> Result<(bool, bool), String> {
        let cookie = result.get("cookie").ok_or("observation missing cookie")?;
        let local = result
            .get("local_storage")
            .ok_or("observation missing local_storage")?;
        if !(cookie.is_null() || cookie.is_string()) || !(local.is_null() || local.is_string()) {
            return Err("invalid observation value type".into());
        }
        Ok((cookie == expected, local == expected))
    }
    fn storage(extra: &Extra, path: PathBuf) -> ProfileStorage {
        if extra.storage == "Memory" {
            ProfileStorage::Memory
        } else {
            ProfileStorage::Persistent { directory: path }
        }
    }
    fn scenario(
        rt: &mut Runtime,
        opt: &ProbeOptions,
        x: &Extra,
        obs: &Observed,
        report: &mut Value,
    ) -> Result<Vec<ProfileHandle>, String> {
        let deadline = Instant::now() + Duration::from_secs(90);
        wait(rt, opt.mode, deadline, |r| Ok(r.is_ready()))?;
        report["root_lease_active_observed"] = json!(rt.diagnostics().root_lease_held);
        if report["root_lease_active_observed"] != true {
            return Err("root lease was not held while Runtime was active".into());
        }
        let default = rt.default_profile_handle();
        let mut profiles = vec![default];
        for name in ["P1", "P2"] {
            profiles.push(
                rt.create_profile(ProfileConfig {
                    name: name.into(),
                    storage: storage(x, x.root.join(name)),
                })
                .map_err(|e| format!("profile {name}: {e:?}"))?
                .handle,
            );
        }
        for &p in &profiles {
            wait(rt, opt.mode, deadline, |r| {
                match r
                    .profile_state(p)
                    .map_err(|e| format!("profile state: {e:?}"))?
                {
                    ProfileState::Ready => Ok(true),
                    ProfileState::Pending => Ok(false),
                    ProfileState::Failed => Err("profile failed".into()),
                }
            })?;
        }
        let mut browsers = Vec::new();
        let mut tokens = Vec::new();
        for (index, value) in ["D", "P1", "P2"].iter().enumerate() {
            let token = format!("{}-{value}", x.prefix);
            let op = if x.phase == "Write" { "write" } else { "read" };
            health(&x.origin)?;
            let registration = request(
                &x.origin,
                "POST",
                "/register",
                Some(
                    json!({"token": token, "op": op, "value": if op == "write" { *value } else { "" }}),
                ),
            )?;
            let url = registration
                .get("url")
                .and_then(Value::as_str)
                .ok_or("registration missing URL")?;
            if !url.starts_with(&format!("{}/profile-test?", x.origin)) {
                return Err("registration origin mismatch".into());
            }
            browsers.push(
                rt.create_browser(BrowserConfig {
                    initial_url: url.into(),
                    profile: Some(profiles[index]),
                    render_preference: opt.backend,
                    viewport: Viewport {
                        width: 320,
                        height: 240,
                        scale: 1.0,
                    },
                    options: Default::default(),
                })
                .map_err(|e| format!("browser create: {e:?}"))?,
            );
            tokens.push(token);
        }
        for (index, value) in ["D", "P1", "P2"].iter().enumerate() {
            let path = format!("/observation?token={}", tokens[index]);
            let result = loop {
                health(&x.origin)?;
                let result = request(&x.origin, "GET", &path, None)?;
                match result.get("ready").and_then(Value::as_bool) {
                    Some(true) => break result,
                    Some(false) => {}
                    _ => return Err("invalid observation readiness".into()),
                }
                if Instant::now() >= deadline {
                    return Err("observation timeout".into());
                }
                tick(rt, opt.mode)?;
            };
            health(&x.origin)?;
            if result.get("token").and_then(Value::as_str) != Some(tokens[index].as_str()) {
                return Err("observation token mismatch".into());
            }
            let expected = if x.phase == "Read" && x.storage == "Memory" {
                Value::Null
            } else {
                json!(value)
            };
            let (cm, lm) = observation_matches(&result, &expected)?;
            report["storage_checks"].as_array_mut().unwrap().push(json!({"profile_id": profiles[index].local(), "cookie_matches": cm, "local_storage_matches": lm, "passed": cm && lm}));
            if !cm || !lm {
                return Err(format!(
                    "storage mismatch for profile {}",
                    profiles[index].local()
                ));
            }
        }
        wait(rt, opt.mode, deadline, |_| {
            if let Some(e) = obs.error.lock().unwrap().clone() {
                return Err(e);
            }
            let frames = obs.frames.lock().unwrap();
            Ok(browsers.iter().all(|b| {
                b.snapshot().state == SessionState::Alive && frames.contains(&b.snapshot().browser)
            }))
        })?;
        // The last Browser for P1 may close while its Runtime/context remains.
        // Reopen the same profile in this process before testing process restart.
        browsers[1].request_close(CloseMode::Force);
        wait(rt, opt.mode, deadline, |_| {
            Ok(browsers[1].snapshot().state == SessionState::Closed)
        })?;
        let p1_again = rt
            .create_profile(ProfileConfig {
                name: "P1".into(),
                storage: storage(x, x.root.join("P1")),
            })
            .map_err(|e| format!("profile reuse: {e:?}"))?;
        if p1_again.is_new || p1_again.handle != profiles[1] {
            return Err("profile identity was not reused".into());
        }
        let token = format!("{}-P1-reuse", x.prefix);
        let registration = request(
            &x.origin,
            "POST",
            "/register",
            Some(json!({"token":token,"op":"read","value":""})),
        )?;
        let url = registration
            .get("url")
            .and_then(Value::as_str)
            .ok_or("reuse registration missing URL")?;
        if !url.starts_with(&format!("{}/profile-test?", x.origin)) {
            return Err("reuse registration origin mismatch".into());
        }
        let reused = rt
            .create_browser(BrowserConfig {
                initial_url: url.into(),
                profile: Some(p1_again.handle),
                render_preference: opt.backend,
                viewport: Viewport {
                    width: 320,
                    height: 240,
                    scale: 1.0,
                },
                options: Default::default(),
            })
            .map_err(|e| format!("reuse browser: {e:?}"))?;
        let observation = loop {
            health(&x.origin)?;
            let value = request(
                &x.origin,
                "GET",
                &format!("/observation?token={token}"),
                None,
            )?;
            match value.get("ready").and_then(Value::as_bool) {
                Some(true) => break value,
                Some(false) => {}
                None => return Err("reuse observation readiness missing".into()),
            }
            if Instant::now() >= deadline {
                return Err("reuse observation timed out".into());
            }
            tick(rt, opt.mode)?;
        };
        health(&x.origin)?;
        if observation.get("token").and_then(Value::as_str) != Some(token.as_str()) {
            return Err("reuse observation token mismatch".into());
        }
        let expected = if x.phase == "Read" && x.storage == "Memory" {
            Value::Null
        } else {
            json!("P1")
        };
        let (cookie_matches, local_matches) = observation_matches(&observation, &expected)?;
        if !cookie_matches || !local_matches {
            return Err("profile data lost after its last Browser closed".into());
        }
        wait(rt, opt.mode, deadline, |_| {
            Ok(reused.snapshot().state == SessionState::Alive
                && obs
                    .frames
                    .lock()
                    .unwrap()
                    .contains(&reused.snapshot().browser))
        })?;
        report["same_runtime_profile_reused"] = json!(true);
        let gate = opt.observer_gate.as_ref().ok_or("observer gate required")?;
        publish_observer_ready(gate, std::process::id()).map_err(|e| format!("observer: {e}"))?;
        wait(rt, opt.mode, deadline, |_| {
            if [&browsers[0], &browsers[2], &reused]
                .iter()
                .any(|b| b.snapshot().state != SessionState::Alive)
            {
                return Err("browser closed before observer release".into());
            }
            Ok(gate.is_file())
        })?;
        health(&x.origin)?;
        Ok(profiles)
    }
    fn native(opt: &ProbeOptions, x: &Extra, report: &mut Value) -> i32 {
        if opt.scenario != ProbeScenario::Smoke {
            report["error"] = json!("profile restart requires Smoke");
            return 1;
        }
        let host = match std::env::current_exe() {
            Ok(v) => v,
            Err(e) => {
                report["error"] = json!(e.to_string());
                return 1;
            }
        };
        let obs = Arc::new(Observed::default());
        let config = RuntimeConfig {
            thread_mode: opt.mode,
            files: RuntimeFiles {
                directory: host.parent().unwrap().to_path_buf(),
                helper_executable: opt.helper.clone(),
            },
            root_cache_path: x.root.clone(),
            default_profile: storage(x, x.root.clone()),
            content_policy: TrustedContentPolicy {
                allowed_origins: vec![x.origin.clone()],
                allow_embedded_data: true,
            },
            settings: Default::default(),
        };
        let services = RuntimeServices {
            events: obs.clone(),
            frames: obs.clone(),
            ui: obs.clone(),
        };
        let mut rt = match Runtime::start(config, services) {
            Ok(r) => r,
            Err(e) => {
                report["error"] = json!(format!("start: {e:?}"));
                return 1;
            }
        };
        let profiles = match scenario(&mut rt, opt, x, &obs, report) {
            Ok(p) => p,
            Err(e) => {
                report["error"] = json!(e);
                Vec::new()
            }
        };
        let mut tickets = Vec::new();
        for p in profiles {
            match rt.flush_profile_cookies(p) {
                Ok(t) => tickets.push((p.local(), t, false)),
                Err(e) => {
                    if report["error"].is_null() {
                        report["error"] = json!(format!("flush: {e:?}"));
                    }
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        let shutdown = rt.request_shutdown(CloseMode::Force, deadline);
        let mut finished = false;
        loop {
            for (id, ticket, complete) in &mut tickets {
                if !*complete {
                    match ticket.poll() {
                        CookieFlushStatus::Pending => {}
                        CookieFlushStatus::Complete => {
                            *complete = true;
                            report["flush_results"]
                                .as_array_mut()
                                .unwrap()
                                .push(json!({"profile_id": id, "complete": true}));
                        }
                        CookieFlushStatus::Failed(e) => {
                            if report["error"].is_null() {
                                report["error"] = json!(format!("flush failed: {e:?}"));
                            }
                        }
                    }
                }
            }
            match rt.poll_shutdown(&shutdown) {
                ShutdownStatus::Ready => {
                    if tickets.iter().any(|(_, _, done)| !done) {
                        if report["error"].is_null() {
                            report["error"] = json!("shutdown ready before flush completion");
                        }
                    } else {
                        match rt.finish_shutdown() {
                            Ok(_) => finished = true,
                            Err(e) => {
                                if report["error"].is_null() {
                                    report["error"] = json!(format!("finish: {e:?}"));
                                }
                            }
                        }
                        break;
                    }
                }
                ShutdownStatus::Stopped | ShutdownStatus::ProcessExitRequired { .. } => break,
                _ => {}
            }
            if Instant::now() >= deadline {
                break;
            }
            if let Err(e) = tick(&mut rt, opt.mode) {
                if report["error"].is_null() {
                    report["error"] = json!(e);
                }
                break;
            }
        }
        if !finished && report["error"].is_null() {
            report["error"] = json!("shutdown did not drain");
        }
        if let Err(e) = health(&x.origin) {
            if report["error"].is_null() {
                report["error"] = json!(e);
            }
        }
        if let Some(e) = obs.error.lock().unwrap().clone() {
            if report["error"].is_null() {
                report["error"] = json!(e);
            }
        }
        let d = rt.diagnostics();
        report["actual_mode"] = if d.context_thread_id.is_some_and(|id| match opt.mode {
            ThreadMode::Integrated => id == d.main_thread_id,
            ThreadMode::Threaded => id != d.main_thread_id,
        }) {
            json!(opt.mode_name())
        } else {
            Value::Null
        };
        report["actual_backend"] = match d.observed_backend {
            Some(RenderPreference::SoftwareRequired) => json!("Software"),
            Some(RenderPreference::D3d11Required) => json!("D3d11"),
            _ => Value::Null,
        };
        for (key, value) in [
            ("init_thread_id", json!(d.init_thread_id)),
            (
                "cef_ui_thread_id",
                json!(d.cef_ui_thread_id.or(d.context_thread_id)),
            ),
            ("shutdown_thread_id", json!(d.shutdown_thread_id)),
            ("manual_pump_count", json!(d.manual_pump_count)),
            ("tasks_posted", json!(d.tasks.posted)),
            ("tasks_completed", json!(d.tasks.completed)),
            ("task_post_failures", json!(d.tasks.post_failed)),
            ("pending_tasks", json!(d.tasks.pending)),
            ("pending_requests", json!(d.pending_requests)),
            ("pending_profiles", json!(d.pending_profiles)),
            (
                "profile_references_retained",
                json!(d.profile_references_retained),
            ),
            ("cef_shutdown_calls", json!(d.cef_shutdown_calls)),
            (
                "native_calls_after_shutdown",
                json!(d.native_calls_after_shutdown),
            ),
            ("app_reference_retained", json!(d.app_reference_retained)),
            ("single_process_disabled", json!(d.single_process_disabled)),
            ("pending_cookie_flushes", json!(d.pending_cookie_flushes)),
            ("cookie_flushes_accepted", json!(d.cookie_flushes_accepted)),
            (
                "cookie_flushes_completed",
                json!(d.cookie_flushes_completed),
            ),
            ("root_lease_held", json!(d.root_lease_held)),
            ("created", json!(obs.created.load(Ordering::Relaxed))),
            ("closed", json!(obs.closed.load(Ordering::Relaxed))),
            (
                "valid_frame_browsers",
                json!(obs.frames.lock().unwrap().len()),
            ),
        ] {
            report[key] = value;
        }
        if !finished {
            std::mem::forget(rt);
        }
        if finished && report["error"].is_null() {
            0
        } else {
            1
        }
    }
    fn validate_report(report: &Value, opt: &ProbeOptions, main: u64) -> bool {
        let valid = report["requested_mode"] == opt.mode_name()
            && report["actual_mode"] == opt.mode_name()
            && report["requested_backend"] == opt.backend_name()
            && report["actual_backend"] == opt.backend_name()
            && report["init_thread_id"] == main
            && report["shutdown_thread_id"] == main
            && report["cef_ui_thread_id"].as_u64().is_some_and(|id| {
                id != 0
                    && if opt.mode == ThreadMode::Integrated {
                        id == main
                    } else {
                        id != main
                    }
            })
            && if opt.mode == ThreadMode::Integrated {
                report["manual_pump_count"].as_u64().is_some_and(|v| v > 0)
            } else {
                report["manual_pump_count"] == 0
            };
        let zero = [
            "pending_tasks",
            "pending_requests",
            "pending_profiles",
            "profile_references_retained",
            "native_calls_after_shutdown",
            "pending_cookie_flushes",
        ];
        let valid = valid
            && zero.iter().all(|k| report[*k] == 0)
            && report["created"] == 4
            && report["closed"] == 4
            && report["valid_frame_browsers"] == 4
            && report["same_runtime_profile_reused"] == true
            && report["cef_shutdown_calls"] == 1
            && report["cookie_flushes_accepted"] == 3
            && report["cookie_flushes_completed"] == 3
            && report["root_lease_held"] == false
            && report["app_reference_retained"] == false
            && report["single_process_disabled"] == true
            && report["storage_checks"]
                .as_array()
                .is_some_and(|a| a.len() == 3 && a.iter().all(|v| v["passed"] == true))
            && report["flush_results"]
                .as_array()
                .is_some_and(|a| a.len() == 3)
            && report["tasks_posted"].as_u64().is_some_and(|posted| {
                posted > 0
                    && report["tasks_completed"]
                        .as_u64()
                        .zip(report["task_post_failures"].as_u64())
                        .is_some_and(|(done, failed)| done.checked_add(failed) == Some(posted))
            });

        if !valid
            || report["error"] != Value::Null
            || report["exit_code"] != 0
            || report["schema"] != 1
            || report["kind"] != "profile-restart"
            || !report["pid"].as_u64().is_some_and(|pid| pid != 0)
            || report["root_lease_active_observed"] != true
            || report["main_thread_id"] != main
            || !matches!(report["storage"].as_str(), Some("Memory" | "Persistent"))
            || !matches!(report["phase"].as_str(), Some("Write" | "Read"))
        {
            return false;
        }
        for id in [0u64, 1, 2] {
            for (field, flag) in [("storage_checks", "passed"), ("flush_results", "complete")] {
                let Some(rows) = report[field].as_array() else {
                    return false;
                };
                let mut found = rows.iter().filter(|row| row["profile_id"] == id);
                let Some(row) = found.next() else {
                    return false;
                };
                if found.next().is_some() || row[flag] != true {
                    return false;
                }
                if field == "storage_checks"
                    && (row["cookie_matches"] != true || row["local_storage_matches"] != true)
                {
                    return false;
                }
            }
        }
        true
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn incomplete_or_duplicated_restart_evidence_cannot_pass() {
            let mut opt = ProbeOptions::parse(vec![
                "--mode".into(),
                "Integrated".into(),
                "--backend".into(),
                "Software".into(),
                "--helper".into(),
                std::env::temp_dir()
                    .join("helper.exe")
                    .to_string_lossy()
                    .into_owned(),
                "--report".into(),
                std::env::temp_dir()
                    .join("report.json")
                    .to_string_lossy()
                    .into_owned(),
            ])
            .unwrap();
            let mut good = json!({"schema":1,"kind":"profile-restart","storage":"Persistent","phase":"Read","pid":17,"main_thread_id":1,"actual_mode":"Integrated","actual_backend":"Software","init_thread_id":1,"shutdown_thread_id":1,"cef_ui_thread_id":1,"manual_pump_count":1,"created":4,"closed":4,"valid_frame_browsers":4,"same_runtime_profile_reused":true,"tasks_posted":9,"tasks_completed":9,"task_post_failures":0,"cef_shutdown_calls":1,"cookie_flushes_accepted":3,"cookie_flushes_completed":3,"root_lease_held":false,"root_lease_active_observed":true,"app_reference_retained":false,"single_process_disabled":true,"error":null,"exit_code":0});
            for key in [
                "pending_tasks",
                "pending_requests",
                "pending_profiles",
                "profile_references_retained",
                "native_calls_after_shutdown",
                "pending_cookie_flushes",
            ] {
                good[key] = json!(0);
            }
            good["storage_checks"] = json!((0..3).map(|id|json!({"profile_id":id,"cookie_matches":true,"local_storage_matches":true,"passed":true})).collect::<Vec<_>>());
            good["flush_results"] = json!(
                (0..3)
                    .map(|id| json!({"profile_id":id,"complete":true}))
                    .collect::<Vec<_>>()
            );
            good["requested_mode"] = json!("Integrated");
            good["requested_backend"] = json!("Software");
            assert!(validate_report(&good, &opt, 1));
            let mut candidate = good.clone();
            opt.backend = RenderPreference::D3d11Required;
            candidate["requested_backend"] = json!("D3d11");
            assert!(!validate_report(&candidate, &opt, 1));
            candidate["actual_backend"] = json!("D3d11");
            assert!(validate_report(&candidate, &opt, 1));
            opt.backend = RenderPreference::SoftwareRequired;
            assert!(!validate_report(&candidate, &opt, 1));
            for key in [
                "root_lease_active_observed",
                "cookie_flushes_completed",
                "pending_cookie_flushes",
                "exit_code",
            ] {
                let mut missing = good.clone();
                missing.as_object_mut().unwrap().remove(key);
                assert!(!validate_report(&missing, &opt, 1), "{key}");
            }
            for field in ["storage_checks", "flush_results"] {
                let mut duplicate = good.clone();
                duplicate[field][1] = duplicate[field][0].clone();
                assert!(!validate_report(&duplicate, &opt, 1));
            }
            let mut first_error = good.clone();
            first_error["error"] = json!("sticky protocol error");
            assert!(!validate_report(&first_error, &opt, 1));
        }

        #[test]
        fn null_requires_explicit_fields_and_valid_types() {
            assert!(observation_matches(&json!({"cookie": null}), &Value::Null).is_err());
            assert!(
                observation_matches(&json!({"cookie": null, "local_storage": 1}), &Value::Null)
                    .is_err()
            );
            assert_eq!(
                observation_matches(
                    &json!({"cookie": null, "local_storage": null}),
                    &Value::Null
                )
                .unwrap(),
                (true, true)
            );
            assert_eq!(
                observation_matches(&json!({"cookie": "D", "local_storage": "P1"}), &json!("D"))
                    .unwrap(),
                (true, false)
            );
        }
    }
    pub fn run() -> i32 {
        let (opt, x) = match options() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{e}");
                return 2;
            }
        };
        let main = thread_id();
        let mut report = json!({"schema": 1, "kind": "profile-restart", "success": false, "requested_mode": opt.mode_name(), "requested_backend": opt.backend_name(), "actual_mode": null, "actual_backend": null, "pid": std::process::id(), "main_thread_id": main, "storage": x.storage, "phase": x.phase, "root": x.root, "storage_checks": [], "flush_results": [], "error": null});
        for key in [
            "init_thread_id",
            "cef_ui_thread_id",
            "shutdown_thread_id",
            "manual_pump_count",
            "created",
            "closed",
            "valid_frame_browsers",
            "tasks_posted",
            "tasks_completed",
            "task_post_failures",
            "pending_tasks",
            "pending_requests",
            "pending_profiles",
            "profile_references_retained",
            "cef_shutdown_calls",
            "native_calls_after_shutdown",
            "app_reference_retained",
            "single_process_disabled",
            "pending_cookie_flushes",
            "cookie_flushes_accepted",
            "cookie_flushes_completed",
            "root_lease_held",
            "exit_code",
        ] {
            report[key] = Value::Null;
        }
        let mut code = native(&opt, &x, &mut report);
        report["exit_code"] = json!(code);
        let valid = validate_report(&report, &opt, main);
        if code == 0 && !valid {
            if report["error"].is_null() {
                report["error"] = json!("incomplete or inconsistent restart evidence");
            }
            code = 1;
        }
        report["success"] = json!(code == 0 && valid && report["error"].is_null());
        report["exit_code"] = json!(code);
        if let Err(e) = serde_json::to_vec_pretty(&report)
            .map_err(|e| e.to_string())
            .and_then(|bytes| std::fs::write(&opt.report, bytes).map_err(|e| e.to_string()))
        {
            eprintln!("report: {e}");
            return 1;
        }
        code
    }
}
fn main() {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    std::process::exit(windows::run());
    #[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
    {
        eprintln!("native probe requires Windows x64 MSVC");
        std::process::exit(2);
    }
}
