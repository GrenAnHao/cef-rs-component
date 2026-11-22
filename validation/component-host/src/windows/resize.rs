//! Partial 11.3: real public-window/layout resize, independent desktop pixels.
//! No CEF viewport calls, synthetic input/DPI, mode switching or full-task claim.
use super::*;
use rust_cef_gpui::{UiComposition, contracts::SessionState, ports::BrowserHandle};
use std::num::NonZeroUsize;

const EXTENTS: [(u32, u32); 5] = [(320, 240), (16, 12), (500, 300), (64, 48), (320, 240)];
const WAIT: Duration = Duration::from_secs(20);

fn client_size((w, h): (u32, u32)) -> (u32, u32) {
    ((w + 80).max(200), (h + 120).max(180))
}
pub(super) fn initial_client_size() -> (u32, u32) {
    client_size(EXTENTS[0])
}

pub(super) fn passive_right_bounds(
    work: Bounds<gpui::Pixels>,
) -> Result<Bounds<gpui::Pixels>, &'static str> {
    let (left, top) = (work.origin.x.as_f32(), work.origin.y.as_f32());
    let (width, height) = (work.size.width.as_f32(), work.size.height.as_f32());
    let (max_w, max_h) = EXTENTS
        .into_iter()
        .map(client_size)
        .fold((0, 0), |(a, b), (w, h)| (a.max(w), b.max(h)));
    // The move is only the initial bounds of a new owned frame. Reserve
    // nonclient space and accommodate the largest, not just the first client.
    const H_MARGIN: f32 = 80.;
    const TOP: f32 = 40.;
    if ![left, top, width, height].into_iter().all(f32::is_finite)
        || width < max_w as f32 + 2. * H_MARGIN
        || height < max_h as f32 + TOP + H_MARGIN
    {
        return Err("primary work area cannot fit the passive Resize sequence");
    }
    let (w, h) = initial_client_size();
    Ok(Bounds::new(
        gpui::point(px(left + width - H_MARGIN - max_w as f32), px(top + TOP)),
        size(px(w as f32), px(h as f32)),
    ))
}

fn target((w, h): (u32, u32), scale: f32) -> (u32, u32) {
    (
        ((w as f32) * scale).ceil() as u32,
        ((h as f32) * scale).ceil() as u32,
    )
}
fn page() -> String {
    // The centre stays blue for the host's existing legacy frame counter, but
    // independent entire-VIEW comparison uses both fixed-width coloured bars.
    let html = "<html><head><title>Resize static</title></head><body style='margin:0;background:rgb(26,76,126)'><div style='position:fixed;left:0;top:0;width:4px;height:100%;background:rgb(190,90,20)'></div><div style='position:fixed;left:0;top:0;width:100%;height:4px;background:rgb(110,40,180)'></div></body></html>";
    let mut uri = String::from("data:text/html,");
    for b in html.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}
fn atomic(path: &Path, value: &str) -> Result<(), String> {
    let tmp = path.with_extension("pending");
    std::fs::write(&tmp, value).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}
