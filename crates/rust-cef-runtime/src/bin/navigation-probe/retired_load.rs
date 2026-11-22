// One real loopback load held across original close and replacement creation.
// The worker's eventual socket outcome is not renderer-delivery evidence.
use super::*;
use rust_cef_runtime::{
    navigation_fixture::SlowWrites,
    ports::{BrowserHandle, CloseResult, CommandId, FrameSource},
};
use serde_json::{Value, json};

const TITLE_SLOW: &str = "Nav Slow";
const EVENT_LIMIT: usize = 256;

#[derive(Clone, Debug, Serialize)]
struct Event {
    browser: [u64; 2],
    sequence: u64,
    name: &'static str,
    code: Option<i32>,
}

fn identity(browser: BrowserId) -> [u64; 2] {
    [browser.runtime().value(), browser.local()]
}

fn command(id: CommandId) -> [u64; 3] {
    let [r, b] = identity(id.browser);
    [r, b, id.sequence]
}

struct Observed {
    slow_url: String,
    events: Mutex<Vec<Event>>,
    overflow: AtomicU64,
    frames_after_closed: AtomicU64,
}

impl Observed {
    fn count(&self) -> usize {
        lock(&self.events).len()
    }
    fn healthy(&self) -> bool {
        self.overflow.load(Ordering::Acquire) == 0
            && self.frames_after_closed.load(Ordering::Acquire) == 0
    }
    fn event(
        &self,
        start: usize,
        browser: BrowserId,
        name: &str,
        code: Option<i32>,
    ) -> Option<u64> {
        lock(&self.events)
            .iter()
            .skip(start)
            .find(|e| e.browser == identity(browser) && e.name == name && e.code == code)
            .map(|e| e.sequence)
    }
}

impl BrowserEventSink for Observed {
    fn publish(&self, e: BrowserEventEnvelope) -> PublishResult {
        let (name, code) = match e.event {
            BrowserEvent::Created => ("created", None),
            BrowserEvent::Closed => ("closed", None),
            BrowserEvent::AddressChanged(url) if url == self.slow_url => ("slow-address", None),
            BrowserEvent::AddressChanged(_) => ("address", None),
            BrowserEvent::TitleChanged(title) if title == TITLE_SLOW => ("slow-title", None),
            BrowserEvent::TitleChanged(title) if title == "Nav A" => ("title-a", None),
            BrowserEvent::TitleChanged(title) if title == "Nav B" => ("title-b", None),
            BrowserEvent::TitleChanged(_) => ("title-other", None),
            BrowserEvent::LoadingChanged { loading: true, .. } => ("loading", None),
            BrowserEvent::LoadingChanged { loading: false, .. } => ("settled", None),
            BrowserEvent::LoadFinished {
                is_main_frame: true,
                http_status_code,
            } => ("main-finished", Some(http_status_code)),
            BrowserEvent::LoadCancelled {
                is_main_frame: true,
                code,
            } => ("main-cancelled", Some(code)),
            BrowserEvent::LoadFailed {
                is_main_frame: true,
                code,
            } => ("main-failed", Some(code)),
            BrowserEvent::CreateFailed(_) => ("create-failed", None),
            BrowserEvent::CaptureFailed(_) => ("capture-failed", None),
            _ => ("other", None),
        };
        let mut events = lock(&self.events);
        if events.len() == EVENT_LIMIT {
            self.overflow.fetch_add(1, Ordering::Release);
            return PublishResult::Rejected;
        }
        events.push(Event {
            browser: identity(e.browser),
            sequence: e.sequence,
            name,
            code,
        });
        PublishResult::Published
    }
}
impl FrameSink for Observed {
    fn publish(&self, f: OwnedFrame) -> PublishResult {
        if lock(&self.events)
            .iter()
            .any(|e| e.name == "closed" && e.browser == identity(f.info.browser))
        {
            self.frames_after_closed.fetch_add(1, Ordering::Release);
            return PublishResult::Rejected;
        }
        PublishResult::Published
    }
}
impl UiDispatcher for Observed {
    fn wake(&self) -> Result<(), EngineError> {
        Ok(())
    }
}

