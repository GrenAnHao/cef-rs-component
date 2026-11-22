//! Validation-only recovery exhaustion through actual GPUI layout and desktop observation.
use super::pointer_probe::{PointerProbe, WindowInfo};
use super::*;
use rust_cef_gpui::{
    UiComposition,
    contracts::SessionState,
    ports::{BrowserCommand, BrowserHandle, CaptureFailureKind},
    runtime::FrameRecoveryDiagnostics,
    test_support::{FrameProbeControl, probe_frame_sha256},
};
use std::num::NonZeroUsize;

pub(super) const STALE_LABEL: &str = "STALE: resize recovery exhausted";
const WAIT: Duration = Duration::from_secs(20);
const QUIET: Duration = Duration::from_millis(250);
const MANUAL_WAIT: Duration = Duration::from_secs(15);
const MANUAL_SPAN: Duration = Duration::from_millis(150);

// Exact owner, DPI and client geometry of the original fixture throughout the
// readiness gate and subsequent captures; never query another window's data.
#[derive(PartialEq, Eq)]
struct ManualOwner {
    hwnd: u64,
    pid: u32,
    thread: u32,
    dpi: u32,
    client: (i32, i32, u32, u32),
}

impl ManualOwner {
    fn sample(probe: &PointerProbe) -> Result<Self, String> {
        Ok(Self::from_info(&probe.info()?, probe.owner_thread()?))
    }

    fn from_info(info: &WindowInfo, thread: u32) -> Self {
        Self {
            hwnd: info.hwnd,
            pid: info.pid,
            thread,
            dpi: info.dpi,
            client: (info.left, info.top, info.width, info.height),
        }
    }
}

fn sustained_foreground(
    since: &mut Option<Instant>,
    sample_started: Instant,
    sample_returned: Instant,
    foreground: bool,
) -> bool {
    if !foreground {
        *since = None;
        return false;
    }
    // First sample is no later than its return, last no earlier than entry.
    // Time spent inside a query cannot increase the proven sample spacing.
    sample_started.saturating_duration_since(*since.get_or_insert(sample_returned)) >= MANUAL_SPAN
}

#[derive(Default)]
pub(super) struct InitialForeground {
    before: Option<bool>,
    attempts: u8,
    after: Option<bool>,
    manual_owner: Option<ManualOwner>,
}
impl InitialForeground {
    pub(super) fn require_original_manual(
        &self,
        info: &WindowInfo,
        thread: u32,
        probe: &PointerProbe,
    ) -> Result<(), String> {
        super::visibility::require_unlocked_session()?;
        if self.manual_owner.as_ref() != Some(&ManualOwner::from_info(info, thread))
            || !probe.is_foreground()?
        {
            return Err("original manual foreground or client geometry no longer qualified".into());
        }
        Ok(())
    }

    pub(super) fn json(&self, options: &Options) -> String {
        object(vec![
            ("approved", bool_json(options.initial_foreground_approved)),
            ("attempts", self.attempts.to_string()),
            ("before", self.before.map_or("null".into(), bool_json)),
            ("after", self.after.map_or("null".into(), bool_json)),
            (
                "policy",
                quote(
                    if options.exhaustion_manual_foreground
                        || options.visibility_manual_foreground
                        || options.resize_manual_foreground
                    {
                        "manual-owned-observation"
                    } else if options.initial_foreground_approved {
                        "one-ordinary-owned-request"
                    } else {
                        "passive"
                    },
                ),
            ),
        ])
    }
}
#[derive(Default)]
pub(super) struct ExpectedFault {
    browser: Option<BrowserId>,
    armed: bool,
    count: u64,
    event_sequence: u64,
    retired: bool,
}
impl ExpectedFault {
    pub(super) fn arm(&mut self, browser: BrowserId) -> Result<(), String> {
        if self.browser.is_some() || self.retired {
            return Err("fault window cannot be rearmed".into());
        }
        self.browser = Some(browser);
        self.armed = true;
        Ok(())
    }
    pub(super) fn observe(
        &mut self,
        browser: BrowserId,
        sequence: u64,
        error: &EngineError,
    ) -> bool {
        if !self.armed
            || self.retired
            || self.browser != Some(browser)
            || self.count != 0
            || sequence == 0
            || !matches!(
                error,
                EngineError::Capture {
                    kind: CaptureFailureKind::ResizeExhausted,
                    code: None
                }
            )
        {
            return false;
        }
        self.armed = false;
        self.count = 1;
        self.event_sequence = sequence;
        true
    }
    pub(super) fn count(&self) -> u64 {
        self.count
    }
    pub(super) fn retire(&mut self) -> Result<(), String> {
        if self.count != 1 || self.armed || self.retired {
            return Err("fault window retirement requires exactly one event".into());
        }
        self.retired = true;
        Ok(())
    }
    pub(super) fn json(&self) -> String {
        object(vec![
            ("count", self.count.to_string()),
            ("browser", self.browser.map_or("null".into(), bid)),
            ("event_sequence", self.event_sequence.to_string()),
            ("retired", bool_json(self.retired)),
            (
                "kind",
                if self.count == 1 {
                    quote("ResizeExhausted")
                } else {
                    "null".into()
                },
            ),
            ("code", "null".into()),
        ])
    }
}
fn bid(id: BrowserId) -> String {
    format!("[{},{}]", id.runtime().value(), id.local())
}
fn nums(values: &[u64]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}
fn recovery_json(r: &FrameRecoveryDiagnostics) -> String {
    object(vec![
        ("status", quote(r.status)),
        ("generation", r.generation.to_string()),
        ("expected_width", r.expected_width.to_string()),
        ("expected_height", r.expected_height.to_string()),
        ("attempts", r.attempts.to_string()),
        ("requests_total", r.requests_total.to_string()),
        (
            "native_invalidate_entries",
            r.native_invalidate_entries.to_string(),
        ),
        ("native_thread", r.native_thread.to_string()),
        ("native_offsets_us", nums(&r.native_offsets_us)),
        ("request_offsets_us", nums(&r.request_offsets_us)),
        ("acks", r.acks.to_string()),
        ("exhaustions", r.exhaustions.to_string()),
        ("queued", bool_json(r.queued)),
        ("post_failures", r.post_failures.to_string()),
    ])
}
fn pristine_idle(r: &FrameRecoveryDiagnostics) -> bool {
    r.status == "Idle"
        && r.generation == 0
        && r.expected_width == 0
        && r.expected_height == 0
        && r.attempts == 0
        && r.requests_total == 0
        && r.native_invalidate_entries == 0
        && r.native_thread == 0
        && r.native_offsets_us.is_empty()
        && r.request_offsets_us.is_empty()
        && r.acks == 0
        && r.exhaustions == 0
        && !r.queued
        && r.post_failures == 0
        && r.probe_dropped == 0
        && r.incomplete_rejected == 0
}
fn offsets_ok(v: &[u64]) -> bool {
    v.len() == 8
        && v.windows(2)
            .all(|w| w[1].checked_sub(w[0]).is_some_and(|d| d >= 100_000))
}
fn exhausted(
    r: &FrameRecoveryDiagnostics,
    base: &FrameRecoveryDiagnostics,
    ui: u64,
    target: (u32, u32),
) -> bool {
    r.status == "Exhausted"
        && r.attempts == 8
        && !r.queued
        && r.post_failures == 0
        && ui > 0
        && r.native_thread == ui
        && r.requests_total.checked_sub(base.requests_total) == Some(8)
        && r.native_invalidate_entries
            .checked_sub(base.native_invalidate_entries)
            == Some(8)
        && r.acks == base.acks
        && r.exhaustions.checked_sub(base.exhaustions) == Some(1)
        && (r.expected_width, r.expected_height) == target
        && offsets_ok(&r.native_offsets_us)
        && offsets_ok(&r.request_offsets_us)
}
pub(super) fn page() -> String {
    recovery_page(false)
}