async fn step(
    cx: &mut AsyncApp,
    rt: &mut Runtime,
    receiver: &UiWakeReceiver,
    stats: &Stats,
    out: &mut Outcome,
    composition: &mut UiComposition,
) -> Result<(), String> {
    if turn(cx, rt, receiver, false).await? {
        out.wake_consumed += 1;
        let report = app_update(cx, |app| composition.dispatch(app))?;
        if report.removed != 0 || !report.failures.is_empty() {
            return Err(format!("resize dispatch: {report:?}"));
        }
    }
    stats.check()
}
fn metadata(
    stage: usize,
    browser: BrowserId,
    extent: (u32, u32),
    scale: f32,
    d: &ViewDiagnostics,
    raw: Option<&OwnedFrame>,
    manual: Option<(String, String)>,
    passive: Option<String>,
) -> String {
    let (cw, ch) = client_size(extent);
    let raw = raw.map_or_else(
        || "null".into(),
        |f| {
            object(vec![
                ("file", quote(&format!("resize-stage-{stage}-view.bgra"))),
                (
                    "browser",
                    format!(
                        "[{},{}]",
                        f.info.browser.runtime().value(),
                        f.info.browser.local()
                    ),
                ),
                ("surface", quote(&format!("{:?}", f.info.surface))),
                ("generation", f.info.surface_generation.to_string()),
                ("sequence", f.info.sequence.to_string()),
                ("width", f.info.width.to_string()),
                ("height", f.info.height.to_string()),
                ("bytes", f.pixels.len().to_string()),
            ])
        },
    );
    let mut fields = vec![
        ("schema", "1".into()),
        ("kind", quote("window-layout-resize-slice")),
        ("pid", std::process::id().to_string()),
        ("stage", stage.to_string()),
        (
            "browser",
            format!("[{},{}]", browser.runtime().value(), browser.local()),
        ),
        ("scale_factor", scale.to_string()),
        ("view_bounds", format!("[0,48,{},{}]", extent.0, extent.1)),
        ("client_dip", format!("[{cw},{ch}]")),
        ("diagnostics", metrics_json(d)),
        ("view_raw", raw),
        ("detached", bool_json(stage == 5)),
        ("task_11_3_accepted", bool_json(false)),
    ];
    if let Some((window, facts)) = manual {
        fields.push(("window", window));
        fields.push(("initial_foreground", facts));
    }
    if let Some(passive) = passive {
        fields.push(("passive_window", passive));
    }
    object(fields)
}
#[allow(clippy::too_many_arguments)]
async fn observe(
    cx: &mut AsyncApp,
    rt: &mut Runtime,
    receiver: &UiWakeReceiver,
    stats: &Stats,
    out: &mut Outcome,
    composition: &mut UiComposition,
    options: &Options,
    deadline: Instant,
    stage: usize,
    text: &str,
) -> Result<(), String> {
    let case = options.report.parent().ok_or("resize case missing")?;
    atomic(&case.join(format!("resize-stage-{stage}.json")), text)?;
    if stage == 0 {
        // Shared launcher starts its stage callback only after this main gate.
        atomic(
            &options.gate.with_extension("ready"),
            &std::process::id().to_string(),
        )?;
    }
    atomic(
        &options.gate.with_extension(format!("resize-{stage}.ready")),
        &std::process::id().to_string(),
    )?;
    let release = options
        .gate
        .with_extension(format!("resize-{stage}.release"));
    let until = (Instant::now() + WAIT).min(deadline);
    while !release.is_file() {
        if Instant::now() >= until {
            return Err(format!("resize stage {stage} observer timeout"));
        }
        step(cx, rt, receiver, stats, out, composition).await?;
    }
    if std::fs::read_to_string(release)
        .map_err(|e| e.to_string())?
        .trim()
        != std::process::id().to_string()
    {
        return Err("resize stage release PID mismatch".into());
    }
    Ok(())
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
    #[cfg(feature = "validation")]
    let mut manual = if options.resize_manual_foreground {
        Some((
            super::pointer_probe::PointerProbe::install(TITLE)?,
            super::exhaustion::InitialForeground::default(),
        ))
    } else {
        None
    };
    let result = run(
        cx,
        rt,
        receiver,
        stats,
        root,
        options,
        out,
        #[cfg(feature = "validation")]
        manual.as_mut(),
    )
    .await;
    #[cfg(feature = "validation")]
    if options.resize_capture_evidence
        && let Err(original) = &result
    {
        if let Err(secondary) = write_failure(cx, rt, stats, root, options, out, original) {
            eprintln!("Resize secondary evidence failure: {secondary}");
        }
    }
    #[cfg(feature = "validation")]
    if let Some((mut probe, _)) = manual {
        return super::exhaustion_evidence::retain_primary(result, probe.retire());
    }
    result
}

#[cfg(feature = "validation")]
fn write_failure(
    cx: &mut AsyncApp,
    rt: &Runtime,
    stats: &Stats,
    root: WindowHandle<HostRoot>,
    options: &Options,
    out: &Outcome,
    original: &str,
) -> Result<(), String> {
    use super::exhaustion_evidence::{atomic, probe_json, recovery_json};
    use rust_cef_gpui::test_support::probe_frame_sha256;

    let id = out.browser;
    let query_error = |message: String| object(vec![("query_error", quote(&message))]);
    let capture = id.map_or("null".into(), |id| match rt.frame_capture_diagnostics(id) {
        Ok(d) => object(vec![
            ("owner_thread", d.owner_thread.to_string()),
            ("failures", d.failures.to_string()),
            (
                "observed_backend",
                d.observed_backend.map_or("null".into(), quote),
            ),
            ("opens", d.opens.to_string()),
            ("copies", d.copies.to_string()),
            ("maps", d.maps.to_string()),
            ("unmaps", d.unmaps.to_string()),
            ("retired", bool_json(d.retired)),
        ]),
        Err(e) => query_error(format!("{e:?}")),
    });
    let recovery = id.map_or("null".into(), |id| {
        match rt.frame_recovery_diagnostics(id) {
            Ok(d) => recovery_json(&d),
            Err(e) => query_error(format!("{e:?}")),
        }
    });
    let source = id.map_or("null".into(), |id| match rt.frame_source(id) {
        Ok(source) => {
            let s = source.snapshot();
            let frame = s.view.frame.as_ref().map_or("null".into(), |f| {
                let i = f.info;
                object(vec![
                    ("generation", i.surface_generation.to_string()),
                    ("sequence", i.sequence.to_string()),
                    ("width", i.width.to_string()),
                    ("height", i.height.to_string()),
                    ("sha256", quote(&probe_frame_sha256(f))),
                ])
            });
            object(vec![
                ("generation", s.view.generation.to_string()),
                ("frame", frame),
                ("closed", bool_json(s.closed)),
            ])
        }
        Err(e) => query_error(format!("{e:?}")),
    });
    let layout = match update(cx, root, |host, window, _| {
        (host.extent, window.scale_factor())
    }) {
        Ok(((w, h), scale)) => format!("[{w},{h},{scale}]"),
        Err(e) => query_error(e),
    };
    let events = stats.capture_events.lock().unwrap().clone();
    let native = id.and_then(|id| rt.probe_frame_state(id));
    let facts = object(vec![
        ("kind", quote("resize-capture-failure-evidence")),
        ("pid", std::process::id().to_string()),
        (
            "browser",
            id.map_or("null".into(), |id| {
                format!("[{},{}]", id.runtime().value(), id.local())
            }),
        ),
        ("original_error", quote(original)),
        ("success", bool_json(false)),
        ("capture_events", events.json()),
        ("probe", native.as_ref().map_or("null".into(), probe_json)),
        ("capture", capture),
        ("recovery", recovery),
        ("source", source),
        ("layout", layout),
        ("task_11_3_accepted", bool_json(false)),
    ]);
    atomic(
        &options.report.with_file_name("resize-capture-failure.json"),
        facts.as_bytes(),
    )
}