struct Scenario<'a> {
    rt: &'a mut Runtime,
    fixture: &'a NavigationFixture,
    seen: &'a Observed,
    options: &'a ProbeOptions,
    until: Instant,
}

impl Scenario<'_> {
    fn wait(&mut self, label: &str, mut ready: impl FnMut(&Self) -> bool) -> Result<(), String> {
        let deadline = (Instant::now() + PHASE_TIMEOUT).min(self.until);
        loop {
            if let Some(e) = self.fixture.errors().first() {
                return Err(format!("loopback fixture {label}: {e}"));
            }
            if !self.seen.healthy() {
                return Err(format!("retired event/frame ledger {label} failed"));
            }
            if ready(self) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("original load isolation {label} timeout"));
            }
            tick(self.rt, self.options.mode)?;
        }
    }
    fn open(
        &mut self,
        url: &str,
        title: &str,
    ) -> Result<(BrowserHandle, Arc<dyn FrameSource>), String> {
        let h = self
            .rt
            .create_browser(BrowserConfig {
                initial_url: url.into(),
                profile: None,
                render_preference: RenderPreference::SoftwareRequired,
                viewport: Viewport {
                    width: FRAME_WIDTH,
                    height: FRAME_HEIGHT,
                    scale: 1.,
                },
                options: Default::default(),
            })
            .map_err(|e| format!("open original isolation Browser: {e:?}"))?;
        let id = h.snapshot().browser;
        let source = self
            .rt
            .frame_source(id)
            .map_err(|e| format!("original frame source: {e:?}"))?;
        self.wait("initial DOM, browser and VIEW", |_| {
            settled(&h, url, title) && valid_view(source.as_ref(), id)
        })?;
        Ok((h, source))
    }
}

fn valid_view(source: &dyn FrameSource, browser: BrowserId) -> bool {
    source.latest(SurfaceKind::View).is_some_and(|f| {
        f.info.browser == browser
            && f.info.width == FRAME_WIDTH_U32
            && f.info.height == FRAME_HEIGHT_U32
            && f.info.sequence > 0
            && f.info.surface_generation > 0
            && f.pixels
                .get(((120 * 320 + 160) * 4)..((120 * 320 + 160) * 4 + 4))
                == Some(&CENTER_BGRA)
    })
}
fn settled(h: &BrowserHandle, url: &str, title: &str) -> bool {
    let s = h.snapshot();
    s.is_ready()
        && !s.navigation.is_loading
        && s.navigation.address == url
        && s.navigation.title == title
        && s.navigation.main_http_status == Some(200)
        && s.navigation.last_main_frame_error.is_none()
}
fn held(f: &NavigationFixture) -> bool {
    f.slow_request_count() == 1
        && f.request_count(PATH_SLOW) == Some(1)
        && f.slow_writes() == SlowWrites::default()
}
fn go(h: &BrowserHandle, url: &str) -> Result<CommandId, String> {
    h.submit(BrowserCommand::Navigate(url.into()))
        .map_err(|e| format!("navigate original isolation Browser: {e:?}"))
}

