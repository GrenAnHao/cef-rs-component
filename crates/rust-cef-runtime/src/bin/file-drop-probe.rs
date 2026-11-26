//! Windowless file-input and outbound refusal fixtures, not GPUI/Win32 drag acceptance.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[path = "file-drop-probe/outbound.rs"]
mod outbound;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod windows {
    use super::outbound;
    use rust_cef_core::{
        contracts::{
            BrowserConfig, ProfileStorage, RenderPreference, RuntimeConfig, RuntimeFiles,
            ThreadMode, TrustedContentPolicy, Viewport,
        },
        file_drop::{FileDragPhase, FileDropPaths},
        input::{InputEvent, InputKind},
        ports::{
            BrowserCommand, BrowserEvent, BrowserEventEnvelope, BrowserEventSink, BrowserHandle,
            CloseMode, EngineError, FrameSink, MouseModifiers, OwnedFrame, PublishResult,
            UiDispatcher,
        },
    };
    use rust_cef_runtime::{
        runtime::{Runtime, RuntimeServices, ShutdownStatus, StartError},
        test_support::{ProbeOptions, ProbeScenario, publish_observer_ready},
    };
    use serde_json::{Value, json};
    use std::{
        io::Write,
        path::Path,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        time::{Duration, Instant},
    };
    const PAYLOAD: &[u8] = b"owned CEF file drop\n";
    #[derive(Default)]
    pub(super) struct Seen {
        created: AtomicU64,
        closed: AtomicU64,
        frames: AtomicU64,
        blue: AtomicBool,
        error: Mutex<Option<String>>,
    }
    impl BrowserEventSink for Seen {
        fn publish(&self, e: BrowserEventEnvelope) -> PublishResult {
            match e.event {
                BrowserEvent::Created => {
                    self.created.fetch_add(1, Ordering::Relaxed);
                }
                BrowserEvent::Closed => {
                    self.closed.fetch_add(1, Ordering::Relaxed);
                }
                BrowserEvent::CreateFailed(e) | BrowserEvent::CaptureFailed(e) => {
                    self.error.lock().unwrap().get_or_insert(format!("{e:?}"));
                }
                _ => {}
            }
            PublishResult::Published
        }
    }
    impl FrameSink for Seen {
        fn publish(&self, f: OwnedFrame) -> PublishResult {
            self.frames.fetch_add(1, Ordering::Relaxed);
            let center = ((240 / 2) * 320 + 320 / 2) * 4;
            if f.info.width == 320
                && f.info.height == 240
                && f.cpu_pixels().and_then(|px| px.get(center..center + 4))
                    == Some(&[126, 76, 26, 255])
            {
                self.blue.store(true, Ordering::Release);
            }
            PublishResult::Published
        }
    }
    impl UiDispatcher for Seen {
        fn wake(&self) -> Result<(), EngineError> {
            Ok(())
        }
    }
    fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
        let mut f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        f.write_all(bytes).map_err(|e| e.to_string())
    }
    fn page() -> String {
        let html = "<body style='margin:0;height:100vh;background:rgb(26,76,126)'><script>let enters=0;document.title='file-ready';document.addEventListener('dragenter',e=>{e.preventDefault();document.title='enter:'+ ++enters});document.addEventListener('dragover',e=>{e.preventDefault();document.title='over:'+enters});document.addEventListener('dragleave',e=>{document.title='leave:'+enters});document.addEventListener('drop',async e=>{e.preventDefault();let fs=e.dataTransfer.files;if(fs.length!==1){document.title='bad-count';return}let b=new Uint8Array(await fs[0].arrayBuffer()),h=2166136261;for(let x of b){h=Math.imul(h^x,16777619)>>>0}document.title='drop:'+enters+':'+fs.length+':'+b.length+':'+h})</script></body>";
        fixture_url(html)
    }
    pub(super) fn fixture_url(html: &str) -> String {
        let mut url = String::from("data:text/html,");
        for b in html.bytes() {
            if b.is_ascii_alphanumeric() {
                url.push(b as char);
            } else {
                url.push_str(&format!("%{b:02X}"));
            }
        }
        url
    }
    fn tick(rt: &mut Runtime, mode: ThreadMode) -> Result<(), String> {
        if mode == ThreadMode::Integrated {
            rt.pump().map_err(|e| format!("pump: {e:?}"))?;
        }
        std::thread::sleep(Duration::from_millis(3));
        Ok(())
    }
    pub(super) fn wait(
        rt: &mut Runtime,
        mode: ThreadMode,
        seen: &Seen,
        label: &str,
        mut ready: impl FnMut(&Runtime) -> bool,
    ) -> Result<(), String> {
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(e) = seen.error.lock().unwrap().clone() {
                return Err(e);
            }
            if ready(rt) {
                return Ok(());
            }
            if Instant::now() >= until {
                return Err(format!("{label} timeout"));
            }
            tick(rt, mode)?;
        }
    }
    pub(super) fn send(h: &BrowserHandle, kind: InputKind) -> Result<(), String> {
        let epoch = h
            .input_snapshot()
            .map_err(|e| format!("epoch: {e:?}"))?
            .epoch;
        h.submit(BrowserCommand::Input(InputEvent { epoch, kind }))
            .map_err(|e| format!("file input admission: {e:?}"))?;
        Ok(())
    }
    pub fn run() -> i32 {
        let mut args = std::env::args().skip(1).peekable();
        let outbound_only = args.peek().is_some_and(|arg| arg == "--outbound-denial");
        if outbound_only {
            let _ = args.next();
        }
        let options = match ProbeOptions::parse(args) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{e}");
                return 2;
            }
        };
        if !matches!(
            options.backend,
            RenderPreference::SoftwareRequired | RenderPreference::D3d11Required
        ) || options.scenario != ProbeScenario::Smoke
            || options.observer_gate.is_none()
        {
            eprintln!(
                "file-drop-probe requires one explicit mode/required backend and observer gate; no scenario override"
            );
            return 2;
        }
        let directory = options.report.parent().unwrap();
        let file = directory.join("owned drop fixture.txt");
        if !outbound_only && let Err(e) = write_new(&file, PAYLOAD) {
            eprintln!("fixture: {e}");
            return 2;
        }
        let seen = Arc::new(Seen::default());
        let config = RuntimeConfig {
            thread_mode: options.mode,
            files: RuntimeFiles {
                directory: std::env::current_exe().unwrap().parent().unwrap().into(),
                helper_executable: options.helper.clone(),
            },
            root_cache_path: directory.join("cache"),
            default_profile: ProfileStorage::Memory,
            content_policy: TrustedContentPolicy {
                allowed_origins: vec![],
                allow_embedded_data: true,
            },
            settings: Default::default(),
        };
        let mut rt = match Runtime::start(
            config,
            RuntimeServices {
                events: seen.clone(),
                frames: seen.clone(),
                ui: seen.clone(),
            },
        ) {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("start: {e:?}");
                return match e {
                    StartError::InitializeFailed { exit_code }
                    | StartError::ProcessExitRequired { exit_code } => exit_code.unwrap_or(1),
                    _ => 2,
                };
            }
        };
        let payload_hash = PAYLOAD.iter().fold(2166136261u32, |h, b| {
            (h ^ u32::from(*b)).wrapping_mul(16777619)
        });
        let url = if outbound_only {
            outbound::page()
        } else {
            page()
        };
        let mut handle = None;
        let mut outbound_detail = None;
        let mut samples = Vec::<Value>::new();
        let mut terminal_observations = Vec::<Value>::new();
        let mut dropped = false;
        let mut cancelled = false;
        let mut closing_active = false;
        let work = (|| -> Result<(), String> {
            let until = Instant::now() + Duration::from_secs(20);
            while !rt.is_ready() {
                if Instant::now() >= until {
                    return Err("context ready timeout".into());
                }
                tick(&mut rt, options.mode)?;
            }
            let h = rt
                .create_browser(BrowserConfig {
                    initial_url: url.clone(),
                    profile: None,
                    render_preference: options.backend,
                    viewport: Viewport {
                        width: 320,
                        height: 240,
                        scale: 1.,
                    },
                    options: Default::default(),
                })
                .map_err(|e| format!("create: {e:?}"))?;
            handle = Some(h.clone());
            let ready_title = if outbound_only {
                outbound::READY
            } else {
                "file-ready"
            };
            wait(&mut rt, options.mode, &seen, "actual fixture frame", |_| {
                seen.blue.load(Ordering::Acquire)
                    && h.snapshot().navigation.title == ready_title
                    && !h.snapshot().navigation.is_loading
            })?;
            let gate = options.observer_gate.as_ref().unwrap();
            publish_observer_ready(gate, std::process::id()).map_err(|e| e.to_string())?;
            wait(&mut rt, options.mode, &seen, "own child observer", |_| {
                gate.is_file()
            })?;
            h.submit(BrowserCommand::SetFocus(true))
                .map_err(|e| format!("focus: {e:?}"))?;
            if outbound_only {
                outbound_detail = Some(outbound::exercise(&mut rt, options.mode, &seen, &h, &url)?);
                return Ok(());
            }
            let m = MouseModifiers::default();
            for gesture in 1..=3 {
                let paths =
                    FileDropPaths::new(vec![file.clone()]).map_err(|e| format!("paths: {e:?}"))?;
                send(
                    &h,
                    InputKind::FileDragEnter {
                        paths,
                        x: 20,
                        y: 20,
                        modifiers: m,
                    },
                )?;
                wait(&mut rt, options.mode, &seen, "DOM enter", |_| {
                    h.snapshot().navigation.title == format!("enter:{gesture}")
                })?;
                send(
                    &h,
                    InputKind::FileDragOver {
                        x: 20,
                        y: 20,
                        modifiers: m,
                    },
                )?;
                wait(&mut rt, options.mode, &seen, "DOM dragover", |_| {
                    h.snapshot().navigation.title == format!("over:{gesture}")
                })?;
                wait(
                    &mut rt,
                    options.mode,
                    &seen,
                    "confirmed UI Over phase",
                    |_| {
                        h.input_snapshot().is_ok_and(|s| {
                            s.file_drag_phase == FileDragPhase::Over
                                && s.file_drag_active
                                && s.inflight == 0
                                && !s.native_execution_unconfirmed
                        })
                    },
                )?;
                let before = h
                    .input_snapshot()
                    .map_err(|e| format!("input snapshot: {e:?}"))?;
                samples.push(json!({"gesture":gesture,"active_before_terminal":before.file_drag_active,"epoch":before.epoch,"executed":before.executed,"phase":format!("{:?}",before.file_drag_phase),"inflight":before.inflight,"unconfirmed":before.native_execution_unconfirmed}));
                if !before.file_drag_active
                    || before.file_drag_phase != FileDragPhase::Over
                    || before.inflight != 0
                    || before.native_execution_unconfirmed
                {
                    return Err("actual UI drag state absent".into());
                }
                if gesture == 1 {
                    send(
                        &h,
                        InputKind::FileDrop {
                            x: 20,
                            y: 20,
                            modifiers: m,
                        },
                    )?;
                    let title = format!("drop:1:1:{}:{payload_hash}", PAYLOAD.len());
                    wait(
                        &mut rt,
                        options.mode,
                        &seen,
                        "actual file bytes drop",
                        |_| h.snapshot().navigation.title == title,
                    )?;
                    dropped = true;
                } else if gesture == 2 {
                    send(&h, InputKind::FileDragLeave)?;
                    wait(&mut rt, options.mode, &seen, "DOM cancel", |_| {
                        h.snapshot().navigation.title == "leave:2"
                    })?;
                    cancelled = true;
                } else {
                    closing_active = true;
                }
                let observed = h.snapshot();
                terminal_observations.push(json!({"gesture":gesture,"title":observed.navigation.title,"address_unchanged":observed.navigation.address==url}));
                if observed.navigation.address != url {
                    return Err("file drop navigated instead of page acceptance".into());
                }
            }
            Ok(())
        })();
        let mut error = work.err();
        let until = Instant::now() + Duration::from_secs(30);
        let ticket = rt.request_shutdown(CloseMode::Force, until);
        let mut finished = false;
        loop {
            match rt.poll_shutdown(&ticket) {
                ShutdownStatus::Ready => {
                    match rt.finish_shutdown() {
                        Ok(_) => finished = true,
                        Err(e) => {
                            error.get_or_insert(format!("finish: {e:?}"));
                        }
                    };
                    break;
                }
                ShutdownStatus::Blocked { .. } => {
                    error.get_or_insert("shutdown blocked".into());
                    break;
                }
                _ => {}
            }
            if Instant::now() >= until {
                error.get_or_insert("shutdown timeout".into());
                break;
            }
            if let Err(e) = tick(&mut rt, options.mode) {
                error.get_or_insert(e);
                break;
            }
        }
        // A valid early frame/drop must not erase a capture/create error
        // delivered during the closing barrier.
        if let Some(late_error) = seen.error.lock().unwrap().clone() {
            error.get_or_insert(late_error);
        }
        let d = rt.diagnostics();
        let after = handle.as_ref().and_then(|h| h.input_snapshot().ok());
        let retired_denial = if outbound_only {
            handle
                .as_ref()
                .and_then(|h| outbound::receipt(&rt, h.snapshot().browser))
        } else {
            None
        };
        let same_denial_after_close = outbound_detail
            .as_ref()
            .zip(retired_denial.as_ref())
            .is_some_and(|(live, retired)| live["native"] == json!(retired));
        let scenario_ok = if outbound_only {
            same_denial_after_close
                && after
                    .as_ref()
                    .is_some_and(|s| s.pressed_buttons == 0 && s.inflight == 0)
        } else {
            dropped && cancelled && closing_active
        };
        let ok = error.is_none()
            && after.as_ref().is_some_and(|s| {
                !s.file_drag_active
                    && s.file_drag_phase == FileDragPhase::Idle
                    && !s.cleanup_pending
                    && !s.native_execution_unconfirmed
            })
            && scenario_ok
            && finished
            && seen.created.load(Ordering::Relaxed) == 1
            && seen.closed.load(Ordering::Relaxed) == 1
            && d.tasks.pending == 0
            && d.pending_requests == 0
            && d.pending_profiles == 0
            && d.cef_shutdown_calls == 1
            && d.native_calls_after_shutdown == 0
            && !d.app_reference_retained
            && !d.root_lease_held
            && match options.mode {
                ThreadMode::Integrated => d.manual_pump_count > 0,
                ThreadMode::Threaded => d.manual_pump_count == 0,
            }
            && d.observed_backend == Some(options.backend)
            && d.init_thread_id == d.main_thread_id
            && d.shutdown_thread_id == Some(d.main_thread_id)
            && d.cef_ui_thread_id.is_some_and(|ui| match options.mode {
                ThreadMode::Integrated => ui == d.main_thread_id,
                ThreadMode::Threaded => ui != d.main_thread_id,
            })
            && d.tasks.posted == d.tasks.completed;
        let mut report = json!({"kind":"windowless-file-drop-slice","pid":std::process::id(),"success":ok,"error":error,"mode":format!("{:?}",d.mode),"backend":format!("{:?}",options.backend),"observed_backend":format!("{:?}",d.observed_backend),"init_thread":d.init_thread_id,"gpui_or_win32_drag_verified":false,"task_12_3_accepted":false,"file_drop_verified":dropped,"cancel_verified":cancelled,"active_drag_at_shutdown":closing_active,"samples":samples,"terminal_observations":terminal_observations,"input_after_close":after.as_ref().map(|s|json!({"file_drag_active":s.file_drag_active,"phase":format!("{:?}",s.file_drag_phase),"cleanup_pending":s.cleanup_pending,"native_execution_unconfirmed":s.native_execution_unconfirmed})),"created":seen.created.load(Ordering::Relaxed),"closed":seen.closed.load(Ordering::Relaxed),"frames":seen.frames.load(Ordering::Relaxed),"main_thread":d.main_thread_id,"cef_ui_thread":d.cef_ui_thread_id,"shutdown_thread":d.shutdown_thread_id,"manual_pump":d.manual_pump_count,"tasks_posted":d.tasks.posted,"tasks_completed":d.tasks.completed,"pending_tasks":d.tasks.pending,"pending_requests":d.pending_requests,"pending_profiles":d.pending_profiles,"shutdown_calls":d.cef_shutdown_calls,"native_after":d.native_calls_after_shutdown,"app_retained":d.app_reference_retained,"root_lease":d.root_lease_held,"native_calls":d.native_calls,"finished":finished,"payload_bytes":PAYLOAD.len(),"expected_payload_fnv32":payload_hash,"foreground_requested":false});
        if outbound_only {
            report["kind"] = json!("windowless-outbound-drag-denial");
            report["payload_bytes"] = json!(0);
            report["expected_payload_fnv32"] = Value::Null;
            report["outbound_drag"] = json!({
                "live": outbound_detail, "retired_callback_state": retired_denial,
                "physical_input_sent": false, "system_ole_started": false,
                "fixture_file_created": file.exists(),
                "pressed_after_close": after.as_ref().map(|s| s.pressed_buttons),
                "inflight_after_close": after.as_ref().map(|s| s.inflight),
                "profile_references_retained": d.profile_references_retained,
                "pending_cookie_flushes": d.pending_cookie_flushes
            });
        }
        match write_new(
            &options.report,
            serde_json::to_string_pretty(&report).unwrap().as_bytes(),
        ) {
            Ok(_) => {}
            Err(e) => {
                eprintln!("report: {e}");
                return 1;
            }
        }
        if ok { 0 } else { 1 }
    }
}
fn main() {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    std::process::exit(windows::run());
    #[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
    std::process::exit(2);
}