#[cfg(feature = "validation")]
struct ResizeTransitions<'a> {
    stats: &'a Stats,
    browser: BrowserId,
    prior: Option<((u64, u64, u32, u32), String)>,
    active: Option<super::exhaustion_evidence::LetterboxCut>,
    done: Vec<SettledTransition>,
}

#[cfg(feature = "validation")]
struct SettledTransition {
    cut: super::exhaustion_evidence::LetterboxCut,
    through: usize,
    sequence: u64,
    sha256: String,
    ack: u64,
}

#[cfg(feature = "validation")]
impl<'a> ResizeTransitions<'a> {
    fn new(stats: &'a Stats, browser: BrowserId) -> Self {
        Self {
            stats,
            browser,
            prior: None,
            active: None,
            done: Vec::with_capacity(4),
        }
    }

    fn frame(f: &OwnedFrame) -> (u64, u64, u32, u32) {
        let i = f.info;
        (i.surface_generation, i.sequence, i.width, i.height)
    }

    fn native(
        &self,
        rt: &Runtime,
    ) -> Result<rust_cef_gpui::test_support::FrameProbeState, String> {
        rt.probe_frame_state(self.browser)
            .ok_or("verified Resize callback ledger missing".into())
    }

    fn verify(&self, rt: &Runtime, failures: Option<u64>) -> Result<usize, String> {
        self.stats.check()?;
        // Capture records the native row before emitting CaptureFailed.
        let events = self.stats.capture_events.lock().unwrap().clone();
        let native = self.native(rt)?;
        super::exhaustion_evidence::verify_resize_prefix(
            &events,
            &native,
            |n| {
                self.done
                    .iter()
                    .find(|settled| n <= settled.through)
                    .map(|s| &s.cut)
                    .or(self.active.as_ref())
            },
            failures,
        )
    }

    fn arm(&mut self, rt: &Runtime, target: (u32, u32)) -> Result<(), String> {
        use super::exhaustion_evidence::LetterboxCut;
        use rust_cef_gpui::test_support::probe_frame_sha256;
        self.verify(rt, None)?;
        let (old, sha256) = self
            .prior
            .as_ref()
            .ok_or("verified Resize lacks previous painted bytes")?;
        let snapshot = rt
            .frame_source(self.browser)
            .map_err(|e| format!("resize prior source: {e:?}"))?
            .snapshot();
        let retained = snapshot
            .view
            .frame
            .as_ref()
            .ok_or("verified Resize previous frame vanished")?;
        if self.active.is_some()
            || snapshot.closed
            || snapshot.view.generation != old.0
            || Self::frame(retained) != *old
            || probe_frame_sha256(retained) != *sha256
        {
            return Err("verified Resize prior exact frame changed before transition".into());
        }
        let cap = rt
            .frame_capture_diagnostics(self.browser)
            .map_err(|e| format!("resize baseline capture: {e:?}"))?;
        let r = rt
            .frame_recovery_diagnostics(self.browser)
            .map_err(|e| format!("resize baseline recovery: {e:?}"))?;
        let ui = rt
            .diagnostics()
            .cef_ui_thread_id
            .ok_or("verified Resize CEF UI missing")?;
        if ui == 0
            || cap.owner_thread != ui
            || cap.observed_backend != Some("D3d11")
            || cap.retired
            || r.queued && r.status == "Pending"
        {
            return Err("verified Resize baseline ownership/recovery mismatch".into());
        }
        self.active = Some(LetterboxCut {
            browser: self.browser,
            ui,
            generation: old
                .0
                .checked_add(1)
                .ok_or("verified Resize generation overflow")?,
            target,
            frame: *old,
            sha256: sha256.clone(),
            acks: r.acks,
            io: (cap.opens, cap.copies, cap.maps, cap.unmaps),
        });
        let mut owner = self.stats.resize_letterbox.lock().unwrap();
        if owner.is_some() {
            return Err("verified Resize already has an armed owner".into());
        }
        *owner = Some(self.browser);
        Ok(())
    }