fn exercise(s: &mut Scenario<'_>, result: &mut Value) -> Result<[BrowserId; 3], String> {
    s.wait("Runtime readiness", |r| r.rt.is_ready())?;
    let path_a = s.fixture.url(PATH_A)?;
    let path_b = s.fixture.url(PATH_B)?;
    let (a, a_view) = s.open(&path_a, "Nav A")?;
    let (b, b_view) = s.open(&path_b, "Nav B")?;
    let a_id = a.snapshot().browser;
    let b_id = b.snapshot().browser;
    result["original_pair"] = json!([identity(a_id), identity(b_id)]);
    let gate = s
        .options
        .observer_gate
        .as_ref()
        .ok_or("missing observer gate")?
        .clone();
    publish_observer_ready(&gate, std::process::id()).map_err(|e| e.to_string())?;
    s.wait("original helper observation", |_| gate.is_file())?;
    if std::fs::read_to_string(&gate)
        .map_err(|e| e.to_string())?
        .trim()
        != "release"
    {
        return Err("wrong observer release marker".into());
    }
    result["observer_released"] = json!(true);
    let mark = s.seen.count();
    let slow_command = go(&a, &s.seen.slow_url)?;
    s.wait("server holds A's main request", |r| {
        held(r.fixture)
            && a.snapshot().navigation.is_loading
            && r.seen.event(mark, a_id, "loading", None).is_some()
    })?;
    if !settled(&b, &path_b, "Nav B") {
        return Err("B changed while A was loading".into());
    }
    result["slow_cut"] = json!({"command":command(slow_command),"loading_event":s.seen.event(mark,a_id,"loading",None),"requests":1,"writes":0});
    let close = a.request_close(CloseMode::Force);
    s.wait("actual original Closed and retired source", |r| {
        close.result() == CloseResult::Closed
            && a.snapshot().state == SessionState::Closed
            && a_view.snapshot().closed
            && r.seen.event(mark, a_id, "closed", None).is_some()
    })?;
    let closed = s
        .seen
        .event(mark, a_id, "closed", None)
        .ok_or("missing exact A Closed event")?;
    if !held(s.fixture)
        || close.browser != a_id
        || close.sequence == 0
        || !settled(&b, &path_b, "Nav B")
        || !valid_view(b_view.as_ref(), b_id)
    {
        return Err("original request not held or B lost at A Close".into());
    }
    result["original_closed"] = json!({"ticket":[a_id.runtime().value(),a_id.local(),close.sequence],"sequence":closed,"frame_source_closed":true,"b_original_alive":true,"writes":0});
    let (c, c_view) = s.open(&path_a, "Nav A")?;
    let c_id = c.snapshot().browser;
    let c_created = s
        .seen
        .event(0, c_id, "created", None)
        .ok_or("replacement missing actual Created")?;
    if a_id.runtime() != b_id.runtime()
        || a_id.runtime() != c_id.runtime()
        || !(a_id.local() < b_id.local() && b_id.local() < c_id.local())
        || c_created <= closed
        || !held(s.fixture)
        || !settled(&b, &path_b, "Nav B")
    {
        return Err("replacement was not a new live Browser before release".into());
    }
    result["replacement_before_release"] =
        json!({"browser":identity(c_id),"created_event":c_created,"writes":0});
    s.wait("quiet old-handle refusal cut", |r| {
        r.rt.diagnostics().tasks.pending == 0
    })?;
    let before = s.rt.diagnostics().tasks.posted;
    let b_before = b.snapshot();
    let c_before = c.snapshot();
    let navigate = a.submit(BrowserCommand::Navigate(s.seen.slow_url.clone()));
    let reload = a.submit(BrowserCommand::Reload);
    let after = s.rt.diagnostics().tasks.posted;
    if navigate != Err(EngineError::Closed)
        || reload != Err(EngineError::Closed)
        || before == 0
        || before != after
        || !held(s.fixture)
        || a.snapshot().browser != a_id
        || a.snapshot().state != SessionState::Closed
        || !a_view.snapshot().closed
        || b.snapshot() != b_before
        || c.snapshot() != c_before
    {
        return Err(format!(
            "retired A handle did not fail closed at replacement cut: {navigate:?}, {reload:?}, tasks {before}->{after}"
        ));
    }
    result["retired_handle_refused"] = json!({"browser":identity(a_id),"replacement":identity(c_id),
        "navigate":format!("{:?}",navigate.unwrap_err()),"reload":format!("{:?}",reload.unwrap_err()),
        "posted_before":before,"posted_after":after,"slow_requests":s.fixture.slow_request_count(),
        "b_c_snapshots_unchanged":true});
    s.fixture.release_slow();
    s.wait("original HTTP worker returned from write", |r| {
        r.fixture.slow_writes() != SlowWrites::default()
    })?;
    let w = s.fixture.slow_writes();
    result["server_write"] = json!({"released":w.released,"expired":w.expired,"socket_write_ok":w.socket_write_ok,"peer_gone":w.peer_gone,"error":w.error});
    if w.released != 1
        || w.expired != 0
        || w.error != 0
        || w.socket_write_ok + w.peer_gone != 1
        || !settled(&b, &path_b, "Nav B")
        || !settled(&c, &path_a, "Nav A")
    {
        return Err("wrong HTTP release outcome or foreign navigation after release".into());
    }
    let mut receipts = Vec::new();
    for (handle, source, id, url, title, event_name, path) in [
        (
            &b,
            &b_view,
            b_id,
            path_a.as_str(),
            "Nav A",
            "title-a",
            PATH_A,
        ),
        (
            &c,
            &c_view,
            c_id,
            path_b.as_str(),
            "Nav B",
            "title-b",
            PATH_B,
        ),
    ] {
        let mark = s.seen.count();
        let before = s
            .fixture
            .request_count(path)
            .ok_or("unknown fixture path")?;
        let accepted = go(handle, url)?;
        s.wait("fresh own navigation after late response", |r| {
            settled(handle, url, title)
                && valid_view(source.as_ref(), id)
                && r.fixture.request_count(path) == Some(before + 1)
                && r.seen.event(mark, id, event_name, None).is_some()
                && r.seen.event(mark, id, "main-finished", Some(200)).is_some()
        })?;
        receipts.push(json!({"command":command(accepted),"event_floor":mark,"title_event":s.seen.event(mark,id,event_name,None),
            "finish_event":s.seen.event(mark,id,"main-finished",Some(200)),"requests_before":before,"requests_after":before+1}));
        result["survivor_navigations"] = json!(receipts);
    }
    Ok([a_id, b_id, c_id])
}

