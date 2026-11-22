//! Public-only dispatch scenario, also used by the passive multi-profile case.
//!
//! This module is deliberately a host. It owns the real GPUI window, both real
//! CEF Browsers, all local strong `Entity` handles and the explicit
//! `UiComposition`. The component crate only receives weak routes and never a
//! pump, timer, wake service, `cx.quit` or process exit.
//!
//! Rules kept here (not in the library):
//!
//! * dispatch/retry runs only on a real UI wake, through `app_update`, never
//!   from a normal `root.update`;
//! * one deliberately nested `root.update -> dispatch` proves the Access
//!   failure path without retiring either route;
//! * capacity/duplicate/wrong-window negatives run before any native frame is
//!   consumed, and the temporary GPUI window is removed again;
//! * every stage is admitted only after real `created_images` and
//!   `painted_sequence` growth plus a real `on_next_frame` completion, not by
//!   reusing an earlier `paint_calls > 0`;
//! * dropped views are represented by cloned diagnostics only; no strong
//!   entity is stored in a report, route table or closure.

use std::num::NonZeroUsize;

use gpui::{WeakEntity, point};
use rust_cef_gpui::{
    CefBrowser, CompositionError, DispatchReport, UiComposition,
    contracts::{BrowserConfig, BrowserId, ProfileConfig, ProfileHandle, SessionState},
    ports::{BrowserCommand, BrowserHandle},
    runtime::ProfileState,
};

use super::*;

const A_BLUE: [u8; 3] = [26, 76, 126];
const A_ORANGE: [u8; 3] = [190, 90, 20];
const B_PURPLE: [u8; 3] = [110, 40, 180];
const B_CYAN: [u8; 3] = [20, 150, 170];
/// Host-side green fallback painted by the parent `HostRoot` once a view is gone.
const DETACHED_GREEN: [u8; 3] = [0, 170, 68];
const STAGES: u32 = 6;
/// Per-stage external-capture bound. The driver allows 20 s per stage.
const STAGE_WAIT: Duration = Duration::from_secs(20);
/// Whole-scenario bound for the six stages; leaves shutdown headroom inside the
/// driver's 120 s process timeout while staying close to the 90 s budget.
const SCENARIO_BUDGET: Duration = Duration::from_secs(75);
const OBSERVER_WAIT: Duration = Duration::from_secs(20);
const CLOSE_WAIT: Duration = Duration::from_secs(10);

/// Minimal fixed solid-colour data page. Only the fixtures above are ever
/// passed in; there is no evaluation, CDP, script or network input. The
/// optional marker makes a re-navigation a distinct exact URI so the host
/// policy admits it and CEF produces a genuinely new frame sequence.
fn data_uri(color: [u8; 3], marker: Option<u8>) -> String {
    let extra = marker.map_or_else(String::new, |value| format!("<!--s{value}-->"));
    let html = format!(
        "<body style=\"margin:0;background:rgb({},{},{})\"></body>{extra}",
        color[0], color[1], color[2]
    );
    let mut uri = String::from("data:text/html,");
    for byte in html.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

fn browser_config(url: String, backend: RenderPreference, scale: f32) -> BrowserConfig {
    BrowserConfig {
        initial_url: url,
        profile: None,
        render_preference: backend,
        viewport: Viewport {
            width: 320,
            height: 240,
            scale,
        },
        options: Default::default(),
    }
}

pub(super) struct ProfilePair {
    pub(super) default: ProfileHandle,
    pub(super) named: ProfileHandle,
    is_same: bool,
    is_sharing: bool,
}

impl ProfilePair {
    pub(super) async fn open(
        cx: &mut AsyncApp,
        rt: &mut Runtime,
        receiver: &UiWakeReceiver,
        stats: &Stats,
        cache_root: &Path,
        deadline: Instant,
    ) -> Result<Self, String> {
        let default = rt.default_profile_handle();
        if rt
            .profile_config(default)
            .map_err(|e| format!("default config: {e:?}"))?
            .storage
            != ProfileStorage::Memory
            || !matches!(rt.profile_state(default), Ok(ProfileState::Ready))
        {
            return Err("default Memory profile is not ready".into());
        }
        let directory = cache_root.join("UiPersistent");
        let registration = rt
            .create_profile(ProfileConfig {
                name: "UiPersistent".into(),
                storage: ProfileStorage::Persistent { directory },
            })
            .map_err(|e| format!("create UI Persistent profile: {e:?}"))?;
        let named = registration.handle;
        if !registration.is_new || named == default || named.runtime() != default.runtime() {
            return Err("UI fixture did not create an independent named profile".into());
        }
        loop {
            stats.check()?;
            match rt.profile_state(named) {
                Ok(ProfileState::Ready) => break,
                Ok(ProfileState::Pending) if Instant::now() < deadline => {}
                other => {
                    return Err(format!(
                        "UI Persistent profile did not become ready: {other:?}"
                    ));
                }
            }
            turn(cx, rt, receiver, false).await?;
        }
        let config = rt
            .profile_config(named)
            .map_err(|e| format!("named profile config: {e:?}"))?;
        if config.name != "UiPersistent"
            || !matches!(config.storage, ProfileStorage::Persistent { .. })
        {
            return Err("named UI profile was not registered as Persistent".into());
        }
        let ticket = rt
            .compare_profiles(default, named)
            .map_err(|e| format!("UI native context comparison: {e:?}"))?;
        let (is_same, is_sharing) = loop {
            stats.check()?;
            match ticket.poll() {
                Some(Ok(comparison)) if !comparison.is_same && !comparison.is_sharing => {
                    break (comparison.is_same, comparison.is_sharing);
                }
                None if Instant::now() < deadline => {}
                observed => {
                    return Err(format!(
                        "distinct non-sharing UI contexts not confirmed: {observed:?}"
                    ));
                }
            }
            turn(cx, rt, receiver, false).await?;
        };
        Ok(Self {
            default,
            named,
            is_same,
            is_sharing,
        })
    }

    pub(super) fn report(
        &self,
        rt: &Runtime,
        a_id: BrowserId,
        b_id: BrowserId,
    ) -> Result<String, String> {
        if !matches!(rt.profile_state(self.default), Ok(ProfileState::Ready))
            || !matches!(rt.profile_state(self.named), Ok(ProfileState::Ready))
            || a_id.runtime() != self.default.runtime()
            || b_id.runtime() != self.named.runtime()
        {
            return Err("UI profiles were not retained through view retirement".into());
        }
        let slot = |browser: BrowserId, profile: ProfileHandle, storage: &str| {
            object(vec![
                (
                    "browser",
                    format!("[{},{}]", browser.runtime().value(), browser.local()),
                ),
                (
                    "configured_profile",
                    format!("[{},{}]", profile.runtime().value(), profile.local()),
                ),
                ("storage", quote(storage)),
                ("ready_after_views", "true".into()),
            ])
        };
        Ok(object(vec![
            ("a", slot(a_id, self.default, "Memory")),
            ("b", slot(b_id, self.named, "Persistent")),
            ("is_same", bool_json(self.is_same)),
            ("is_sharing", bool_json(self.is_sharing)),
            ("native_browser_context_queried", "false".into()),
            ("storage_tested", "false".into()),
        ]))
    }
}

fn atomic_write(path: &Path, text: &str) -> Result<(), String> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("tmp");
    let temporary = path.with_extension(format!("{extension}.tmp"));
    std::fs::write(&temporary, text).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, path).map_err(|error| error.to_string())
}