    fn full_frame(&mut self, rt: &Runtime, f: &OwnedFrame) -> Result<bool, String> {
        use rust_cef_gpui::test_support::probe_frame_sha256;
        let count = self.verify(rt, None)?;
        let sha256 = probe_frame_sha256(f);
        if let Some(cut) = self.active.as_ref() {
            let r = rt
                .frame_recovery_diagnostics(self.browser)
                .map_err(|e| format!("resize target recovery: {e:?}"))?;
            if f.info.surface_generation != cut.generation
                || (f.info.width, f.info.height) != cut.target
                || r.generation != cut.generation
                || (r.expected_width, r.expected_height) != cut.target
            {
                return Err(
                    "verified Resize full frame or recovery is not the armed target".into(),
                );
            }
            let ack = cut
                .acks
                .checked_add(1)
                .ok_or("verified Resize ACK overflow")?;
            if r.status == "Pending" && r.acks == cut.acks {
                return Ok(false);
            }
            if r.status != "Recovered" || r.acks != ack {
                return Err("verified Resize target did not recover with exactly one ACK".into());
            }
            // The original complete frame is about to get its PNG comparison.
            // Nothing after this boundary can be assigned to the old cut.
            self.disarm();
            let cut = self
                .active
                .take()
                .ok_or("verified Resize lost armed target")?;
            self.done.push(SettledTransition {
                cut,
                through: count,
                sequence: f.info.sequence,
                sha256: sha256.clone(),
                ack,
            });
            self.verify(rt, None)?;
        } else if self.prior.is_some() {
            return Err("verified Resize settlement without an active target".into());
        }
        self.prior = Some((Self::frame(f), sha256));
        Ok(true)
    }

    fn disarm(&self) {
        let mut owner = self.stats.resize_letterbox.lock().unwrap();
        if *owner == Some(self.browser) {
            *owner = None;
        }
    }

    fn retired(&self, rt: &Runtime) -> Result<String, String> {
        use super::exhaustion_evidence::probe_json;
        let cap = rt
            .frame_capture_diagnostics(self.browser)
            .map_err(|e| format!("verified Resize retirement: {e:?}"))?;
        let ui = rt
            .diagnostics()
            .cef_ui_thread_id
            .ok_or("verified Resize retirement UI missing")?;
        if self.active.is_some()
            || self.done.len() != 4
            || !cap.retired
            || cap.native_resources_live
            || cap.abandoned
            || cap.live_imports != 0
            || cap.staging_bytes != 0
            || cap.maps != cap.unmaps
            || cap.owner_thread != ui
            || cap.retirement_thread != ui
            || cap.software_captures != 0
            || (cap.failures > 0
                && (cap.last_failure.as_deref() != Some("IncompleteFrame")
                    || cap.last_failure_code.is_some()))
        {
            return Err("verified Resize terminal capture resources or counts mismatch".into());
        }
        let count = self.verify(rt, Some(cap.failures))?;
        if self.done.last().map(|r| r.through) != Some(count) {
            return Err("verified Resize late/unassigned event at retirement".into());
        }
        let cuts: Vec<_> = self
            .done
            .iter()
            .map(|r| {
                let (g, s, w, h) = r.cut.frame;
                let (width, height) = r.cut.target;
                let (open, copy, map, unmap) = r.cut.io;
                object(vec![
                    ("old_frame", format!("[{g},{s},{w},{h}]")),
                    ("old_sha256", quote(&r.cut.sha256)),
                    ("target", format!("[{},{width},{height}]", r.cut.generation)),
                    ("io_at_arm", format!("[{open},{copy},{map},{unmap}]")),
                    ("ack_before", r.cut.acks.to_string()),
                    ("ack_after", r.ack.to_string()),
                    ("recovered_sequence", r.sequence.to_string()),
                    ("recovered_sha256", quote(&r.sha256)),
                    ("through_rejection", r.through.to_string()),
                ])
            })
            .collect();
        Ok(object(vec![
            ("validated_rejections", count.to_string()),
            ("native_failure_count", cap.failures.to_string()),
            ("cuts", format!("[{}]", cuts.join(","))),
            ("native_capture_retired", bool_json(true)),
            ("owner_ui_thread", ui.to_string()),
            ("retirement_ui_thread", cap.retirement_thread.to_string()),
            (
                "capture_events",
                self.stats.capture_events.lock().unwrap().json(),
            ),
            ("native", probe_json(&self.native(rt)?)),
        ]))
    }
}

#[cfg(feature = "validation")]
impl Drop for ResizeTransitions<'_> {
    fn drop(&mut self) {
        self.disarm();
    }
}