fn verify_final(seen: &Observed, ids: [BrowserId; 3], evidence: &Value) -> Result<(), String> {
    let events = lock(&seen.events);
    let ids = ids.map(identity);
    let closed = events
        .iter()
        .position(|e| e.browser == ids[0] && e.name == "closed")
        .ok_or("missing A Closed")?;
    if !seen.healthy()
        || events[closed + 1..].iter().any(|e| e.browser == ids[0])
        || events.iter().any(|e| {
            !ids.contains(&e.browser)
                || e.sequence == 0
                || matches!(
                    e.name,
                    "slow-title" | "capture-failed" | "create-failed" | "main-failed"
                )
                || (e.name == "slow-address" && e.browser != ids[0])
                || (e.name == "main-cancelled" && (e.browser != ids[0] || e.code != Some(-3)))
        })
    {
        return Err("retired or foreign Browser event escaped original scope".into());
    }
    for id in ids {
        for name in ["created", "closed"] {
            if events
                .iter()
                .filter(|e| e.browser == id && e.name == name)
                .count()
                != 1
            {
                return Err("non-unique original lifecycle event".into());
            }
        }
    }
    if evidence["survivor_navigations"].as_array().map(Vec::len) != Some(2) {
        return Err("missing survivor receipts".into());
    }
    Ok(())
}