fn recovery_page(marked: bool) -> String {
    let marker = if marked {
        "<div style='position:fixed;left:16px;top:16px;width:8px;height:8px;background:rgb(17,34,51)'></div>"
    } else {
        ""
    };
    let text = format!(
        "<html><title>Controlled recovery</title><body style='margin:0;background:rgb(26,76,126)'><div style='position:fixed;left:0;top:0;width:4px;height:100%;background:rgb(190,90,20)'></div><div style='position:fixed;left:0;top:0;width:100%;height:4px;background:rgb(110,40,180)'></div>{marker}</body></html>"
    );
    let mut uri = "data:text/html,".to_string();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            uri.push(b as char)
        } else {
            uri.push_str(&format!("%{b:02X}"))
        }
    }
    uri
}
fn target(extent: (i32, i32), scale: f32) -> (u32, u32) {
    (
        (extent.0 as f32 * scale).ceil() as u32,
        (extent.1 as f32 * scale).ceil() as u32,
    )
}
fn original_target_recovered(
    exhausted: &FrameRecoveryDiagnostics,
    now: &FrameRecoveryDiagnostics,
) -> bool {
    now.status == "Recovered"
        && now.generation == exhausted.generation
        && now.expected_width == exhausted.expected_width
        && now.expected_height == exhausted.expected_height
        && now.acks.checked_sub(exhausted.acks) == Some(1)
        && now.exhaustions == exhausted.exhaustions
        && now.requests_total == exhausted.requests_total
        && now.native_invalidate_entries == exhausted.native_invalidate_entries
        && now.post_failures == 0
        && !now.queued
}

fn contains_recovery_marker(frame: &OwnedFrame, scale: f32) -> bool {
    let (x, y) = target((20, 20), scale);
    if x >= frame.info.width || y >= frame.info.height {
        return false;
    }
    let offset = ((u64::from(y) * u64::from(frame.info.width) + u64::from(x)) * 4) as usize;
    frame.pixels.as_slice().get(offset..offset + 4) == Some(&[51, 34, 17, 255])
}