fn read_diag(
    cx: &mut AsyncApp,
    root: WindowHandle<HostRoot>,
    entity: &Entity<CefBrowser>,
) -> Result<ViewDiagnostics, String> {
    update(cx, root, |_, _, cx| entity.read(cx).diagnostics())
}

/// CEF target identity: f32 multiply, then ceil, as the external capture uses.
fn expected_frame(scale: f32) -> (u32, u32) {
    (
        (320.0_f32 * scale).ceil() as u32,
        (240.0_f32 * scale).ceil() as u32,
    )
}

fn active_ok(diag: &ViewDiagnostics, scale: f32) -> bool {
    let (width, height) = expected_frame(scale);
    diag.last_error.is_none()
        && diag.paint_calls > 0
        && diag.painted_sequence > 0
        && diag.created_images > 0
        && diag.frame_width == width
        && diag.frame_height == height
}

fn released_ok(diag: &ViewDiagnostics) -> bool {
    diag.last_error.is_none()
        && diag.created_images > 0
        && diag.created_images == diag.released_images
}

fn navigation_ready(handle: &BrowserHandle, url: &str) -> bool {
    let snapshot = handle.snapshot();
    snapshot.state == SessionState::Alive
        && !snapshot.navigation.is_loading
        && snapshot.navigation.address == url
}

fn view_json(
    slot: &str,
    browser: BrowserId,
    color: [u8; 3],
    active: bool,
    diag: &ViewDiagnostics,
) -> String {
    object(vec![
        ("slot", quote(slot)),
        (
            "browser",
            format!("[{},{}]", browser.runtime().value(), browser.local()),
        ),
        ("color", format!("[{},{},{}]", color[0], color[1], color[2])),
        ("active", bool_json(active)),
        ("diagnostics", metrics_json(diag)),
    ])
}

fn stage_json(stage: u32, scale: f32, a: String, b: String) -> String {
    object(vec![
        ("schema", "1".into()),
        ("stage", stage.to_string()),
        ("pid", std::process::id().to_string()),
        ("scale_factor", format!("{scale:?}")),
        ("views", format!("[{a},{b}]")),
    ])
}