pub(super) fn run(options: &ProbeOptions) -> i32 {
    if options.backend != RenderPreference::SoftwareRequired
        || options.scenario != ProbeScenario::Smoke
        || options.observer_gate.is_none()
    {
        eprintln!(
            "retired-load isolation requires Software and an explicit observer gate; no scenario override"
        );
        return 2;
    }
    let mut proof = json!({"schema":1,"kind":"retired-load-isolation","mode":options.mode_name(),"backend":"Software","pid":std::process::id()});
    let work = (|| -> Result<(), String> {
        let mut fixture = NavigationFixture::new().map_err(|e| e.to_string())?;
        let seen = Arc::new(Observed {
            slow_url: fixture.url(PATH_SLOW)?,
            events: Mutex::new(Vec::with_capacity(EVENT_LIMIT)),
            overflow: AtomicU64::new(0),
            frames_after_closed: AtomicU64::new(0),
        });
        let host = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut rt = Runtime::start(
            RuntimeConfig {
                thread_mode: options.mode,
                files: RuntimeFiles {
                    directory: host.parent().ok_or("missing host parent")?.into(),
                    helper_executable: options.helper.clone(),
                },
                root_cache_path: options
                    .report
                    .parent()
                    .ok_or("missing evidence parent")?
                    .join("cache"),
                default_profile: ProfileStorage::Memory,
                content_policy: TrustedContentPolicy {
                    allowed_origins: vec![fixture.origin().into()],
                    allow_embedded_data: false,
                },
                settings: Default::default(),
            },
            RuntimeServices {
                events: seen.clone(),
                frames: seen.clone(),
                ui: seen.clone(),
            },
        )
        .map_err(|e| format!("start isolation Runtime: {e:?}"))?;
        let case = exercise(
            &mut Scenario {
                rt: &mut rt,
                fixture: &fixture,
                seen: &seen,
                options,
                until: Instant::now() + SCENARIO_BUDGET,
            },
            &mut proof,
        );
        if case.is_err() {
            fixture.release_slow();
        }
        let finished = drain(&mut rt, options.mode);
        let errors = fixture.finish();
        let d = rt.diagnostics();
        proof["events"] = json!(&*lock(&seen.events));
        proof["audit"] = json!({"finished":finished,"main":d.main_thread_id,"init":d.init_thread_id,"ui":d.cef_ui_thread_id,"shutdown_thread":d.shutdown_thread_id,
            "observed_backend":format!("{:?}",d.observed_backend),"pump":d.manual_pump_count,"tasks_posted":d.tasks.posted,"tasks_completed":d.tasks.completed,
            "post_failed":d.tasks.post_failed,"pending_tasks":d.tasks.pending,"pending_profiles":d.pending_profiles,"pending_flush":d.pending_cookie_flushes,
            "pending_requests":d.pending_requests,"profile_refs":d.profile_references_retained,"app_retained":d.app_reference_retained,"root_lease":d.root_lease_held,
            "shutdown":d.cef_shutdown_calls,"native_after":d.native_calls_after_shutdown,"ledger_overflow":seen.overflow.load(Ordering::Acquire),
            "frames_after_closed":seen.frames_after_closed.load(Ordering::Acquire),"fixture_errors":errors});
        if !finished {
            std::mem::forget(rt);
        }
        let ids = case?;
        let thread_ok = d.cef_ui_thread_id.is_some_and(|ui| {
            ui > 0
                && ((options.mode == ThreadMode::Integrated
                    && ui == d.main_thread_id
                    && d.manual_pump_count > 0)
                    || (options.mode == ThreadMode::Threaded
                        && ui != d.main_thread_id
                        && d.manual_pump_count == 0))
        });
        if !finished
            || !errors.is_empty()
            || !thread_ok
            || d.shutdown_thread_id != Some(d.main_thread_id)
            || d.init_thread_id != d.main_thread_id
            || d.observed_backend != Some(RenderPreference::SoftwareRequired)
            || d.tasks.posted == 0
            || d.tasks.posted != d.tasks.completed
            || d.tasks.post_failed != 0
            || d.tasks.pending != 0
            || d.pending_profiles != 0
            || d.pending_cookie_flushes != 0
            || d.pending_requests != 0
            || d.profile_references_retained != 0
            || d.app_reference_retained
            || d.root_lease_held
            || d.cef_shutdown_calls != 1
            || d.native_calls_after_shutdown != 0
            || fixture.slow_request_count() != 1
            || fixture.slow_writes().released != 1
        {
            return Err("incomplete isolation teardown or server cut witness".into());
        }
        verify_final(&seen, ids, &proof)
    })();
    let code = if work.is_ok() { 0 } else { 1 };
    proof["success"] = json!(code == 0);
    proof["error"] = json!(work.err());
    proof["exit_code"] = json!(code);
    match serde_json::to_vec_pretty(&proof)
        .map_err(|e| e.to_string())
        .and_then(|bytes| std::fs::write(&options.report, bytes).map_err(|e| e.to_string()))
    {
        Ok(()) => code,
        Err(e) => {
            eprintln!("isolation report: {e}");
            1
        }
    }
}