fn atomic(path: &Path, text: &str) -> Result<(), String> {
    let temp = path.with_extension("pending");
    std::fs::write(&temp, text).map_err(|e| e.to_string())?;
    std::fs::rename(temp, path).map_err(|e| e.to_string())
}
struct Run<'a> {
    cx: &'a mut AsyncApp,
    rt: &'a mut Runtime,
    receiver: &'a UiWakeReceiver,
    stats: &'a Stats,
    root: WindowHandle<HostRoot>,
    options: &'a Options,
    out: &'a mut Outcome,
    composition: UiComposition,
    probe: &'a PointerProbe,
    id: BrowserId,
    scale: f32,
    deadline: Instant,
    phase: &'static str,
    baseline: Option<FrameRecoveryDiagnostics>,
    letterbox: Option<super::exhaustion_evidence::LetterboxCut>,
}
impl Run<'_> {
    fn check_letterboxes(&self, retired_failures: Option<u64>) -> Result<usize, String> {
        let Some(cut) = &self.letterbox else {
            return Ok(0);
        };
        let events = self.stats.capture_events.lock().unwrap().clone();
        cut.verify(&events, &self.control_state()?, retired_failures)
    }
    async fn step(&mut self) -> Result<(), String> {
        let result = async {
            if turn(self.cx, self.rt, self.receiver, false).await? {
                self.out.wake_consumed += 1;
                let r = app_update(self.cx, |app| self.composition.dispatch(app))?;
                if r.removed != 0 || !r.failures.is_empty() {
                    return Err(format!("exhaustion dispatch: {r:?}"));
                }
            }
            self.stats.check()?;
            self.check_letterboxes(None)?;
            Ok::<_, String>(())
        }
        .await;
        // CaptureFailed is normally noticed here: snapshot before returning
        // the Err and unwinding the caller's Entity/retained-frame locals.
        if let Err(original) = &result {
            super::exhaustion_evidence::capture_once(
                self.cx,
                self.rt,
                self.stats,
                self.root,
                self.options,
                self.out,
                self.phase,
                self.baseline.as_ref(),
                original,
            );
        }
        result
    }
    fn rec(&self) -> Result<FrameRecoveryDiagnostics, String> {
        self.rt
            .frame_recovery_diagnostics(self.id)
            .map_err(|e| format!("recovery snapshot: {e:?}"))
    }
    fn ui(&self) -> Result<u64, String> {
        self.rt
            .diagnostics()
            .cef_ui_thread_id
            .filter(|v| *v > 0)
            .ok_or("missing actual CEF UI thread".into())
    }
    fn control_state(&self) -> Result<rust_cef_gpui::test_support::FrameProbeState, String> {
        self.rt
            .probe_frame_state(self.id)
            .ok_or("missing probe state".into())
    }
    async fn control(&mut self, drop: bool) -> Result<(), String> {
        let before = self.control_state()?.applied;
        self.rt
            .probe_frame_control(self.id, FrameProbeControl::DropView(drop))
            .map_err(|e| format!("control post: {e:?}"))?;
        let until = (Instant::now() + WAIT).min(self.deadline);
        loop {
            let p = self.control_state()?;
            if p.applied == before.checked_add(1).ok_or("probe applied overflow")?
                && p.drop_view == drop
                && !p.incomplete_pending
            {
                return Ok(());
            }
            if Instant::now() >= until {
                return Err(format!("control application timeout: {p:?}"));
            }
            self.step().await?;
        }
    }
    async fn frame(
        &mut self,
        entity: &Entity<CefBrowser>,
        handle: &BrowserHandle,
        minimum_generation: u64,
        extent: (i32, i32),
        marked_recovery: bool,
    ) -> Result<(Arc<OwnedFrame>, ViewDiagnostics), String> {
        let until = (Instant::now() + WAIT).min(self.deadline);
        let mut last = String::new();
        loop {
            if Instant::now() >= until {
                return Err(format!("recovery complete-frame timeout: {last}"));
            }
            rendered(self.cx, self.root).await?;
            let (d, scale) = update(self.cx, self.root, |_, w, app| {
                (entity.read(app).diagnostics(), w.scale_factor())
            })?;
            let s = self
                .rt
                .frame_source(self.id)
                .map_err(|e| format!("source: {e:?}"))?
                .snapshot();
            let state = handle.snapshot();
            let r = self.rec()?;
            last = format!("{} {}", metrics_json(&d), recovery_json(&r));
            if let Some(f) = s.view.frame.as_ref() {
                let size = target(extent, self.scale);
                if scale == self.scale
                    && state.state == SessionState::Alive
                    && !state.navigation.is_loading
                    && !s.closed
                    && s.view.visible
                    && s.viewport
                        == Some(Viewport {
                            width: extent.0,
                            height: extent.1,
                            scale: self.scale,
                        })
                    && s.view.generation == f.info.surface_generation
                    && f.info.surface_generation > minimum_generation
                    && f.info.browser == self.id
                    && f.info.surface == SurfaceKind::View
                    && (f.info.width, f.info.height) == size
                    && (d.frame_width, d.frame_height) == size
                    && d.painted_generation == f.info.surface_generation
                    && d.painted_sequence == f.info.sequence
                    && d.surface_bounds == (0., 48., extent.0 as f32, extent.1 as f32)
                    && (!marked_recovery || contains_recovery_marker(f, self.scale))
                    && d.scale_factor == self.scale
                    && d.paint_calls > 0
                    && d.last_error.is_none()
                    && !d.pending_resync
                    && !r.queued
                    && r.post_failures == 0
                    && ((r.status == "Recovered"
                        && r.generation == f.info.surface_generation
                        && (r.expected_width, r.expected_height) == size)
                        || (minimum_generation == 0 && pristine_idle(&r)))
                {
                    return Ok((f.clone(), d));
                }
            }
            self.step().await?;
        }
    }
    async fn capture(
        &mut self,
        stage: u32,
        d: &ViewDiagnostics,
        frame: Option<&OwnedFrame>,
        baseline: &FrameRecoveryDiagnostics,
        quiet_ms: u64,
    ) -> Result<(), String> {
        let case = self
            .options
            .report
            .parent()
            .ok_or("exhaustion case missing")?;
        let info = self.probe.info()?;
        let thread = self.probe.owner_thread()?;
        if let Some(owner) = self.out.initial_foreground.manual_owner.as_ref() {
            super::visibility::require_unlocked_session()?;
            if ManualOwner::from_info(&info, thread) != *owner || !self.probe.is_foreground()? {
                return Err(
                    "manual Exhaustion lost the original foreground/geometry before capture".into(),
                );
            }
        }
        let (label, extent, scale) = update(self.cx, self.root, |host, w, _| {
            (host.fault_label, host.extent, w.scale_factor())
        })?;
        if scale != self.scale {
            return Err("physical scale changed during exhaustion slice".into());
        }
        let source = self
            .rt
            .frame_source(self.id)
            .map_err(|e| format!("capture source: {e:?}"))?
            .snapshot();
        let p = self.control_state()?;
        let rec = self.rec()?;
        let raw = if let Some(f) = frame {
            let name = format!("exhaustion-stage-{stage}-view.bgra");
            std::fs::write(case.join(&name), f.pixels.as_slice()).map_err(|e| e.to_string())?;
            object(vec![
                ("file", quote(&name)),
                ("browser", bid(f.info.browser)),
                ("surface", quote(&format!("{:?}", f.info.surface))),
                ("width", f.info.width.to_string()),
                ("height", f.info.height.to_string()),
                ("bytes", f.pixels.len().to_string()),
                ("generation", f.info.surface_generation.to_string()),
                ("sequence", f.info.sequence.to_string()),
            ])
        } else {
            "null".into()
        };
        let text = object(vec![
            ("schema", "1".into()),
            ("kind", quote("recovery-exhaustion-slice")),
            ("scope", quote("recovery-exhaustion-slice")),
            ("input_sent", bool_json(false)),
            (
                "initial_foreground",
                self.out.initial_foreground.json(self.options),
            ),
            ("foreground_forced", bool_json(false)),
            ("observer_resized", bool_json(false)),
            ("pid", std::process::id().to_string()),
            ("stage", stage.to_string()),
            ("browser", bid(self.id)),
            (
                "window",
                object(vec![
                    ("hwnd", info.hwnd.to_string()),
                    ("pid", info.pid.to_string()),
                    ("thread", thread.to_string()),
                    ("dpi", info.dpi.to_string()),
                    ("left", info.left.to_string()),
                    ("top", info.top.to_string()),
                    ("width", info.width.to_string()),
                    ("height", info.height.to_string()),
                ]),
            ),
            ("scale_factor", self.scale.to_string()),
            ("view_bounds", format!("[0,48,{},{}]", extent.0, extent.1)),
            ("diagnostics", metrics_json(d)),
            ("view_raw", raw),
            ("stale", bool_json(stage == 1)),
            ("fault_label", label.map_or("null".into(), quote)),
            ("recovery", recovery_json(&rec)),
            ("baseline_recovery", recovery_json(baseline)),
            ("fault", self.stats.fault.lock().unwrap().json()),
            (
                "probe",
                object(vec![
                    ("drop_view", bool_json(p.drop_view)),
                    ("applied", p.applied.to_string()),
                    ("dropped", p.dropped.to_string()),
                    ("incomplete_pending", bool_json(p.incomplete_pending)),
                    ("incomplete_rejected", p.incomplete_rejected.to_string()),
                ]),
            ),
            ("quiet_observation_ms", quiet_ms.to_string()),
            ("source_generation", source.view.generation.to_string()),
            ("detached", bool_json(stage == 3)),
            ("task_11_3_accepted", bool_json(false)),
        ]);
        atomic(&case.join(format!("exhaustion-stage-{stage}.json")), &text)?;
        if stage == 0 {
            atomic(
                &self.options.gate.with_extension("ready"),
                &std::process::id().to_string(),
            )?;
        }
        atomic(
            &self
                .options
                .gate
                .with_extension(format!("exhaustion-{stage}.ready")),
            &std::process::id().to_string(),
        )?;
        let release = self
            .options
            .gate
            .with_extension(format!("exhaustion-{stage}.release"));
        let until = (Instant::now() + WAIT).min(self.deadline);
        while !release.is_file() {
            if Instant::now() >= until {
                return Err(format!("exhaustion stage {stage} observer timeout"));
            }
            self.step().await?;
        }
        if std::fs::read_to_string(release)
            .map_err(|e| e.to_string())?
            .trim()
            != std::process::id().to_string()
        {
            return Err("exhaustion stage release PID mismatch".into());
        }
        Ok(())
    }
}
#[allow(clippy::too_many_arguments)]
async fn inner(
    cx: &mut AsyncApp,
    rt: &mut Runtime,
    receiver: &UiWakeReceiver,
    stats: &Stats,
    root: WindowHandle<HostRoot>,
    options: &Options,
    out: &mut Outcome,
    probe: &PointerProbe,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(100);
    while rt.state() != RuntimeState::Ready {
        if Instant::now() >= deadline {
            return Err("exhaustion Runtime readiness timeout".into());
        }
        turn(cx, rt, receiver, false).await?;
        stats.check()?;
    }
    let scale = update(cx, root, |_, w, _| w.scale_factor())?;
    if !scale.is_finite() || scale <= 0. {
        return Err("invalid actual scale".into());
    }
    let handle = rt
        .create_browser(BrowserConfig {
            initial_url: page(),
            profile: None,
            render_preference: options.backend,
            viewport: Viewport {
                width: 320,
                height: 240,
                scale,
            },
            options: Default::default(),
        })
        .map_err(|e| format!("exhaustion create: {e:?}"))?;
    let id = handle.snapshot().browser;
    out.browser = Some(id);
    let until = (Instant::now() + WAIT).min(deadline);
    while handle.snapshot().state != SessionState::Alive {
        if handle.snapshot().state != SessionState::Creating || Instant::now() >= until {
            return Err("exhaustion Browser creation timeout".into());
        }
        turn(cx, rt, receiver, false).await?;
        stats.check()?;
    }
    let attachment = rt
        .attach_browser(&handle)
        .map_err(|e| format!("exhaustion attach: {e:?}"))?;
    out.duplicate_refused = rt.attach_browser(&handle).is_err();
    if !out.duplicate_refused {
        return Err("exhaustion duplicate attachment accepted".into());
    }
    let entity = update(cx, root, move |host, w, app| {
        let e = CefBrowser::new(attachment, w, app)?;
        host.browser = Some(e.clone());
        app.notify();
        Ok::<_, rust_cef_gpui::ViewError>(e)
    })?
    .map_err(|e| format!("exhaustion View: {e:?}"))?;
    let mut composition = UiComposition::new(NonZeroUsize::new(1).ok_or("zero route")?);
    update(cx, root, |_, w, app| {
        composition.register(&entity, w, &*app)
    })?
    .map_err(|e| format!("exhaustion register: {e:?}"))?;
    let first = app_update(cx, |app| composition.dispatch(app))?;
    if first.attempted != 1 || first.removed != 0 || !first.failures.is_empty() {
        return Err(format!("exhaustion initial dispatch: {first:?}"));
    }
    let mut r = Run {
        cx,
        rt,
        receiver,
        stats,
        root,
        options,
        out,
        composition,
        probe,
        id,
        scale,
        deadline,
        phase: "initial-frame-and-observer",
        baseline: None,
        letterbox: None,
    };
    // Keep Run alive for fallback; step snapshots fatal events before caller
    // locals unwind. After explicit View release, missing source/paint stays
    // unavailable rather than keeping an extra Entity that breaks Weak proof.
    let result = async {
        let (old, d0) = r.frame(&entity, &handle, 0, (320, 240), false).await?;
        let baseline = r.rec()?;
        r.baseline = Some(baseline.clone());
        r.capture(0, &d0, Some(&old), &baseline, 0).await?;
        if recovery_json(&r.rec()?) != recovery_json(&baseline) {
            return Err("initial recovery drifted during stage0 capture".into());
        }
        r.phase = "inject-and-resize";
        let dropped_before = r.control_state()?.dropped;
        r.control(true).await?;
        stats.fault.lock().unwrap().arm(id)?;
        if options.exhaustion_manual_foreground && options.backend == RenderPreference::D3d11Required {
            let capture = r.rt.frame_capture_diagnostics(id).map_err(|e| format!("letterbox baseline: {e:?}"))?;
            if capture.failures != 0 || !r.control_state()?.native_rejections.is_empty() {
                return Err("letterbox baseline already had capture failures".into());
            }
            r.letterbox = Some(super::exhaustion_evidence::LetterboxCut {
                browser: id, ui: r.ui()?,
                generation: old.info.surface_generation.checked_add(1).ok_or("letterbox target generation overflow")?,
                target: target((500, 300), scale),
                frame: (old.info.surface_generation, old.info.sequence, old.info.width, old.info.height),
                sha256: probe_frame_sha256(&old),
                acks: baseline.acks,
                io: (capture.opens, capture.copies, capture.maps, capture.unmaps),
            });
            *stats.exhaustion_letterbox.lock().unwrap() = Some(id);
        }
        update(r.cx, root, |host, _, app| {
            host.extent = (500., 300.);
            app.notify();
        })?;
        let until = (Instant::now() + WAIT).min(deadline);
        loop {
            let rec = r.rec()?;
            if exhausted(&rec, &baseline, r.ui()?, target((500, 300), scale))
                && stats.fault.lock().unwrap().count() == 1
            {
                break;
            }
            if Instant::now() >= until {
                return Err(format!(
                    "controlled exhaustion timeout: {} {}",
                    recovery_json(&rec),
                    stats.fault.lock().unwrap().json()
                ));
            }
            r.step().await?;
        }
        let quiet_started = Instant::now();
        while quiet_started.elapsed() < QUIET {
            r.step().await?;
        }
        let failed = r.rec()?;
        if !exhausted(&failed, &baseline, r.ui()?, target((500, 300), scale))
            || stats.fault.lock().unwrap().count() != 1
            || r.control_state()?.dropped <= dropped_before
        {
            return Err("exhaustion quiet window violated exact8/event/drop witness".into());
        }
        let s =
            r.rt.frame_source(id)
                .map_err(|e| format!("retained source: {e:?}"))?
                .snapshot();
        let kept = s
            .view
            .frame
            .as_ref()
            .ok_or("exhaustion lost old good frame")?;
        if s.closed
            || !s.view.visible
            || s.view.generation != failed.generation
            || s.view.generation <= old.info.surface_generation
            || !Arc::ptr_eq(kept, &old)
            || kept.pixels.as_slice() != old.pixels.as_slice()
        {
            return Err("exhaustion changed old good frame or wrong source generation".into());
        }
        update(r.cx, root, |host, _, app| {
            host.fault_label = Some(STALE_LABEL);
            app.notify();
        })?;
        rendered(r.cx, root).await?;
        let d1 = update(r.cx, root, |_, _, app| entity.read(app).diagnostics())?;
        if d1.last_error.is_some()
            || d1.painted_generation != old.info.surface_generation
            || d1.painted_sequence != old.info.sequence
            || (d1.frame_width, d1.frame_height) != (old.info.width, old.info.height)
            || d1.surface_bounds != (0., 48., 500., 300.)
        {
            return Err(format!("old good image not actually painted 1:1: {d1:?}"));
        }
        let quiet_ms = u64::try_from(quiet_started.elapsed().as_millis())
            .map_err(|_| "quiet duration overflow")?;
        r.capture(1, &d1, Some(&old), &baseline, quiet_ms).await?;
        let mut same_target = None;
        if let Some(cut) = &r.letterbox {
            r.phase = "recover-original-exhausted-target";
            *stats.exhaustion_letterbox.lock().unwrap() = None;
            r.check_letterboxes(None)?;
            stats.check()?;
            let after_observer = r.rec()?;
            if recovery_json(&after_observer) != recovery_json(&failed)
                || failed.generation != cut.generation
                || stats.fault.lock().unwrap().count() != 1 || !r.control_state()?.drop_view
            {
                return Err("original target changed after exhaustion observer release".into());
            }
            r.control(false).await?;
            // A normal trusted content change requests a real compositor frame
            // at the unchanged viewport. Do not add a ninth recovery Invalidate.
            handle.submit(BrowserCommand::Navigate(recovery_page(true)))
                .map_err(|e| format!("same-target recovery navigation: {e:?}"))?;
            let (frame, painted) = r.frame(&entity, &handle, failed.generation - 1, (500, 300), true).await?;
            let report = r.rec()?;
            if !original_target_recovered(&failed, &report)
                || frame.info.surface_generation != failed.generation || frame.info.sequence <= old.info.sequence
            {
                return Err("same-target recovery did not ACK the original generation at exact eight Invalidates".into());
            }
            let path = options.report.with_file_name("exhaustion-original-target.bgra");
            super::exhaustion_evidence::atomic(&path, frame.pixels.as_slice())?;
            same_target = Some(object(vec![
                ("file", quote("exhaustion-original-target.bgra")),
                ("bytes", frame.pixels.as_slice().len().to_string()),
                ("sha256", quote(&probe_frame_sha256(&frame))),
                ("browser", bid(id)),
                ("generation", frame.info.surface_generation.to_string()),
                ("sequence", frame.info.sequence.to_string()),
                ("width", frame.info.width.to_string()),
                ("height", frame.info.height.to_string()),
                ("marker_verified", bool_json(true)),
                ("paint", metrics_json(&painted)),
                ("recovery", recovery_json(&report)),
            ]));
        } else {
            r.control(false).await?;
        }
        r.phase = "return-to-original-layout";
        update(r.cx, root, |host, _, app| {
            host.extent = (320., 240.);
            app.notify();
        })?;
        let (fresh, d2) = r.frame(&entity, &handle, failed.generation, (320, 240), false).await?;
        let recovered = r.rec()?;
        if fresh.info.sequence <= old.info.sequence
            || recovered.acks <= baseline.acks
            || recovered.exhaustions != failed.exhaustions
            || recovered.post_failures != 0
            || stats.fault.lock().unwrap().count() != 1
        {
            return Err("new target did not truly recover without another fault".into());
        }
        update(r.cx, root, |host, _, app| {
            host.fault_label = None;
            app.notify();
        })?;
        rendered(r.cx, root).await?;
        r.out.paint = Some(d2.clone());
        r.capture(2, &d2, Some(&fresh), &baseline, 0).await?;
        r.phase = "release-view-and-green";
        let weak = entity.downgrade();
        let released = update(r.cx, root, |host, w, app| {
            let e = host.browser.take().ok_or("exhaustion host lost View")?;
            let d = e.update(app, |v, app| {
                v.release_image(w, app)
                    .map_err(|e| format!("exhaustion release: {e:?}"))?;
                Ok::<_, String>(v.diagnostics())
            })?;
            host.detached = true;
            host.fault_label = None;
            app.notify();
            Ok::<_, String>(d)
        })??;
        r.composition.unregister(id);
        drop(entity);
        if app_update(r.cx, |app| {
            weak.upgrade().is_some() || !app.windows().contains(&root.into())
        })? {
            return Err("exhaustion View release/window qualification failed".into());
        }
        if released.created_images == 0
            || released.created_images != released.released_images
            || released.last_error.is_some()
        {
            return Err("exhaustion image release unbalanced".into());
        }
        let before = update(r.cx, root, |host, _, _| host.renders)?;
        rendered(r.cx, root).await?;
        r.out.detached_alive = update(r.cx, root, |host, _, _| {
            host.detached && host.renders > before
        })?;
        r.out.released_view = Some(released.clone());
        while !matches!(
            handle.snapshot().state,
            SessionState::Closing | SessionState::Closed
        ) {
            if Instant::now() >= deadline {
                return Err("exhaustion release did not request Closing".into());
            }
            r.step().await?;
        }
        r.out.view_release_observed_closing = true;
        stats.fault.lock().unwrap().retire()?;
        r.capture(3, &released, None, &baseline, 0).await?;
        r.phase = "final-release";
        while !options.gate.is_file() {
            if Instant::now() >= deadline {
                return Err("exhaustion final observer timeout".into());
            }
            r.step().await?;
        }
        if std::fs::read_to_string(&options.gate)
            .map_err(|e| e.to_string())?
            .trim()
            != "release"
        {
            return Err("exhaustion final gate mismatch".into());
        }
        let letterbox_evidence = if r.letterbox.is_some() {
            while handle.snapshot().state != SessionState::Closed {
                if Instant::now() >= deadline { return Err("letterbox Browser retirement timeout".into()); }
                r.step().await?;
            }
            let capture = r.rt.frame_capture_diagnostics(id).map_err(|e| format!("retired letterbox capture: {e:?}"))?;
            if !capture.retired || capture.native_resources_live || capture.abandoned || capture.live_imports != 0
                || capture.maps != capture.unmaps || capture.owner_thread != r.ui()? || capture.retirement_thread != r.ui()?
                || capture.software_captures != 0
                || (capture.failures > 0 && (capture.last_failure.as_deref() != Some("IncompleteFrame") || capture.last_failure_code.is_some()))
            {
                return Err("letterbox capture ownership or terminal accounting incomplete".into());
            }
            stats.check()?;
            let count = r.check_letterboxes(Some(capture.failures))?;
            object(vec![
                ("validated_rejections", count.to_string()),
                ("native_failure_count", capture.failures.to_string()),
                ("capture_events", stats.capture_events.lock().unwrap().json()),
                ("native", super::exhaustion_evidence::probe_json(&r.control_state()?)),
                ("original_target_recovery", same_target.ok_or("missing same-target recovery bytes")?),
                ("native_capture_retired", bool_json(true)),
                ("owner_ui_thread", capture.owner_thread.to_string()),
                ("retirement_ui_thread", capture.retirement_thread.to_string()),
            ])
        } else { "null".into() };
        r.out.exhaustion_detail = Some(object(vec![
            ("kind", quote("recovery-exhaustion-slice")),
            ("letterbox", letterbox_evidence),
            ("success", bool_json(true)),
            ("stages", "4".into()),
            ("exhaustion_verified", bool_json(true)),
            ("task_11_3_accepted", bool_json(false)),
            ("baseline", recovery_json(&baseline)),
            ("exhausted", recovery_json(&failed)),
            ("recovered", recovery_json(&recovered)),
            ("fault", stats.fault.lock().unwrap().json()),
            ("quiet_observation_ms", quiet_ms.to_string()),
        ]));
        Ok(())
    }
    .await;
    if let Err(original) = &result {
        super::exhaustion_evidence::capture_once(
            r.cx,
            r.rt,
            r.stats,
            r.root,
            r.options,
            r.out,
            r.phase,
            r.baseline.as_ref(),
            original,
        );
    }
    result
}
pub(super) async fn observe_manual_foreground(
    cx: &mut AsyncApp,
    rt: &mut Runtime,
    receiver: &UiWakeReceiver,
    root: WindowHandle<HostRoot>,
    probe: &PointerProbe,
    facts: &mut InitialForeground,
    scenario: &'static str,
) -> Result<(), String> {
    super::visibility::require_unlocked_session()?;
    let owner = ManualOwner::sample(probe)?;
    facts.before = Some(probe.is_foreground()?);
    let end = Instant::now() + MANUAL_WAIT;
    let mut since = None;
    loop {
        let now = Instant::now();
        if now >= end {
            return Err(format!(
                "manual {scenario} foreground timeout; no request or input"
            ));
        }
        if !update(cx, root, |_, window, _| window.is_visible())?
            || ManualOwner::sample(probe)? != owner
        {
            return Err(format!(
                "manual {scenario} original window became hidden or changed geometry"
            ));
        }
        let sample_started = Instant::now();
        let foreground = probe.is_foreground()?;
        if sustained_foreground(&mut since, sample_started, Instant::now(), foreground) {
            super::visibility::require_unlocked_session()?;
            if ManualOwner::sample(probe)? != owner || !probe.is_foreground()? {
                return Err(format!("manual {scenario} foreground drifted at readiness"));
            }
            if Instant::now() >= end {
                return Err(format!(
                    "manual {scenario} foreground timeout; no request or input"
                ));
            }
            facts.after = Some(true);
            facts.manual_owner = Some(owner);
            return Ok(());
        }
        turn(cx, rt, receiver, false).await?;
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
    let mut probe = match PointerProbe::install(TITLE) {
        Ok(probe) => probe,
        Err(original) => {
            super::exhaustion_evidence::capture_once(
                cx,
                rt,
                stats,
                root,
                options,
                out,
                "probe-install",
                None,
                &original,
            );
            return Err(original);
        }
    };
    let setup = if options.exhaustion_manual_foreground {
        observe_manual_foreground(
            cx,
            rt,
            receiver,
            root,
            &probe,
            &mut out.initial_foreground,
            "Exhaustion",
        )
        .await
    } else {
        (|| {
            if options.initial_foreground_approved {
                // User opt-in is Exhaustion-only. End the GPUI read borrow, then
                // recheck unlocked session and the original property/owner through
                // PointerProbe before its at-most-one ordinary OS request.
                if !update(cx, root, |_, w, _| w.is_visible())? {
                    return Err("initial owned window is hidden/minimized".into());
                }
                super::visibility::require_unlocked_session()?;
                let before = probe.is_foreground()?;
                out.initial_foreground.before = Some(before);
                if !before {
                    out.initial_foreground.attempts = 1;
                    probe.request_foreground()?; // Ordinary SetForegroundWindow; refusal is fatal, no retry.
                }
                let after = probe.is_foreground()?;
                out.initial_foreground.after = Some(after);
                if !after {
                    return Err("initial owned foreground not confirmed".into());
                }
            }
            Ok::<_, String>(())
        })()
    };
    let result = match setup {
        Ok(()) => inner(cx, rt, receiver, stats, root, options, out, &probe).await,
        Err(original) => {
            super::exhaustion_evidence::capture_once(
                cx,
                rt,
                stats,
                root,
                options,
                out,
                if options.exhaustion_manual_foreground {
                    "manual-owned-foreground"
                } else {
                    "initial-owned-foreground-request"
                },
                None,
                &original,
            );
            Err(original)
        }
    };
    if let Err(original) = &result {
        super::exhaustion_evidence::capture_once(
            cx,
            rt,
            stats,
            root,
            options,
            out,
            "before-run-or-body-fallback",
            None,
            original,
        );
    }
    *stats.exhaustion_letterbox.lock().unwrap() = None;
    let result = super::exhaustion_evidence::retain_primary(result, probe.retire());
    if let Err(original) = &result {
        super::exhaustion_evidence::capture_once(
            cx,
            rt,
            stats,
            root,
            options,
            out,
            "probe-retirement",
            None,
            original,
        );
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_gpui::contracts::RuntimeGeneration;
    #[test]
    fn same_target_recovery_cannot_borrow_new_generation_ack_or_ninth_invalidate() {
        let exhausted = FrameRecoveryDiagnostics {
            status: "Exhausted",
            generation: 2,
            expected_width: 750,
            expected_height: 450,
            attempts: 8,
            requests_total: 8,
            native_invalidate_entries: 8,
            exhaustions: 1,
            ..FrameRecoveryDiagnostics::default()
        };
        let mut recovered = exhausted.clone();
        recovered.status = "Recovered";
        recovered.acks = 1;
        assert!(original_target_recovered(&exhausted, &recovered));
        recovered.generation = 3;
        assert!(!original_target_recovered(&exhausted, &recovered));
        recovered.generation = 2;
        recovered.expected_width = 480;
        assert!(!original_target_recovered(&exhausted, &recovered));
        recovered.expected_width = 750;
        recovered.native_invalidate_entries = 9;
        assert!(!original_target_recovered(&exhausted, &recovered));
        recovered.native_invalidate_entries = 8;
        recovered.acks = 0;
        assert!(!original_target_recovered(&exhausted, &recovered));
    }

    #[test]
    fn manual_foreground_opt_in_refuses_wrong_scope_and_ordinary_request() {
        let enabled = |value, scenario, ordinary| {
            exhaustion_manual_foreground_flag(value, scenario, ordinary)
        };
        assert_eq!(enabled(None, HostScenario::Exhaustion, false), Ok(false));
        assert_eq!(
            enabled(Some("true"), HostScenario::Exhaustion, false),
            Ok(true)
        );
        assert!(enabled(Some("true"), HostScenario::Exhaustion, true).is_err());
        assert!(enabled(Some("true"), HostScenario::Resize, false).is_err());
        assert!(enabled(Some("yes"), HostScenario::Exhaustion, false).is_err());
    }

    #[test]
    fn visibility_manual_foreground_requires_explicit_visibility_approval() {
        let yes = Some("true");
        assert_eq!(
            visibility_manual_foreground_flag(None, HostScenario::Visibility, true),
            Ok(false)
        );
        assert_eq!(
            visibility_manual_foreground_flag(yes, HostScenario::Visibility, true),
            Ok(true)
        );
        assert!(visibility_manual_foreground_flag(yes, HostScenario::Visibility, false).is_err());
        assert!(visibility_manual_foreground_flag(yes, HostScenario::Exhaustion, true).is_err());
        assert!(
            visibility_manual_foreground_flag(Some("yes"), HostScenario::Visibility, true).is_err()
        );
    }

    #[test]
    fn manual_foreground_requires_sustained_samples_after_any_loss() {
        let start = Instant::now();
        let mut since = None;
        let sample = |since: &mut Option<Instant>, before, after, foreground| {
            sustained_foreground(
                since,
                start + Duration::from_millis(before),
                start + Duration::from_millis(after),
                foreground,
            )
        };
        assert!(!sample(&mut since, 0, 1, false));
        assert!(!sample(&mut since, 100, 252, true));
        assert!(!sample(&mut since, 252, 260, true));
        assert!(!sample(&mut since, 300, 301, false));
        assert!(!sample(&mut since, 301, 303, true));
        assert!(!sample(&mut since, 452, 455, true));
        assert!(sample(&mut since, 453, 455, true));
    }

    #[test]
    fn one_fault_is_identity_phase_kind_sequence_bound_and_retirement_keeps_evidence() {
        let a = BrowserId::new(RuntimeGeneration::new(1), 1);
        let foreign = BrowserId::new(RuntimeGeneration::new(2), 1);
        let error = EngineError::Capture {
            kind: CaptureFailureKind::ResizeExhausted,
            code: None,
        };
        let mut g = ExpectedFault::default();
        assert!(!g.observe(a, 1, &error));
        g.arm(a).unwrap();
        assert!(!g.observe(foreign, 1, &error));
        assert!(!g.observe(a, 0, &error));
        assert!(!g.observe(
            a,
            1,
            &EngineError::Capture {
                kind: CaptureFailureKind::ResizeExhausted,
                code: Some(0)
            }
        ));
        assert!(!g.observe(
            a,
            1,
            &EngineError::Capture {
                kind: CaptureFailureKind::IncompleteFrame,
                code: None
            }
        ));
        assert_eq!(g.count(), 0);
        assert!(g.observe(a, 5, &error));
        assert!(!g.observe(a, 6, &error));
        assert!(g.arm(a).is_err());
        g.retire().unwrap();
        assert_eq!(g.browser, Some(a));
        assert_eq!(g.event_sequence, 5);
        assert!(g.retired);
        assert!(!g.observe(a, 7, &error));
        assert_eq!(g.count(), 1);
    }
    #[test]
    fn exhausted_predicate_requires_native_eight_spacing_thread_and_no_ack() {
        let base = FrameRecoveryDiagnostics::default();
        assert!(pristine_idle(&base));
        let mut not_pristine = base.clone();
        not_pristine.requests_total = 1;
        assert!(!pristine_idle(&not_pristine));
        let mut r = base.clone();
        r.status = "Exhausted";
        r.attempts = 8;
        r.requests_total = 8;
        r.native_invalidate_entries = 8;
        r.native_thread = 9;
        r.expected_width = 750;
        r.expected_height = 450;
        r.exhaustions = 1;
        r.request_offsets_us = (0..8).map(|i| i * 100_000).collect();
        r.native_offsets_us = r.request_offsets_us.clone();
        assert!(exhausted(&r, &base, 9, (750, 450)));
        r.acks = 1;
        assert!(!exhausted(&r, &base, 9, (750, 450)));
        r.acks = 0;
        r.native_offsets_us[7] = 699_999;
        assert!(!exhausted(&r, &base, 9, (750, 450)));
        r.native_offsets_us[7] = 700_000;
        assert!(!exhausted(&r, &base, 10, (750, 450)));
        r.native_invalidate_entries = 9;
        assert!(!exhausted(&r, &base, 9, (750, 450)));
    }
}