fn report_json(label: &str, stage: i32, report: &DispatchReport, routes: usize) -> String {
    object(vec![
        ("label", quote(label)),
        ("stage", stage.to_string()),
        ("attempted", report.attempted.to_string()),
        ("updated", report.updated.to_string()),
        ("removed", report.removed.to_string()),
        ("pending", report.pending.to_string()),
        ("errors", report.failures.len().to_string()),
        (
            "failures",
            format!(
                "[{}]",
                report
                    .failures
                    .iter()
                    .map(|failure| {
                        let (kind, message) = match &failure.kind {
                            rust_cef_gpui::DispatchFailureKind::Access { message } => {
                                ("Access", message.clone())
                            }
                            rust_cef_gpui::DispatchFailureKind::View(error) => {
                                ("View", error.to_string())
                            }
                        };
                        object(vec![
                            (
                                "browser",
                                format!(
                                    "[{},{}]",
                                    failure.browser.runtime().value(),
                                    failure.browser.local()
                                ),
                            ),
                            ("kind", quote(kind)),
                            ("message", quote(&message)),
                        ])
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        ),
        ("routes_after", routes.to_string()),
    ])
}

#[derive(Default)]
struct Book {
    calls: u64,
    attempted: u64,
    updated: u64,
    removed: u64,
    pending_sum: u64,
    errors: u64,
    last_pending: usize,
    dropped: u64,
    entries: Vec<String>,
}

impl Book {
    fn record(&mut self, label: &str, stage: i32, report: &DispatchReport, routes: usize) {
        self.calls += 1;
        self.attempted += report.attempted as u64;
        self.updated += report.updated as u64;
        self.removed += report.removed as u64;
        self.pending_sum += report.pending as u64;
        self.errors += report.failures.len() as u64;
        self.last_pending = report.pending;
        if self.entries.len() < 96 {
            self.entries.push(report_json(label, stage, report, routes));
        } else {
            self.dropped += 1;
        }
    }

    fn totals_json(&self) -> String {
        object(vec![
            ("calls", self.calls.to_string()),
            ("attempted", self.attempted.to_string()),
            ("updated", self.updated.to_string()),
            ("removed", self.removed.to_string()),
            ("pending", self.last_pending.to_string()),
            ("pending_total", self.pending_sum.to_string()),
            ("errors", self.errors.to_string()),
            ("entries_dropped", self.dropped.to_string()),
        ])
    }
}

#[derive(Default)]
struct Negative {
    access: u64,
    capacity: u64,
    already_registered: u64,
    wrong_window: u64,
}

struct Checks(Vec<(String, bool)>);

impl Checks {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn add(&mut self, name: impl Into<String>, passed: bool) {
        self.0.push((name.into(), passed));
    }

    fn all(&self) -> bool {
        self.0.iter().all(|(_, passed)| *passed)
    }

    fn json(&self) -> String {
        format!(
            "[{}]",
            self.0
                .iter()
                .map(|(name, passed)| object(vec![
                    ("name", quote(name)),
                    ("passed", bool_json(*passed)),
                ]))
                .collect::<Vec<_>>()
                .join(",")
        )
    }
}

/// Bounded wait whose every turn also runs the single-wake dispatch. Pumping
/// (Integrated) happens inside `turn`, i.e. outside any host GPUI borrow.
#[allow(clippy::too_many_arguments)]
async fn wait_file(
    cx: &mut AsyncApp,
    rt: &mut Runtime,
    receiver: &UiWakeReceiver,
    stats: &Stats,
    out: &mut Outcome,
    composition: &mut UiComposition,
    book: &mut Book,
    stage: i32,
    path: &Path,
    deadline: Instant,
    what: &str,
) -> Result<(), String> {
    while !path.is_file() {
        if Instant::now() >= deadline {
            return Err(format!("{what} timeout"));
        }
        let woke = turn(cx, rt, receiver, false).await?;
        stats.check()?;
        if !woke {
            continue;
        }
        out.wake_consumed += 1;
        let (report, routes) = app_update(cx, |app| {
            let report = composition.dispatch(app);
            let routes = composition.len();
            (report, routes)
        })?;
        book.record("gate-dispatch", stage, &report, routes);
        if !report.failures.is_empty() {
            return Err(format!(
                "unexpected dispatch failures while waiting for {what}: {}",
                report.failures.len()
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handshake(
    cx: &mut AsyncApp,
    rt: &mut Runtime,
    receiver: &UiWakeReceiver,
    stats: &Stats,
    out: &mut Outcome,
    options: &Options,
    composition: &mut UiComposition,
    book: &mut Book,
    stage: u32,
    meta: String,
    deadline: Instant,
) -> Result<(), String> {
    let case = options
        .report
        .parent()
        .ok_or_else(|| "report path has no case directory".to_owned())?;
    write_json(&case.join(format!("dispatch-stage-{stage}.json")), meta)?;
    let pid = std::process::id().to_string();
    let ready = options.gate.with_extension(format!("stage-{stage}.ready"));
    atomic_write(&ready, &pid)?;
    if stage == 0 {
        // The stage-0 frame is the moment the external helper-process observer
        // may start. Same atomic PID file form the launcher already validates.
        atomic_write(&options.gate.with_extension("ready"), &pid)?;
    }
    let release = options
        .gate
        .with_extension(format!("stage-{stage}.release"));
    wait_file(
        cx,
        rt,
        receiver,
        stats,
        out,
        composition,
        book,
        stage as i32,
        &release,
        deadline,
        &format!("dispatch stage {stage} release"),
    )
    .await?;
    let marker = std::fs::read_to_string(&release).map_err(|error| error.to_string())?;
    if marker.trim() != pid {
        return Err(format!("dispatch stage {stage} release PID mismatch"));
    }
    Ok(())
}

/// Waits until a dropped view entity is really gone. The transient strong
/// handle from `upgrade` is used only inside this predicate and never stored.
async fn wait_released(
    cx: &mut AsyncApp,
    root: WindowHandle<HostRoot>,
    weak: &WeakEntity<CefBrowser>,
    deadline: Instant,
    what: &str,
) -> Result<(), String> {
    while weak.upgrade().is_some() {
        if Instant::now() >= deadline {
            return Err(format!(
                "{what} entity was not released by a real GPUI frame"
            ));
        }
        rendered(cx, root).await?;
    }
    Ok(())
}

/// One stage gate. A stage becomes visible only after a new image was both
/// created and really painted (`created_images` and `painted_sequence` grew),
/// plus a real navigation completion where one was submitted.
#[allow(clippy::too_many_arguments)]
async fn frame_barrier(
    cx: &mut AsyncApp,
    rt: &mut Runtime,
    receiver: &UiWakeReceiver,
    stats: &Stats,
    out: &mut Outcome,
    root: WindowHandle<HostRoot>,
    composition: &mut UiComposition,
    a: Option<&Entity<CefBrowser>>,
    b: Option<&Entity<CefBrowser>>,
    base_a: (u64, u64),
    base_b: (u64, u64),
    need_a: bool,
    need_b: bool,
    need_removed: bool,
    a_nav: Option<(&BrowserHandle, &str)>,
    b_nav: Option<(&BrowserHandle, &str)>,
    stage: i32,
    deadline: Instant,
    book: &mut Book,
) -> Result<(Option<ViewDiagnostics>, Option<ViewDiagnostics>, bool), String> {
    if need_a && a.is_none() {
        return Err(format!("stage {stage} requires a live A view"));
    }
    if need_b && b.is_none() {
        return Err(format!("stage {stage} requires a live B view"));
    }
    let mut removed_seen = !need_removed;
    loop {
        if Instant::now() >= deadline {
            return Err(format!("stage {stage} frame target timeout"));
        }
        // A real GPUI completion makes the last consumed image visible. It runs
        // before the wake wait so an image already consumed by the recovery
        // retry can be painted without requiring another CEF frame.
        rendered(cx, root).await?;
        let a_diag = match a {
            Some(entity) => Some(read_diag(cx, root, entity)?),
            None => None,
        };
        let b_diag = match b {
            Some(entity) => Some(read_diag(cx, root, entity)?),
            None => None,
        };
        let a_done = !need_a
            || a_diag.as_ref().is_some_and(|diag| {
                diag.created_images > base_a.0 && diag.painted_sequence > base_a.1
            });
        let b_done = !need_b
            || b_diag.as_ref().is_some_and(|diag| {
                diag.created_images > base_b.0 && diag.painted_sequence > base_b.1
            });
        let a_nav_ok = a_nav.is_none_or(|(handle, url)| navigation_ready(handle, url));
        let b_nav_ok = b_nav.is_none_or(|(handle, url)| navigation_ready(handle, url));
        let routes = composition.len();
        let removed_ok = !need_removed || (removed_seen && routes == 1);
        if a_done && b_done && a_nav_ok && b_nav_ok && removed_ok {
            if !need_removed && routes != 2 {
                return Err(format!(
                    "stage {stage} lost routes before A was removed: {routes}"
                ));
            }
            return Ok((a_diag, b_diag, removed_seen));
        }

        let woke = turn(cx, rt, receiver, true).await?;
        stats.check()?;
        if !woke {
            continue;
        }
        out.wake_consumed += 1;
        let (report, routes) = app_update(cx, |app| {
            let report = composition.dispatch(app);
            let routes = composition.len();
            (report, routes)
        })?;
        book.record("dispatch", stage, &report, routes);
        if !report.failures.is_empty() {
            return Err(format!(
                "unexpected dispatch failures at stage {stage}: {}",
                report.failures.len()
            ));
        }
        if report.removed > 0 {
            if !need_removed || routes == 0 || routes > 1 {
                return Err(format!(
                    "unexpected route removal at stage {stage}: removed {} routes, {routes} left",
                    report.removed
                ));
            }
            removed_seen = true;
        }
    }
}

pub(super) async fn body(
    cx: &mut AsyncApp,
    rt: &mut Runtime,
    receiver: &UiWakeReceiver,
    stats: &Stats,
    root: WindowHandle<HostRoot>,
    options: &Options,
    out: &mut Outcome,
) -> Result<(), String> {
    let started = Instant::now();
    let scenario_deadline = started + SCENARIO_BUDGET;
    let ready_deadline = (started + Duration::from_secs(25)).min(scenario_deadline);
    while rt.state() != RuntimeState::Ready {
        if Instant::now() >= ready_deadline {
            return Err("Runtime readiness timeout".into());
        }
        turn(cx, rt, receiver, false).await?;
        stats.check()?;
    }

    let scale = update(cx, root, |_, window, _| window.scale_factor())?;
    if !scale.is_finite() || scale <= 0.0 {
        return Err(format!("invalid host window scale {scale}"));
    }

    let profiles = if options.scenario == HostScenario::ProfileDispatch {
        let root = options
            .report
            .parent()
            .ok_or("missing report directory")?
            .join("cache");
        Some(ProfilePair::open(cx, rt, receiver, stats, &root, ready_deadline).await?)
    } else {
        None
    };
    let mut a_config = browser_config(data_uri(A_BLUE, None), options.backend, scale);
    let mut b_config = browser_config(data_uri(B_PURPLE, None), options.backend, scale);
    if let Some(profiles) = &profiles {
        a_config.profile = Some(profiles.default);
        b_config.profile = Some(profiles.named);
    }
    let a_handle = rt
        .create_browser(a_config)
        .map_err(|error| format!("create A: {error:?}"))?;
    let b_handle = rt
        .create_browser(b_config)
        .map_err(|error| format!("create B: {error:?}"))?;
    let a_id = a_handle.snapshot().browser;
    let b_id = b_handle.snapshot().browser;
    if a_id == b_id {
        return Err("Runtime returned the same BrowserId for both browsers".into());
    }
    out.browser = Some(a_id);
    out.additional_browsers = vec![b_id];

    let a_attachment = rt
        .attach_browser(&a_handle)
        .map_err(|error| format!("attach A: {error:?}"))?;
    let b_attachment = rt
        .attach_browser(&b_handle)
        .map_err(|error| format!("attach B: {error:?}"))?;
    let attach_duplicate_refused =
        rt.attach_browser(&a_handle).is_err() && rt.attach_browser(&b_handle).is_err();
    if !attach_duplicate_refused {
        return Err("duplicate Runtime attachment was accepted".into());
    }

    let (a_entity, b_entity) = update(cx, root, move |host, window, cx| {
        let a = CefBrowser::new(a_attachment, window, cx)?;
        let b = CefBrowser::new(b_attachment, window, cx)?;
        host.browser = Some(a.clone());
        host.secondary = Some(b.clone());
        host.multi = true;
        host.detached = false;
        host.secondary_detached = false;
        cx.notify();
        Ok::<_, rust_cef_gpui::ViewError>((a, b))
    })?
    .map_err(|error| format!("view construction: {error:?}"))?;

    // ---------------------------------------------------------------------
    // Expected negatives, all before any native frame is consumed.
    // ---------------------------------------------------------------------
    let base_consumed = (
        read_diag(cx, root, &a_entity)?.consumed_updates,
        read_diag(cx, root, &b_entity)?.consumed_updates,
    );

    let (capacity_a, capacity_b, capacity_routes) = update(cx, root, |_, window, cx| {
        let mut router = UiComposition::new(NonZeroUsize::new(1).expect("nonzero capacity"));
        let a = router.register(&a_entity, window, &*cx);
        let b = router.register(&b_entity, window, &*cx);
        (a, b, router.len())
    })?;
    if capacity_a.is_err() {
        return Err(format!(
            "capacity-one router rejected A: {:?}",
            capacity_a.err()
        ));
    }
    if !matches!(
        &capacity_b,
        Err(CompositionError::Capacity { capacity }) if *capacity == 1
    ) {
        return Err(format!(
            "capacity-one router did not report Capacity for B: {:?}",
            capacity_b.as_ref().err()
        ));
    }
    if capacity_routes != 1 {
        return Err(format!(
            "capacity-one router left {capacity_routes} routes after Capacity"
        ));
    }

    let mut composition = UiComposition::new(NonZeroUsize::new(2).expect("nonzero capacity"));
    let (register_a, register_b, duplicate, main_routes) = update(cx, root, |_, window, cx| {
        let a = composition.register(&a_entity, window, &*cx);
        let b = composition.register(&b_entity, window, &*cx);
        let duplicate = composition.register(&a_entity, window, &*cx);
        (a, b, duplicate, composition.len())
    })?;
    if let Err(error) = register_a {
        return Err(format!("main composition rejected A: {error:?}"));
    }
    if let Err(error) = register_b {
        return Err(format!("main composition rejected B: {error:?}"));
    }
    let duplicate_refused = matches!(
        &duplicate,
        Err(CompositionError::AlreadyRegistered { browser }) if *browser == a_id
    );
    if !duplicate_refused {
        return Err(format!(
            "main composition did not refuse duplicate A: {:?}",
            duplicate.as_ref().err()
        ));
    }
    if main_routes != 2 {
        return Err(format!(
            "main composition has {main_routes} routes after duplicate refusal"
        ));
    }

    // A separate hidden GPUI window is a real foreign window. The view must be
    // rejected by its fixed binding before the Source is read and before a slot
    // is reserved; the temporary window is then removed without touching the
    // original host window.
    let temporary = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(240.), px(160.)),
                ))),
                show: false,
                focus: false,
                ..Default::default()
            },
            |_, cx| {
                cx.new(|_| HostRoot {
                    browser: None,
                    secondary: None,
                    multi: false,
                    secondary_detached: false,
                    fault_label: None,
                    menu: None,
                    print_confirmation: None,
                    extent: (320., 240.),
                    renders: 0,
                    detached: false,
                })
            },
        )
        .map_err(|error| format!("temporary window: {error}"))?;
    let (wrong_window, wrong_routes) = temporary
        .update(cx, |_, window, cx| {
            let mut router = UiComposition::new(NonZeroUsize::new(1).expect("nonzero capacity"));
            let result = router.register(&a_entity, window, &*cx);
            (
                matches!(result, Err(CompositionError::WrongWindow { .. })),
                router.len(),
            )
        })
        .map_err(|error| format!("temporary window update: {error}"))?;
    temporary
        .update(cx, |_, window, _| window.remove_window())
        .map_err(|error| format!("temporary window removal: {error}"))?;
    if !wrong_window {
        return Err("foreign-window registration was not rejected as WrongWindow".into());
    }
    if wrong_routes != 0 {
        return Err(format!(
            "foreign-window rejection still reserved {wrong_routes} routes"
        ));
    }
    let after_negative = (
        read_diag(cx, root, &a_entity)?.consumed_updates,
        read_diag(cx, root, &b_entity)?.consumed_updates,
    );
    if after_negative != base_consumed || after_negative != (0, 0) {
        return Err(format!(
            "negative checks consumed native frames: before {base_consumed:?}, after {after_negative:?}"
        ));
    }

    let mut book = Book::default();
    let mut negative = Negative {
        capacity: 1,
        already_registered: 1,
        wrong_window: 1,
        ..Negative::default()
    };
    let mut checks = Checks::new();
    checks.add("runtime_ready", true);
    checks.add("capacity_one_accepts_a", true);
    checks.add("capacity_one_refuses_b_capacity", true);
    checks.add("main_registers_a_and_b", true);
    checks.add("duplicate_registration_refused", true);
    checks.add("foreign_window_rejected", true);
    checks.add("foreign_window_reserves_no_route", true);
    checks.add("negative_checks_consume_no_frames", true);
    checks.add("duplicate_attachment_refused", attach_duplicate_refused);

    // One deliberately nested root.update -> dispatch. The outer update holds
    // the window temporarily, so both route updates fail as Access. Failures
    // are evidence here, not closure: neither route may be removed or retired.
    let (nested, nested_routes) = update(cx, root, |_, _, cx| {
        let report = composition.dispatch(&mut *cx);
        let routes = composition.len();
        (report, routes)
    })?;
    book.record("nested-access", -1, &nested, nested_routes);
    let nested_access = nested
        .failures
        .iter()
        .filter(|failure| {
            matches!(
                failure.kind,
                rust_cef_gpui::DispatchFailureKind::Access { .. }
            )
        })
        .count();
    negative.access += nested_access as u64;
    if nested.attempted != 2
        || nested.updated != 0
        || nested.removed != 0
        || nested.pending != 2
        || nested.failures.len() != 2
        || nested_access != 2
    {
        return Err(format!(
            "nested dispatch did not produce Access/pending 2: attempted {} updated {} removed {} pending {} failures {}",
            nested.attempted,
            nested.updated,
            nested.removed,
            nested.pending,
            nested.failures.len()
        ));
    }
    if composition.len() != 2 {
        return Err("nested Access failure retired a route".into());
    }
    checks.add("nested_access_pending_two", true);
    checks.add("nested_access_keeps_routes", true);

    // Recover only on a real GPUI completion, then run the explicit bounded
    // retry. Routes stay two; no Err is interpreted as a closed view.
    rendered(cx, root).await?;
    let (retry, retry_routes) = app_update(cx, |app| {
        let report = composition.retry_pending(app);
        let routes = composition.len();
        (report, routes)
    })?;
    book.record("retry-recovery", -1, &retry, retry_routes);
    if !retry.failures.is_empty() || retry.removed != 0 || retry.pending != 0 || retry_routes != 2 {
        return Err(format!(
            "retry recovery failed: attempted {} updated {} removed {} pending {} failures {} routes {}",
            retry.attempted,
            retry.updated,
            retry.removed,
            retry.pending,
            retry.failures.len(),
            retry_routes
        ));
    }
    checks.add("ui_frame_then_retry_recovered", true);
    checks.add("retry_keeps_two_routes", true);

    let stage_limit = |now: Instant| -> Instant { (now + STAGE_WAIT).min(scenario_deadline) };

    // ---------------------------------------------------------------------
    // Stage 0: initial blue/purple, both views active.
    // ---------------------------------------------------------------------
    let (a0, b0, _) = frame_barrier(
        cx,
        rt,
        receiver,
        stats,
        out,
        root,
        &mut composition,
        Some(&a_entity),
        Some(&b_entity),
        (0, 0),
        (0, 0),
        true,
        true,
        false,
        None,
        None,
        0,
        stage_limit(Instant::now()),
        &mut book,
    )
    .await?;
    let (a0, b0) = (
        a0.ok_or_else(|| "stage 0 missing A diagnostics".to_owned())?,
        b0.ok_or_else(|| "stage 0 missing B diagnostics".to_owned())?,
    );
    let stage0_ok = active_ok(&a0, scale) && active_ok(&b0, scale);
    checks.add("stage0_views", stage0_ok);
    if !stage0_ok {
        return Err(format!("stage 0 views are not painted: {a0:?} {b0:?}"));
    }
    handshake(
        cx,
        rt,
        receiver,
        stats,
        out,
        options,
        &mut composition,
        &mut book,
        0,
        stage_json(
            0,
            scale,
            view_json("a", a_id, A_BLUE, true, &a0),
            view_json("b", b_id, B_PURPLE, true, &b0),
        ),
        stage_limit(Instant::now()),
    )
    .await?;
    rendered(cx, root).await?;

    // ---------------------------------------------------------------------
    // Stage 1: A orange, B stays purple.
    // ---------------------------------------------------------------------
    let a_current = read_diag(cx, root, &a_entity)?;
    let b_current = read_diag(cx, root, &b_entity)?;
    let orange = data_uri(A_ORANGE, Some(1));
    a_handle
        .submit(BrowserCommand::Navigate(orange.clone()))
        .map_err(|error| format!("navigate A orange: {error:?}"))?;
    let (a1, b1, _) = frame_barrier(
        cx,
        rt,
        receiver,
        stats,
        out,
        root,
        &mut composition,
        Some(&a_entity),
        Some(&b_entity),
        (a_current.created_images, a_current.painted_sequence),
        (b_current.created_images, b_current.painted_sequence),
        true,
        false,
        false,
        Some((&a_handle, orange.as_str())),
        None,
        1,
        stage_limit(Instant::now()),
        &mut book,
    )
    .await?;
    let (a1, b1) = (
        a1.ok_or_else(|| "stage 1 missing A diagnostics".to_owned())?,
        b1.ok_or_else(|| "stage 1 missing B diagnostics".to_owned())?,
    );
    let stage1_ok = active_ok(&a1, scale) && active_ok(&b1, scale);
    checks.add("stage1_views", stage1_ok);
    if !stage1_ok {
        return Err(format!("stage 1 views are not painted: {a1:?} {b1:?}"));
    }
    handshake(
        cx,
        rt,
        receiver,
        stats,
        out,
        options,
        &mut composition,
        &mut book,
        1,
        stage_json(
            1,
            scale,
            view_json("a", a_id, A_ORANGE, true, &a1),
            view_json("b", b_id, B_PURPLE, true, &b1),
        ),
        stage_limit(Instant::now()),
    )
    .await?;
    rendered(cx, root).await?;

    // ---------------------------------------------------------------------
    // Stage 2: B cyan, A stays orange.
    // ---------------------------------------------------------------------
    let a_current = read_diag(cx, root, &a_entity)?;
    let b_current = read_diag(cx, root, &b_entity)?;
    let cyan = data_uri(B_CYAN, Some(2));
    b_handle
        .submit(BrowserCommand::Navigate(cyan.clone()))
        .map_err(|error| format!("navigate B cyan: {error:?}"))?;
    let (a2, b2, _) = frame_barrier(
        cx,
        rt,
        receiver,
        stats,
        out,
        root,
        &mut composition,
        Some(&a_entity),
        Some(&b_entity),
        (a_current.created_images, a_current.painted_sequence),
        (b_current.created_images, b_current.painted_sequence),
        false,
        true,
        false,
        None,
        Some((&b_handle, cyan.as_str())),
        2,
        stage_limit(Instant::now()),
        &mut book,
    )
    .await?;
    let (a2, b2) = (
        a2.ok_or_else(|| "stage 2 missing A diagnostics".to_owned())?,
        b2.ok_or_else(|| "stage 2 missing B diagnostics".to_owned())?,
    );
    let stage2_ok = active_ok(&a2, scale) && active_ok(&b2, scale);
    checks.add("stage2_views", stage2_ok);
    if !stage2_ok {
        return Err(format!("stage 2 views are not painted: {a2:?} {b2:?}"));
    }
    handshake(
        cx,
        rt,
        receiver,
        stats,
        out,
        options,
        &mut composition,
        &mut book,
        2,
        stage_json(
            2,
            scale,
            view_json("a", a_id, A_ORANGE, true, &a2),
            view_json("b", b_id, B_CYAN, true, &b2),
        ),
        stage_limit(Instant::now()),
    )
    .await?;
    rendered(cx, root).await?;

    // ---------------------------------------------------------------------
    // Stage 3: A back to blue, B stays cyan.
    // ---------------------------------------------------------------------
    let a_current = read_diag(cx, root, &a_entity)?;
    let b_current = read_diag(cx, root, &b_entity)?;
    let blue_again = data_uri(A_BLUE, Some(3));
    a_handle
        .submit(BrowserCommand::Navigate(blue_again.clone()))
        .map_err(|error| format!("navigate A blue: {error:?}"))?;
    let (a3, b3, _) = frame_barrier(
        cx,
        rt,
        receiver,
        stats,
        out,
        root,
        &mut composition,
        Some(&a_entity),
        Some(&b_entity),
        (a_current.created_images, a_current.painted_sequence),
        (b_current.created_images, b_current.painted_sequence),
        true,
        false,
        false,
        Some((&a_handle, blue_again.as_str())),
        None,
        3,
        stage_limit(Instant::now()),
        &mut book,
    )
    .await?;
    let (a3, b3) = (
        a3.ok_or_else(|| "stage 3 missing A diagnostics".to_owned())?,
        b3.ok_or_else(|| "stage 3 missing B diagnostics".to_owned())?,
    );
    let stage3_ok = active_ok(&a3, scale) && active_ok(&b3, scale);
    checks.add("stage3_views", stage3_ok);
    if !stage3_ok {
        return Err(format!("stage 3 views are not painted: {a3:?} {b3:?}"));
    }
    handshake(
        cx,
        rt,
        receiver,
        stats,
        out,
        options,
        &mut composition,
        &mut book,
        3,
        stage_json(
            3,
            scale,
            view_json("a", a_id, A_BLUE, true, &a3),
            view_json("b", b_id, B_CYAN, true, &b3),
        ),
        stage_limit(Instant::now()),
    )
    .await?;
    rendered(cx, root).await?;

    // ---------------------------------------------------------------------
    // Stage 4: release A's image, remove it from the host tree, drop the local
    // strong handle and keep the main route weak. B is re-navigated to a new
    // exact cyan URI; dispatch must prune A (removed >= 1, routes == 1) while B
    // gets a genuinely new frame.
    // ---------------------------------------------------------------------
    let a_before_unload = read_diag(cx, root, &a_entity)?;
    out.paint = Some(a_before_unload);
    let b_current = read_diag(cx, root, &b_entity)?;
    let a_released = update(cx, root, |host, window, cx| {
        let released = a_entity.update(cx, |view, view_cx| view.release_image(window, view_cx));
        host.browser = None;
        host.detached = true;
        cx.notify();
        released?;
        Ok::<_, rust_cef_gpui::ViewError>(a_entity.read(cx).diagnostics())
    })?
    .map_err(|error| format!("release A: {error:?}"))?;
    if !released_ok(&a_released) {
        return Err(format!(
            "A release diagnostics are not balanced: {a_released:?}"
        ));
    }
    out.released_view = Some(a_released.clone());
    let a_weak = a_entity.downgrade();
    drop(a_entity);
    let cyan_four = data_uri(B_CYAN, Some(4));
    b_handle
        .submit(BrowserCommand::Navigate(cyan_four.clone()))
        .map_err(|error| format!("navigate B cyan after A removal: {error:?}"))?;
    wait_released(cx, root, &a_weak, stage_limit(Instant::now()), "A").await?;
    let (a4, b4, removed_seen) = frame_barrier(
        cx,
        rt,
        receiver,
        stats,
        out,
        root,
        &mut composition,
        None,
        Some(&b_entity),
        (0, 0),
        (b_current.created_images, b_current.painted_sequence),
        false,
        true,
        true,
        None,
        Some((&b_handle, cyan_four.as_str())),
        4,
        stage_limit(Instant::now()),
        &mut book,
    )
    .await?;
    if a4.is_some() {
        return Err("stage 4 unexpectedly still exposes A diagnostics".into());
    }
    let b4 = b4.ok_or_else(|| "stage 4 missing B diagnostics".to_owned())?;
    if !removed_seen || composition.len() != 1 {
        return Err(format!(
            "weak A route was not pruned: removed_seen {removed_seen}, routes {}",
            composition.len()
        ));
    }
    checks.add("a_entity_released", a_weak.upgrade().is_none());
    checks.add("weak_route_pruned_a", true);
    if !active_ok(&b4, scale) {
        return Err(format!("stage 4 B view is not painted: {b4:?}"));
    }
    checks.add("stage4_b_active_after_a_removed", true);

    if b_handle.snapshot().state != SessionState::Alive {
        return Err(format!(
            "B is not Alive after A removal: {:?}",
            b_handle.snapshot().state
        ));
    }
    checks.add("b_alive_after_a_removed", true);

    let close_deadline = stage_limit(Instant::now());
    while !matches!(
        a_handle.snapshot().state,
        SessionState::Closing | SessionState::Closed
    ) {
        if Instant::now() >= close_deadline {
            break;
        }
        let woke = turn(cx, rt, receiver, false).await?;
        stats.check()?;
        if woke {
            out.wake_consumed += 1;
            let (report, routes) = app_update(cx, |app| {
                let report = composition.dispatch(app);
                let routes = composition.len();
                (report, routes)
            })?;
            book.record("close-observe", 4, &report, routes);
            if !report.failures.is_empty() {
                return Err(format!(
                    "dispatch failed while observing A close: {}",
                    report.failures.len()
                ));
            }
        }
    }
    let a_close_observed = matches!(
        a_handle.snapshot().state,
        SessionState::Closing | SessionState::Closed
    );
    checks.add("a_close_observed", a_close_observed);

    // The host window itself must still be alive at stage 4, with the green A
    // fallback and the live B view.
    let host_state = update(cx, root, |host, _, _| {
        (
            host.browser.is_none(),
            host.detached,
            host.secondary.is_some(),
            host.multi,
        )
    })?;
    let stage4_host_ok = host_state == (true, true, true, true);
    checks.add("stage4_host_keeps_b", stage4_host_ok);
    if !stage4_host_ok {
        return Err(format!(
            "stage 4 host tree is not A-removed/B-live: {host_state:?}"
        ));
    }
    handshake(
        cx,
        rt,
        receiver,
        stats,
        out,
        options,
        &mut composition,
        &mut book,
        4,
        stage_json(
            4,
            scale,
            view_json("a", a_id, DETACHED_GREEN, false, &a_released),
            view_json("b", b_id, B_CYAN, true, &b4),
        ),
        stage_limit(Instant::now()),
    )
    .await?;
    rendered(cx, root).await?;

    // ---------------------------------------------------------------------
    // Stage 5: release B, unregister its only route, drop its local handle.
    // ---------------------------------------------------------------------
    let renders_before = update(cx, root, |host, _, _| host.renders)?;
    let b_released = update(cx, root, |host, window, cx| {
        let released = b_entity.update(cx, |view, view_cx| view.release_image(window, view_cx));
        host.secondary = None;
        host.secondary_detached = true;
        cx.notify();
        released?;
        Ok::<_, rust_cef_gpui::ViewError>(b_entity.read(cx).diagnostics())
    })?
    .map_err(|error| format!("release B: {error:?}"))?;
    if !released_ok(&b_released) {
        return Err(format!(
            "B release diagnostics are not balanced: {b_released:?}"
        ));
    }
    if !composition.unregister(b_id) {
        return Err("final B unregister did not find its route".into());
    }
    if !composition.is_empty() {
        return Err(format!(
            "composition still has {} routes after final unregister",
            composition.len()
        ));
    }
    let b_weak = b_entity.downgrade();
    drop(b_entity);
    // A WeakEntity may already be gone before the newly notified HostRoot has
    // rendered. Entity destruction is not proof of host presentation: await an
    // actual frame unconditionally before checking renders or publishing green.
    rendered(cx, root).await?;
    wait_released(cx, root, &b_weak, stage_limit(Instant::now()), "B").await?;
    let (host_slots_empty, host_detached, host_secondary_detached, host_renders) =
        update(cx, root, |host, _, _| {
            (
                host.browser.is_none() && host.secondary.is_none(),
                host.detached,
                host.secondary_detached,
                host.renders,
            )
        })?;
    let host_alive = host_slots_empty
        && host_detached
        && host_secondary_detached
        && host_renders > renders_before;
    checks.add("b_entity_released", b_weak.upgrade().is_none());
    checks.add("host_alive_after_both_views", host_alive);
    if !host_alive {
        return Err(format!(
            "host window did not remain alive after both views: slots_empty {host_slots_empty} renders {renders_before}->{host_renders}"
        ));
    }
    let stage5_ok = released_ok(&a_released) && released_ok(&b_released);
    checks.add("stage5_views", stage5_ok);
    if !stage5_ok {
        return Err("stage 5 released diagnostics are not balanced".into());
    }
    handshake(
        cx,
        rt,
        receiver,
        stats,
        out,
        options,
        &mut composition,
        &mut book,
        5,
        stage_json(
            5,
            scale,
            view_json("a", a_id, DETACHED_GREEN, false, &a_released),
            view_json("b", b_id, DETACHED_GREEN, false, &b_released),
        ),
        stage_limit(Instant::now()),
    )
    .await?;
    rendered(cx, root).await?;

    // The external launcher releases the original observer gate in its finally
    // block only after the six stage captures. Do not return before it.
    wait_file(
        cx,
        rt,
        receiver,
        stats,
        out,
        &mut composition,
        &mut book,
        5,
        &options.gate,
        Instant::now() + OBSERVER_WAIT,
        "observer release",
    )
    .await?;
    checks.add("observer_release_observed", true);

    if !a_close_observed {
        // A later dispatch/frame may have completed the original close request.
        // This stays an observation, never an inferred closed state.
        let late = Instant::now() + CLOSE_WAIT;
        while !matches!(
            a_handle.snapshot().state,
            SessionState::Closing | SessionState::Closed
        ) && Instant::now() < late
        {
            turn(cx, rt, receiver, false).await?;
            stats.check()?;
        }
    }
    let a_close_observed = matches!(
        a_handle.snapshot().state,
        SessionState::Closing | SessionState::Closed
    );
    out.view_release_observed_closing = a_close_observed;
    checks.add("a_close_observed_final", a_close_observed);

    stats.check()?;
    let created = stats.created.load(Ordering::Acquire);
    let closed = stats.closed.load(Ordering::Acquire);
    let valid_frames = stats.valid_frames.load(Ordering::Acquire);
    let borrow_violations = stats.borrow_violations.load(Ordering::Acquire);
    let image_a_ok =
        a_released.created_images >= 3 && a_released.created_images == a_released.released_images;
    let image_b_ok =
        b_released.created_images >= 2 && b_released.created_images == b_released.released_images;
    checks.add("exactly_two_browsers_created", created == 2);
    checks.add("no_borrow_violations", borrow_violations == 0);
    checks.add("valid_blue_frame_observed", valid_frames > 0);
    checks.add("a_image_count_balanced", image_a_ok);
    checks.add("b_image_count_balanced", image_b_ok);
    checks.add("expected_access_count", negative.access == 2);
    checks.add("expected_capacity_count", negative.capacity == 1);
    checks.add(
        "expected_already_registered_count",
        negative.already_registered == 1,
    );
    checks.add("expected_wrong_window_count", negative.wrong_window == 1);
    checks.add("final_route_table_empty", composition.is_empty());
    checks.add("stages_completed", true);
    checks.add(
        "released_a_matches_before_paint",
        out.paint
            .as_ref()
            .is_some_and(|paint| paint.paint_calls > 0)
            && out.released_view.as_ref().is_some_and(released_ok),
    );

    let profiles = profiles
        .as_ref()
        .map(|pair| pair.report(rt, a_id, b_id))
        .transpose()?;
    let success = checks.all() && image_a_ok && image_b_ok && a_close_observed && host_alive;
    let detail = object(vec![
        ("success", bool_json(success)),
        ("profiles", profiles.unwrap_or_else(|| "null".into())),
        ("stages", STAGES.to_string()),
        ("checks", checks.json()),
        ("reports", format!("[{}]", book.entries.join(","))),
        ("route_reports", book.totals_json()),
        (
            "views",
            format!(
                "[{},{}]",
                metrics_json(&a_released),
                metrics_json(&b_released)
            ),
        ),
        (
            "image_counts",
            object(vec![
                (
                    "a",
                    object(vec![
                        ("created", a_released.created_images.to_string()),
                        ("released", a_released.released_images.to_string()),
                        ("minimum", "3".into()),
                    ]),
                ),
                (
                    "b",
                    object(vec![
                        ("created", b_released.created_images.to_string()),
                        ("released", b_released.released_images.to_string()),
                        ("minimum", "2".into()),
                    ]),
                ),
            ]),
        ),
        (
            "expected_negative",
            object(vec![
                ("access", negative.access.to_string()),
                ("capacity", negative.capacity.to_string()),
                (
                    "already_registered",
                    negative.already_registered.to_string(),
                ),
                ("wrong_window", negative.wrong_window.to_string()),
            ]),
        ),
        (
            "negative_consumed_updates",
            object(vec![
                ("a_before", base_consumed.0.to_string()),
                ("a_after", after_negative.0.to_string()),
                ("b_before", base_consumed.1.to_string()),
                ("b_after", after_negative.1.to_string()),
            ]),
        ),
        ("paint", metrics_json(&a_released)),
        ("created", created.to_string()),
        ("closed", closed.to_string()),
        ("valid_frames", valid_frames.to_string()),
        ("dispatch_calls", book.calls.to_string()),
        ("borrow_violations", borrow_violations.to_string()),
    ]);
    out.dispatch_detail = Some(detail);
    out.duplicate_refused = attach_duplicate_refused && duplicate_refused;
    out.detached_alive = host_alive;
    out.view_release_observed_closing = a_close_observed;

    if !success {
        return Err("10.2 dispatch validation checks failed".into());
    }
    Ok(())
}