async fn run(
    cx: &mut AsyncApp,
    rt: &mut Runtime,
    receiver: &UiWakeReceiver,
    stats: &Stats,
    root: WindowHandle<HostRoot>,
    options: &Options,
    out: &mut Outcome,
    #[cfg(feature = "validation")] manual: Option<&mut (
        super::pointer_probe::PointerProbe,
        super::exhaustion::InitialForeground,
    )>,
) -> Result<(), String> {
    #[cfg(feature = "validation")]
    let manual = if let Some((probe, facts)) = manual {
        super::exhaustion::observe_manual_foreground(
            cx, rt, receiver, root, probe, facts, "Resize",
        )
        .await?;
        Some((&*probe, &*facts))
    } else {
        None
    };
    stats.check()?;
    let deadline = Instant::now() + Duration::from_secs(100);
    let mut composition = UiComposition::new(NonZeroUsize::new(1).ok_or("zero resize route")?);
    while rt.state() != RuntimeState::Ready {
        if Instant::now() >= deadline {
            return Err("resize runtime readiness timeout".into());
        }
        step(cx, rt, receiver, stats, out, &mut composition).await?;
    }
    let scale = update(cx, root, |_, w, _| w.scale_factor())?;
    if !scale.is_finite() || scale <= 0. {
        return Err("resize invalid real scale".into());
    }
    #[cfg(feature = "validation")]
    let passive = options
        .resize_passive
        .then(|| super::paint_latency::windows_pair::PassiveResizeWindow::observe(scale))
        .transpose()?;
    let handle: BrowserHandle = rt
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
        .map_err(|e| format!("resize create: {e:?}"))?;
    let id = handle.snapshot().browser;
    out.browser = Some(id);
    let until = (Instant::now() + WAIT).min(deadline);
    while handle.snapshot().state != SessionState::Alive {
        if !matches!(handle.snapshot().state, SessionState::Creating) || Instant::now() >= until {
            return Err("resize Browser creation incomplete".into());
        }
        step(cx, rt, receiver, stats, out, &mut composition).await?;
    }
    let attachment = rt
        .attach_browser(&handle)
        .map_err(|e| format!("resize attach: {e:?}"))?;
    out.duplicate_refused = rt.attach_browser(&handle).is_err();
    if !out.duplicate_refused {
        return Err("resize duplicate attachment accepted".into());
    }
    let entity = update(cx, root, move |host, w, app| {
        let view = CefBrowser::new(attachment, w, app)?;
        host.browser = Some(view.clone());
        app.notify();
        Ok::<_, rust_cef_gpui::ViewError>(view)
    })?
    .map_err(|e| format!("resize view: {e:?}"))?;
    update(cx, root, |_, w, app| {
        composition.register(&entity, w, &*app)
    })?
    .map_err(|e| format!("resize register: {e:?}"))?;
    let initial = app_update(cx, |app| composition.dispatch(app))?;
    if initial.attempted != 1 || initial.removed != 0 || !initial.failures.is_empty() {
        return Err(format!("resize initial dispatch: {initial:?}"));
    }
    let mut prior: Option<(u64, u64)> = None;
    // Legacy drag-quality invariant: while the size is still changing, every
    // distinct frame must keep device pixels equal to the view's device extent,
    // so the blit never resamples. Collected per frame in arrival order.
    let mut drag_quality: Vec<(u64, u32, u32, u32, u32)> = Vec::new();
    let mut drag_quality_last: Option<u64> = None;
    #[cfg(feature = "validation")]
    let mut transitions = options
        .resize_verified_transitions
        .then(|| ResizeTransitions::new(stats, id));
    #[cfg(feature = "validation")]
    let exhaustion_observed = std::cell::Cell::new(false);
    for (stage, extent) in EXTENTS.into_iter().enumerate() {
        let (cw, ch) = client_size(extent);
        #[cfg(feature = "validation")]
        if stage != 0
            && let Some(transitions) = transitions.as_mut()
        {
            transitions.arm(rt, target(extent, scale))?;
        }
        #[cfg(feature = "validation")]
        if let Some(passive) = &passive {
            // May synchronously dispatch WM_SIZE; never hold a GPUI update borrow.
            passive.resize((cw, ch))?;
        }
        update(cx, root, |host, w, app| {
            host.extent = (extent.0 as f32, extent.1 as f32);
            if !options.resize_passive && (stage != 0 || !options.resize_manual_foreground) {
                w.resize(size(px(cw as f32), px(ch as f32)));
            }
            app.notify();
        })?;
        let until = (Instant::now() + WAIT).min(deadline);
        let mut last = String::from("none");
        loop {
            if Instant::now() >= until {
                return Err(format!("resize stage {stage} frame timeout: {last}"));
            }
            rendered(cx, root).await?;
            let d = update(cx, root, |_, _, app| entity.read(app).diagnostics())?;
            let source = rt
                .frame_source(id)
                .map_err(|e| format!("resize source: {e:?}"))?
                .snapshot();
            let actual_scale = update(cx, root, |_, w, _| w.scale_factor())?;
            #[cfg(feature = "validation")]
            if let Some(transitions) = transitions.as_ref() {
                transitions.verify(rt, None)?;
            }
            let browser = handle.snapshot();
            last = format!(
                "state={:?} scale={actual_scale} metrics={}",
                browser.state,
                metrics_json(&d)
            );
            // FrameUpdate is returned by the public FrameSource, but not
            // re-exported as a named facade type. Inspect the inferred value
            // here rather than widening the stable component API for a test.
            let (fw, fh) = target(extent, scale);
            let viewport = Viewport {
                width: extent.0 as i32,
                height: extent.1 as i32,
                scale,
            };
            if let Some(frame) = source.view.frame.as_ref()
                && drag_quality_last != Some(frame.info.sequence)
            {
                drag_quality_last = Some(frame.info.sequence);
                drag_quality.push((
                    frame.info.sequence,
                    frame.info.width,
                    frame.info.height,
                    (extent.0 as f32 * scale).round() as u32,
                    (extent.1 as f32 * scale).round() as u32,
                ));
            }
            let frame_ok = source.view.frame.as_ref().is_some_and(|f| {
                !source.closed
                    && source.view.visible
                    && source.viewport == Some(viewport)
                    && source.view.generation == f.info.surface_generation
                    && f.info.browser == id
                    && f.info.surface == SurfaceKind::View
                    && (f.info.width, f.info.height) == (fw, fh)
                    && (d.frame_width, d.frame_height) == (fw, fh)
                    && d.painted_generation == f.info.surface_generation
                    && d.painted_sequence == f.info.sequence
                    && d.scale_factor == scale
                    && d.surface_bounds == (0., 48., extent.0 as f32, extent.1 as f32)
                    && d.paint_calls > 0
                    && d.last_error.is_none()
                    && !d.pending_resync
                    && prior.is_none_or(|(generation, sequence)| {
                        f.info.surface_generation > generation && f.info.sequence > sequence
                    })
            });
            // Real exhaustion with a non-terminal refusal: the refused frame is
            // dropped once, the stream continues, and the product spends its own
            // 8-request budget before reporting ResizeExhausted exactly once.
            #[cfg(feature = "validation")]
            if options.resize_exhaustion && stage == 4 && !exhaustion_observed.get() {
                exhaustion_observed.set(true);
                let exhaust_extent = (120u32, 90u32);
                let (ecw, ech) = client_size(exhaust_extent);
                let (tw, th) = target(exhaust_extent, scale);
                {
                    let mut owner = stats.resize_letterbox.lock().unwrap();
                    if owner.is_some() {
                        return Err("resize exhaustion already has an armed owner".into());
                    }
                    *owner = Some(id);
                }
                rt.probe_frame_control(
                id,
                rust_cef_gpui::test_support::FrameProbeControl::RejectIncompleteUntilReleased,
            )
            .map_err(|e| format!("resize exhaustion arm: {e:?}"))?;
                let arm_until = (Instant::now() + Duration::from_secs(20)).min(deadline);
                loop {
                    if rt
                        .probe_frame_state(id)
                        .is_some_and(|p| p.incomplete_pending)
                    {
                        break;
                    }
                    if Instant::now() >= arm_until {
                        return Err("resize exhaustion refusal was never armed".into());
                    }
                    step(cx, rt, receiver, stats, out, &mut composition).await?;
                }
                // Passive sizing only: Window::resize would activate the window and
                // the detached passive reading requires it to stay unfocused.
                update(cx, root, |host, _, app| {
                    host.extent = (exhaust_extent.0 as f32, exhaust_extent.1 as f32);
                    app.notify();
                })?;
                if let Some(window) = &passive {
                    window.resize((ecw, ech))?;
                }
                let exhaust_until = (Instant::now() + Duration::from_secs(120)).min(deadline);
                let mut attempts = 0u8;
                let mut exhaustions = 0u64;
                loop {
                    step(cx, rt, receiver, stats, out, &mut composition).await?;
                    if let Ok(d) = rt.frame_recovery_diagnostics(id) {
                        attempts = d.attempts;
                    }
                    // Judge the product's own event, not a polled counter: the sink
                    // records ResizeExhausted when it publishes, so no polling gap
                    // can miss it.
                    exhaustions = stats.capture_events.lock().unwrap().count_kind(
                        id,
                        rust_cef_gpui::ports::CaptureFailureKind::ResizeExhausted,
                    ) as u64;
                    if exhaustions >= 1 {
                        break;
                    }
                    if Instant::now() >= exhaust_until {
                        break;
                    }
                }
                let held = rt
                    .frame_source(id)
                    .map_err(|e| format!("resize exhaustion source: {e:?}"))?
                    .snapshot()
                    .view
                    .frame
                    .as_ref()
                    .map(|f| (f.info.width, f.info.height));
                // Release before restoring the size, so the recovery frame can be
                // published; the refusal cannot be left armed past exhaustion.
                rt.probe_frame_control(
                    id,
                    rust_cef_gpui::test_support::FrameProbeControl::ReleaseIncompleteRefusals,
                )
                .map_err(|e| format!("resize exhaustion release: {e:?}"))?;
                let release_until = (Instant::now() + Duration::from_secs(20)).min(deadline);
                loop {
                    if rt
                        .probe_frame_state(id)
                        .is_some_and(|p| !p.incomplete_pending)
                    {
                        break;
                    }
                    if Instant::now() >= release_until {
                        return Err("resize exhaustion refusal release never applied".into());
                    }
                    step(cx, rt, receiver, stats, out, &mut composition).await?;
                }
                let (rw, rh) = target(extent, scale);
                update(cx, root, |host, _, app| {
                    host.extent = (extent.0 as f32, extent.1 as f32);
                    app.notify();
                })?;
                if let Some(window) = &passive {
                    window.resize((cw, ch))?;
                }
                let recover_until = (Instant::now() + Duration::from_secs(30)).min(deadline);
                let mut recovered: Option<(u32, u32)> = None;
                while Instant::now() < recover_until {
                    step(cx, rt, receiver, stats, out, &mut composition).await?;
                    let snap = rt
                        .frame_source(id)
                        .map_err(|e| format!("resize exhaustion source: {e:?}"))?
                        .snapshot();
                    if let Some(f) = snap.view.frame.as_ref()
                        && (f.info.width, f.info.height) == (rw, rh)
                    {
                        recovered = Some((f.info.width, f.info.height));
                        break;
                    }
                }
                let case_dir = options.report.parent().ok_or("resize case missing")?;
                atomic(
                    &case_dir.join("resize-exhaustion.json"),
                    &object(vec![
                        ("schema", "1".into()),
                        ("kind", quote("resize-retry-exhaustion")),
                        ("pid", std::process::id().to_string()),
                        ("attempts", attempts.to_string()),
                        ("exhaustions", exhaustions.to_string()),
                        (
                            "held",
                            held.map_or_else(|| "null".into(), |(w, h)| format!("[{w},{h}]")),
                        ),
                        ("refused", format!("[{tw},{th}]")),
                        ("refused_published", bool_json(held == Some((tw, th)))),
                        (
                            "recovered_after_new_target",
                            recovered.map_or_else(|| "null".into(), |(w, h)| format!("[{w},{h}]")),
                        ),
                        ("recover_target", format!("[{rw},{rh}]")),
                    ]),
                )?;
                if exhaustions < 1 || recovered.is_none() {
                    return Err(format!(
                        "resize exhaustion/recovery not observed: attempts={attempts} exhaustions={exhaustions} recovered={recovered:?}"
                    ));
                }
                out.resize_exhaustion_verified = true;
            }
            if actual_scale == scale
                && browser.state == SessionState::Alive
                && !browser.navigation.is_loading
                && frame_ok
            {
                let f = source.view.frame.as_ref().ok_or("resize frame lost")?;
                #[cfg(feature = "validation")]
                if let Some(transitions) = transitions.as_mut()
                    && !transitions.full_frame(rt, f)?
                {
                    step(cx, rt, receiver, stats, out, &mut composition).await?;
                    continue;
                }
                prior = Some((f.info.surface_generation, f.info.sequence));
                let case = options.report.parent().ok_or("resize case missing")?;
                std::fs::write(
                    case.join(format!("resize-stage-{stage}-view.bgra")),
                    f.pixels.as_slice(),
                )
                .map_err(|e| e.to_string())?;
                let manual_stage = None;
                #[cfg(feature = "validation")]
                let manual_stage = if let Some((probe, facts)) = manual.filter(|_| stage == 0) {
                    let info = probe.info()?;
                    let thread = probe.owner_thread()?;
                    facts.require_original_manual(&info, thread, probe)?;
                    let window = object(vec![
                        ("hwnd", info.hwnd.to_string()),
                        ("pid", info.pid.to_string()),
                        ("thread", thread.to_string()),
                        ("dpi", info.dpi.to_string()),
                        ("left", info.left.to_string()),
                        ("top", info.top.to_string()),
                        ("width", info.width.to_string()),
                        ("height", info.height.to_string()),
                    ]);
                    Some((window, facts.json(options)))
                } else {
                    manual_stage
                };
                #[cfg(feature = "validation")]
                let passive_stage = passive.as_ref().map(|w| w.evidence((cw, ch))).transpose()?;
                #[cfg(not(feature = "validation"))]
                let passive_stage = None;
                let text = metadata(
                    stage,
                    id,
                    extent,
                    scale,
                    &d,
                    Some(f),
                    manual_stage,
                    passive_stage,
                );
                out.paint = Some(d);
                observe(
                    cx,
                    rt,
                    receiver,
                    stats,
                    out,
                    &mut composition,
                    options,
                    deadline,
                    stage,
                    &text,
                )
                .await?;
                #[cfg(feature = "validation")]
                if let Some((probe, facts)) = manual.filter(|_| stage == 0) {
                    facts.require_original_manual(&probe.info()?, probe.owner_thread()?, probe)?;
                }
                break;
            }
            step(cx, rt, receiver, stats, out, &mut composition).await?;
        }
    }
    // Persist the mid-animation 1:1 audit: any frame whose device pixels do not
    // match the view's device extent is a resampled blit and fails the check.
    let drag_case = options.report.parent().ok_or("resize case missing")?;
    let resampled = drag_quality
        .iter()
        .filter(|&&(_, w, h, ew, eh)| w != ew || h != eh)
        .count();
    atomic(
        &drag_case.join("resize-drag-quality.json"),
        &object(vec![
            ("schema", "1".into()),
            ("kind", quote("resize-drag-quality")),
            ("pid", std::process::id().to_string()),
            (
                "frames",
                format!(
                    "[{}]",
                    drag_quality
                        .iter()
                        .map(|&(s, w, h, ew, eh)| format!("[{s},{w},{h},{ew},{eh}]"))
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            ),
            ("observed", drag_quality.len().to_string()),
            ("resampled", resampled.to_string()),
        ]),
    )?;
    if drag_quality.is_empty() || resampled > 0 {
        return Err(format!(
            "resize drag-quality failed: observed={} resampled={resampled}",
            drag_quality.len()
        ));
    }
    let weak = entity.downgrade();
    let (released, renders_before_detach) = update(cx, root, |host, w, app| {
        let view = host.browser.take().ok_or("resize host lost View")?;
        let result = view.update(app, |v, app| {
            v.release_image(w, app)
                .map_err(|e| format!("resize release: {e:?}"))?;
            Ok::<_, String>(v.diagnostics())
        });
        host.detached = true;
        app.notify();
        result.map(|diagnostics| (diagnostics, host.renders))
    })??;
    if !composition.unregister(id) {
        return Err("resize route was not registered at release".into());
    }
    drop(entity);
    if released.created_images == 0
        || released.created_images != released.released_images
        || released.last_error.is_some()
    {
        return Err("resize explicit image release unbalanced".into());
    }
    out.released_view = Some(released.clone());
    // The previous GPUI input handler can still own the Entity until the
    // detached root is drawn. Neither dropping our handle nor rendering
    // alone satisfies the terminal evidence required for the last image.
    let retire_deadline = (Instant::now() + WAIT).min(deadline);
    loop {
        if !app_update(cx, |app| app.windows().contains(&root.into()))? {
            return Err("resize original root disappeared before detached paint".into());
        }
        let root_repainted = update(cx, root, |host, _, _| {
            host.detached && host.browser.is_none() && host.renders > renders_before_detach
        })?;
        let entity_released = weak.upgrade().is_none();
        if Instant::now() >= retire_deadline {
            return Err(format!(
                "resize retirement timeout: root_repainted={root_repainted} entity_released={entity_released}"
            ));
        }
        if root_repainted && entity_released {
            out.detached_alive = true;
            break;
        }
        step(cx, rt, receiver, stats, out, &mut composition).await?;
    }
    #[cfg(feature = "validation")]
    let detached_passive = passive
        .as_ref()
        .map(|w| w.evidence(client_size(EXTENTS[4])))
        .transpose()?;
    #[cfg(not(feature = "validation"))]
    let detached_passive = None;
    observe(
        cx,
        rt,
        receiver,
        stats,
        out,
        &mut composition,
        options,
        deadline,
        5,
        &metadata(
            5,
            id,
            EXTENTS[4],
            scale,
            &released,
            None,
            None,
            detached_passive,
        ),
    )
    .await?;
    while !matches!(
        handle.snapshot().state,
        SessionState::Closing | SessionState::Closed
    ) {
        if Instant::now() >= deadline {
            return Err("resize release did not request close".into());
        }
        step(cx, rt, receiver, stats, out, &mut composition).await?;
    }
    out.view_release_observed_closing = true;
    while !options.gate.is_file() {
        if Instant::now() >= deadline {
            return Err("resize final observer timeout".into());
        }
        step(cx, rt, receiver, stats, out, &mut composition).await?;
    }
    if std::fs::read_to_string(&options.gate)
        .map_err(|e| e.to_string())?
        .trim()
        != "release"
    {
        return Err("resize final observer gate mismatch".into());
    }
    #[cfg(feature = "validation")]
    let verified_transitions = if let Some(transitions) = transitions.as_ref() {
        while handle.snapshot().state != SessionState::Closed {
            if Instant::now() >= deadline {
                return Err("verified Resize CEF retirement timeout".into());
            }
            step(cx, rt, receiver, stats, out, &mut composition).await?;
        }
        transitions.retired(rt)?
    } else {
        "null".into()
    };
    #[cfg(not(feature = "validation"))]
    let verified_transitions = "null".into();
    out.resize_detail = Some(object(vec![
        ("kind", quote("window-layout-resize-slice")),
        ("verified_transitions", verified_transitions),
        ("success", bool_json(true)),
        ("stages", "6".into()),
        ("task_11_3_accepted", bool_json(false)),
        ("physical_dpi", (scale * 96.).to_string()),
        (
            "passive_noactivation_resize",
            bool_json(options.resize_passive),
        ),
        (
            "passive_right_initial_placement",
            bool_json(options.resize_passive_right),
        ),
        ("minimize_verified", bool_json(false)),
        ("unfocused_animation_verified", bool_json(false)),
        ("recovery_exhaustion_verified", bool_json(false)),
    ]));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn passive_right_layout_reserves_largest_stage_inside_one_work_area() {
        let window = |x, y, w, h| Bounds::new(gpui::point(px(x), px(y)), size(px(w), px(h)));
        let result = passive_right_bounds(window(-40., 5., 1463., 775.)).unwrap();
        assert_eq!(result, window(763., 45., 400., 360.));
        assert!(passive_right_bounds(window(0., 0., 700., 800.)).is_err());
        assert!(passive_right_bounds(window(0., 0., 1463., 520.)).is_err());
        assert!(passive_right_bounds(window(0., 0., f32::NAN, 775.)).is_err());
    }

    #[test]
    fn fixed_resize_targets_use_independent_scale_not_painted_width() {
        assert_eq!(EXTENTS.len(), 5);
        assert_eq!(target((16, 12), 1.5), (24, 18));
        assert_eq!(target((64, 48), 1.1), (71, 53));
        assert_eq!(client_size((16, 12)), (200, 180));
        assert_eq!(client_size((500, 300)), (580, 420));
    }
}
