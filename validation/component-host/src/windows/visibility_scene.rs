//! Non-default validation-only, host-owned Browser visibility scene.
//!
//! Four stages: initial visible static page and independent desktop/owner
//! observation, real GPUI minimize + ordinary SW_RESTORE with the Browser
//! hidden for at least 250 ms, restored source recovery with an actual GPUI
//! paint, then explicit View/image/entity release and a green detached stage.
//!
//! This module is a validation host scene, not component policy. It never
//! requests foreground, never calls `activate_window`, never sends input and
//! never restores the window implicitly on failure. Ordinary `SW_RESTORE` may
//! activate the owned window; that is explicitly approved by the caller's
//! Visibility-only flag and is not a foreground request. A failure latches:
//! no retry, no implicit restore and no absorption of CaptureFailed.

use super::pointer_probe::{PointerProbe, WindowInfo};
use super::visibility::OwnerVisibility;
use super::*;
use rust_cef_gpui::{UiComposition, contracts::SessionState};
use std::num::NonZeroUsize;

const WAIT: Duration = Duration::from_secs(20);
const QUIET_MIN_MS: u128 = 250;
const VIEW_W: i32 = 320;
const VIEW_H: i32 = 240;

fn dims(scale: f32) -> (u32, u32) {
    (
        (VIEW_W as f32 * scale).ceil() as u32,
        (VIEW_H as f32 * scale).ceil() as u32,
    )
}

fn viewport(scale: f32) -> Viewport {
    Viewport {
        width: VIEW_W,
        height: VIEW_H,
        scale,
    }
}

fn bid(id: BrowserId) -> String {
    format!("[{},{}]", id.runtime().value(), id.local())
}

/// `Path::with_extension("pending")` is a sibling path; the shared helper
/// writes it with create_new and publishes by hard link, never overwriting an
/// existing evidence target.
fn atomic(path: &Path, text: &str) -> Result<(), String> {
    super::exhaustion_evidence::atomic(path, text.as_bytes())
}

/// The window object keeps the exact field shape the other validation stage
/// contracts use. Host-native minimize observations are reported separately and
/// never substitute for the external stage1 IsIconic identity.
fn window_json(info: &WindowInfo, thread: u32) -> String {
    object(vec![
        ("hwnd", info.hwnd.to_string()),
        ("pid", info.pid.to_string()),
        ("thread", thread.to_string()),
        ("dpi", info.dpi.to_string()),
        ("left", info.left.to_string()),
        ("top", info.top.to_string()),
        ("width", info.width.to_string()),
        ("height", info.height.to_string()),
    ])
}

/// One read-only source observation. `None` values mean "not observed", never
/// a guessed/default value; a query error is carried explicitly.
struct SourceMeta {
    closed: Option<bool>,
    visible: Option<bool>,
    popup_visible: Option<bool>,
    popup_frame: Option<bool>,
    generation: Option<u64>,
    viewport: Option<Viewport>,
    error: Option<String>,
}

impl SourceMeta {
    fn ok(
        closed: bool,
        visible: bool,
        popup_visible: bool,
        popup_frame: bool,
        generation: u64,
        viewport: Option<Viewport>,
    ) -> Self {
        Self {
            closed: Some(closed),
            visible: Some(visible),
            popup_visible: Some(popup_visible),
            popup_frame: Some(popup_frame),
            generation: Some(generation),
            viewport,
            error: None,
        }
    }
    fn fields(&self) -> Vec<(&'static str, String)> {
        vec![
            (
                "source_closed",
                self.closed.map_or_else(|| "null".into(), bool_json),
            ),
            (
                "source_visible",
                self.visible.map_or_else(|| "null".into(), bool_json),
            ),
            (
                "popup_visible",
                self.popup_visible.map_or_else(|| "null".into(), bool_json),
            ),
            (
                "popup_frame_present",
                self.popup_frame.map_or_else(|| "null".into(), bool_json),
            ),
            (
                "source_generation",
                self.generation
                    .map_or_else(|| "null".into(), |v| v.to_string()),
            ),
            (
                "source_viewport",
                self.viewport.map_or_else(
                    || "null".into(),
                    |v| {
                        object(vec![
                            ("width", v.width.to_string()),
                            ("height", v.height.to_string()),
                            ("scale", v.scale.to_string()),
                        ])
                    },
                ),
            ),
            (
                "source_query_error",
                self.error.as_deref().map_or_else(|| "null".into(), quote),
            ),
        ]
    }
}

fn save_raw(options: &Options, stage: u32, frame: &OwnedFrame) -> Result<String, String> {
    let case = options.report.parent().ok_or("visibility case missing")?;
    let name = format!("visibility-stage-{stage}-view.bgra");
    super::exhaustion_evidence::atomic(&case.join(&name), frame.pixels.as_slice())?;
    Ok(object(vec![
        ("file", quote(&name)),
        ("browser", bid(frame.info.browser)),
        ("generation", frame.info.surface_generation.to_string()),
        ("sequence", frame.info.sequence.to_string()),
        ("width", frame.info.width.to_string()),
        ("height", frame.info.height.to_string()),
        ("bytes", frame.pixels.len().to_string()),
    ]))
}

fn recovery_fields(rt: &Runtime, id: BrowserId) -> (String, String) {
    match rt.frame_recovery_diagnostics(id) {
        Ok(r) => (super::exhaustion_evidence::recovery_json(&r), "null".into()),
        Err(e) => ("null".into(), quote(&format!("{e:?}"))),
    }
}

#[allow(clippy::too_many_arguments)]
fn stage_json(
    stage: u32,
    phase: &'static str,
    id: BrowserId,
    scale: f32,
    window: String,
    source: SourceMeta,
    diagnostics: String,
    raw: String,
    recovery: (String, String),
    bridge: String,
    detached: bool,
    extra: Vec<(&'static str, String)>,
) -> String {
    let mut fields: Vec<(&'static str, String)> = vec![
        ("schema", "1".into()),
        ("kind", quote("visibility-stage")),
        ("scope", quote("host-owned-visibility-slice")),
        ("pid", std::process::id().to_string()),
        ("stage", stage.to_string()),
        ("phase", quote(phase)),
        ("browser", bid(id)),
        ("window", window),
        ("scale_factor", scale.to_string()),
        ("view_bounds", format!("[0,48,{VIEW_W},{VIEW_H}]")),
        ("diagnostics", diagnostics),
        ("view_raw", raw),
        ("recovery", recovery.0),
        ("recovery_error", recovery.1),
        ("bridge", bridge),
        ("detached", bool_json(detached)),
        ("task_11_3_accepted", bool_json(false)),
        ("input_sent", bool_json(false)),
        ("foreground_forced", bool_json(false)),
        ("observer_resized", bool_json(false)),
        ("popup_clear_transition_verified", bool_json(false)),
    ];
    fields.extend(source.fields());
    fields.extend(extra);
    object(fields)
}

fn ready_path(options: &Options, stage: u32) -> PathBuf {
    options
        .gate
        .with_extension(format!("visibility-{stage}.ready"))
}

fn release_path(options: &Options, stage: u32) -> PathBuf {
    options
        .gate
        .with_extension(format!("visibility-{stage}.release"))
}

fn publish(options: &Options, stage: u32, text: &str) -> Result<(), String> {
    let case = options.report.parent().ok_or("visibility case missing")?;
    atomic(&case.join(format!("visibility-stage-{stage}.json")), text)?;
    if stage == 0 {
        // Shared launcher starts its independent desktop/owner observer only
        // after this main gate.
        atomic(
            &options.gate.with_extension("ready"),
            &std::process::id().to_string(),
        )?;
    }
    atomic(&ready_path(options, stage), &std::process::id().to_string())
}

fn check_release_pid(path: &Path) -> Result<(), String> {
    if std::fs::read_to_string(path)
        .map_err(|e| e.to_string())?
        .trim()
        != std::process::id().to_string()
    {
        return Err("visibility stage release PID mismatch".into());
    }
    Ok(())
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
            return Err(format!("visibility dispatch: {report:?}"));
        }
    }
    stats.check()
}

#[allow(clippy::too_many_arguments)]
async fn wait_release(
    cx: &mut AsyncApp,
    rt: &mut Runtime,
    receiver: &UiWakeReceiver,
    stats: &Stats,
    out: &mut Outcome,
    composition: &mut UiComposition,
    options: &Options,
    stage: u32,
    deadline: Instant,
) -> Result<(), String> {
    let release = release_path(options, stage);
    let until = (Instant::now() + WAIT).min(deadline);
    while !release.is_file() {
        if Instant::now() >= until {
            return Err(format!("visibility stage {stage} observer timeout"));
        }
        step(cx, rt, receiver, stats, out, composition).await?;
    }
    check_release_pid(&release)
}

/// Stage 1 must keep observing the hidden interval on every poll while the
/// external observer holds its release. Any bridge error is fatal, matching
/// the permanently-ineligible quiet rule.
#[allow(clippy::too_many_arguments)]
async fn wait_release_hidden(
    cx: &mut AsyncApp,
    rt: &mut Runtime,
    receiver: &UiWakeReceiver,
    stats: &Stats,
    out: &mut Outcome,
    composition: &mut UiComposition,
    options: &Options,
    probe: &PointerProbe,
    bridge: &mut OwnerVisibility,
    stage: u32,
    deadline: Instant,
) -> Result<(), String> {
    let release = release_path(options, stage);
    let until = (Instant::now() + WAIT).min(deadline);
    while !release.is_file() {
        if Instant::now() >= until {
            return Err(format!("visibility stage {stage} observer timeout"));
        }
        step(cx, rt, receiver, stats, out, composition).await?;
        bridge.observe_hidden(rt, probe)?;
    }
    check_release_pid(&release)
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
    initial_foreground: &super::exhaustion::InitialForeground,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(100);
    let mut composition = UiComposition::new(NonZeroUsize::new(1).ok_or("zero visibility route")?);
    while rt.state() != RuntimeState::Ready {
        if Instant::now() >= deadline {
            return Err("visibility Runtime readiness timeout".into());
        }
        step(cx, rt, receiver, stats, out, &mut composition).await?;
    }
    let scale = update(cx, root, |_, w, _| w.scale_factor())?;
    if !scale.is_finite() || scale <= 0. {
        return Err("visibility invalid actual scale".into());
    }
    let vp = viewport(scale);
    let (tw, th) = dims(scale);
    let handle = rt
        .create_browser(BrowserConfig {
            // Same static three-colour page as the exhaustion scene.
            initial_url: super::exhaustion::page(),
            profile: None,
            render_preference: options.backend,
            viewport: vp,
            options: Default::default(),
        })
        .map_err(|e| format!("visibility create: {e:?}"))?;
    let id = handle.snapshot().browser;
    out.browser = Some(id);
    let until = (Instant::now() + WAIT).min(deadline);
    while handle.snapshot().state != SessionState::Alive {
        if handle.snapshot().state != SessionState::Creating || Instant::now() >= until {
            return Err("visibility Browser creation incomplete".into());
        }
        step(cx, rt, receiver, stats, out, &mut composition).await?;
    }
    let attachment = rt
        .attach_browser(&handle)
        .map_err(|e| format!("visibility attach: {e:?}"))?;
    out.duplicate_refused = rt.attach_browser(&handle).is_err();
    if !out.duplicate_refused {
        return Err("visibility duplicate attachment accepted".into());
    }
    let entity = update(cx, root, move |host, w, app| {
        let view = CefBrowser::new(attachment, w, app)?;
        host.browser = Some(view.clone());
        app.notify();
        Ok::<_, rust_cef_gpui::ViewError>(view)
    })?
    .map_err(|e| format!("visibility View: {e:?}"))?;
    update(cx, root, |_, w, app| {
        composition.register(&entity, w, &*app)
    })?
    .map_err(|e| format!("visibility register: {e:?}"))?;
    let initial_dispatch = app_update(cx, |app| composition.dispatch(app))?;
    if initial_dispatch.attempted != 1
        || initial_dispatch.removed != 0
        || !initial_dispatch.failures.is_empty()
    {
        return Err(format!("visibility initial dispatch: {initial_dispatch:?}"));
    }

    // Stage 0: initial visible source paint plus the host window facts an
    // independent observer must match to the same HWND.
    let until = (Instant::now() + WAIT).min(deadline);
    let mut last = String::new();
    let (d0, facts0, raw0) = loop {
        if Instant::now() >= until {
            return Err(format!("visibility initial frame timeout: {last}"));
        }
        rendered(cx, root).await?;
        let d = update(cx, root, |_, _, app| entity.read(app).diagnostics())?;
        last = metrics_json(&d);
        let hit = {
            let s = rt
                .frame_source(id)
                .map_err(|e| format!("visibility source: {e:?}"))?
                .snapshot();
            let browser = handle.snapshot();
            let frame_ok = s.view.frame.as_ref().is_some_and(|f| {
                f.info.browser == id
                    && f.info.surface == SurfaceKind::View
                    && f.info.surface_generation == s.view.generation
                    && (f.info.width, f.info.height) == (tw, th)
                    && d.painted_generation == f.info.surface_generation
                    && d.painted_sequence == f.info.sequence
            });
            if browser.state == SessionState::Alive
                && !browser.navigation.is_loading
                && !s.closed
                && s.view.visible
                && s.viewport == Some(vp)
                && d.frame_width == tw
                && d.frame_height == th
                && d.surface_bounds == (0., 48., VIEW_W as f32, VIEW_H as f32)
                && d.scale_factor == scale
                && d.paint_calls > 0
                && d.last_error.is_none()
                && !d.pending_resync
                && frame_ok
                && !s.popup.visible
                && s.popup.frame.is_none()
            {
                s.view.frame.as_ref().map(|f| {
                    (
                        f.clone(),
                        SourceMeta::ok(
                            s.closed,
                            s.view.visible,
                            s.popup.visible,
                            s.popup.frame.is_some(),
                            s.view.generation,
                            s.viewport,
                        ),
                    )
                })
            } else {
                None
            }
        };
        if let Some((frame, facts)) = hit {
            let raw = save_raw(options, 0, &frame)?;
            // The source Arc dies with this iteration; no pixel lease is held
            // across the observer handshake.
            drop(frame);
            break (d, facts, raw);
        }
        step(cx, rt, receiver, stats, out, &mut composition).await?;
    };
    let info0 = probe.info()?;
    if info0.pid != std::process::id() {
        return Err("visibility PointerProbe owner PID mismatch".into());
    }
    let thread = probe.owner_thread()?;
    if options.visibility_manual_foreground {
        initial_foreground.require_original_manual(&info0, thread, probe)?;
    }
    if !update(cx, root, |_, w, _| w.is_visible())? {
        return Err("visibility root window is not visible at stage0".into());
    }
    let recovery0 = recovery_fields(rt, id);
    // Binding is a qualification query only: it reads the original owner and
    // native visible/iconic state and does not minimize or post anything. The
    // single minimize stays after the stage0 external observer release.
    let mut bridge = OwnerVisibility::bind(root, handle.clone(), probe, vp)?;
    let text0 = stage_json(
        0,
        "initial-frame-and-observer",
        id,
        scale,
        window_json(&info0, thread),
        facts0,
        metrics_json(&d0),
        raw0,
        recovery0,
        bridge.evidence_json(),
        false,
        vec![
            ("probe_title", quote(TITLE)),
            (
                "owner_window_source",
                quote(
                    "PointerProbe original owner; external observer must confirm the same HWND/pid/client",
                ),
            ),
            ("external_same_hwnd_confirmation_required", bool_json(true)),
            ("composition_route_registered", bool_json(true)),
            (
                "initial_dispatch_attempted",
                initial_dispatch.attempted.to_string(),
            ),
            (
                "duplicate_attachment_refused",
                bool_json(out.duplicate_refused),
            ),
            ("root_client_600x450_scene", bool_json(true)),
            ("initial_foreground", initial_foreground.json(options)),
        ],
    );
    publish(options, 0, &text0)?;
    wait_release(
        cx,
        rt,
        receiver,
        stats,
        out,
        &mut composition,
        options,
        0,
        deadline,
    )
    .await?;

    // No window operation before the independent stage-0 proof; the
    // manual path must still name the original foreground and geometry.
    if options.visibility_manual_foreground {
        initial_foreground.require_original_manual(&probe.info()?, probe.owner_thread()?, probe)?;
    }
    bridge.minimize(cx, probe)?; // One request only.
    let until = (Instant::now() + WAIT).min(deadline);
    loop {
        if bridge.hide_if_minimized(rt, probe)? {
            break;
        }
        if Instant::now() >= until {
            return Err("visibility owned window did not become iconic".into());
        }
        step(cx, rt, receiver, stats, out, &mut composition).await?;
    }
    // Establishment may span several polls; observe on every poll.
    let until = (Instant::now() + WAIT).min(deadline);
    loop {
        if Instant::now() >= until {
            return Err(format!(
                "visibility hidden mailbox timeout: {}",
                bridge.evidence_json()
            ));
        }
        step(cx, rt, receiver, stats, out, &mut composition).await?;
        if bridge.observe_hidden(rt, probe)? {
            break;
        }
    }
    // One successful observation starts the span; only further successful
    // observations extend it. The bridge, not this loop's clock, owns the
    // >= 250 ms eligibility test.
    let until = (Instant::now() + WAIT).min(deadline);
    while !bridge.quiet_ready() {
        if Instant::now() >= until {
            return Err(format!(
                "visibility hidden quiet timeout: {}",
                bridge.evidence_json()
            ));
        }
        step(cx, rt, receiver, stats, out, &mut composition).await?;
        bridge.observe_hidden(rt, probe)?;
    }
    // A successful observation immediately before the snapshot is what pins
    // the retained Arc; only then can stage1 raw be the actual pre-hide frame.
    bridge.observe_hidden(rt, probe)?;
    let (facts1, raw1) = {
        let s = rt
            .frame_source(id)
            .map_err(|e| format!("visibility hidden source: {e:?}"))?
            .snapshot();
        if s.closed || s.view.visible || s.viewport != Some(vp) {
            return Err("visibility hidden source identity/viewport mismatch".into());
        }
        if s.popup.visible || s.popup.frame.is_some() {
            return Err("visibility hidden popup was not clear".into());
        }
        let frame = s
            .view
            .frame
            .as_ref()
            .ok_or("visibility hidden source frame missing")?;
        let raw = save_raw(options, 1, frame)?;
        let facts = SourceMeta::ok(
            s.closed,
            s.view.visible,
            s.popup.visible,
            s.popup.frame.is_some(),
            s.view.generation,
            s.viewport,
        );
        // Stage 0 raw need not be the same Arc as the actual pre-hide retained
        // frame (a natural first complete frame may arrive later). Stage 1 raw
        // is the retained frame the bridge itself pins with Arc identity.
        (facts, raw)
    };
    // Close the observation gap between our snapshot and the bridge's last
    // observation: if the hidden mailbox still retains the same Arc, the raw
    // above is that retained frame. A mismatch latches the bridge as Failed.
    bridge.observe_hidden(rt, probe)?;
    let d1 = update(cx, root, |_, _, app| entity.read(app).diagnostics())?;
    let recovery1 = recovery_fields(rt, id);
    let text1 = stage_json(
        1,
        "hidden-quiet",
        id,
        scale,
        window_json(&info0, thread),
        facts1,
        metrics_json(&d1),
        raw1,
        recovery1,
        bridge.evidence_json(),
        false,
        vec![
            ("window_geometry_from_stage0_visible_state", bool_json(true)),
            ("host_observed_iconic_before_hide", bool_json(true)),
            (
                "window_geometry_scope",
                quote(
                    "stage0 visible client geometry; a minimized window has no comparable client rect",
                ),
            ),
            (
                "minimized_observed_by",
                quote(
                    "external observer IsIconic identity; no minimized screenshot is required or claimed",
                ),
            ),
            ("external_is_iconic_required", bool_json(true)),
            ("stage0_raw_same_arc_claimed", bool_json(false)),
            ("retained_matches_source_arc", bool_json(true)),
            (
                "retained_identity_from_source_after_hidden_observation",
                bool_json(true),
            ),
            ("hidden_quiet_minimum_ms", QUIET_MIN_MS.to_string()),
        ],
    );
    publish(options, 1, &text1)?;
    wait_release_hidden(
        cx,
        rt,
        receiver,
        stats,
        out,
        &mut composition,
        options,
        probe,
        &mut bridge,
        1,
        deadline,
    )
    .await?;
    // Final recheck after the observer release, then exactly one ordinary
    // restore request; the bridge freezes the measured hidden span.
    bridge.observe_hidden(rt, probe)?;
    if !bridge.quiet_ready() {
        return Err("visibility hidden quiet was not retained to restore".into());
    }
    bridge.restore_after_quiet(rt, probe)?;
    let until = (Instant::now() + WAIT).min(deadline);
    loop {
        if bridge.show_if_restored(probe)? {
            break;
        }
        if Instant::now() >= until {
            return Err("visibility native restore did not become visible".into());
        }
        step(cx, rt, receiver, stats, out, &mut composition).await?;
    }
    let until = (Instant::now() + WAIT).min(deadline);
    let fresh = loop {
        if Instant::now() >= until {
            return Err(format!(
                "visibility source recovery timeout: {}",
                bridge.evidence_json()
            ));
        }
        if let Some(frame) = bridge.observe_source_recovered(rt, probe)? {
            break frame;
        }
        step(cx, rt, receiver, stats, out, &mut composition).await?;
    };

    // Stage 2: the recovered source generation/sequence must be the one GPUI
    // actually paints. The full desktop image remains the external observer's
    // evidence; this host writes only the owned source raw.
    let until = (Instant::now() + WAIT).min(deadline);
    let (d2, facts2, raw2, witness) = loop {
        if Instant::now() >= until {
            return Err("visibility recovered paint timeout".into());
        }
        rendered(cx, root).await?;
        let d = update(cx, root, |_, _, app| entity.read(app).diagnostics())?;
        let hit = {
            let s = rt
                .frame_source(id)
                .map_err(|e| format!("visibility recovered source: {e:?}"))?
                .snapshot();
            let browser = handle.snapshot();
            let frame_ok = s.view.frame.as_ref().is_some_and(|f| {
                f.info.browser == id
                    && f.info.surface == SurfaceKind::View
                    && f.info.surface_generation == s.view.generation
                    && s.view.generation >= fresh.info.surface_generation
                    && f.info.sequence >= fresh.info.sequence
                    && (f.info.width, f.info.height) == (tw, th)
                    && d.painted_generation == f.info.surface_generation
                    && d.painted_sequence == f.info.sequence
            });
            if browser.state == SessionState::Alive
                && !browser.navigation.is_loading
                && !s.closed
                && s.view.visible
                && s.viewport == Some(vp)
                && d.frame_width == tw
                && d.frame_height == th
                && d.surface_bounds == (0., 48., VIEW_W as f32, VIEW_H as f32)
                && d.scale_factor == scale
                && d.paint_calls > 0
                && d.last_error.is_none()
                && !d.pending_resync
                && frame_ok
            {
                s.view.frame.as_ref().map(|f| {
                    (
                        f.clone(),
                        SourceMeta::ok(
                            s.closed,
                            s.view.visible,
                            s.popup.visible,
                            s.popup.frame.is_some(),
                            s.view.generation,
                            s.viewport,
                        ),
                    )
                })
            } else {
                None
            }
        };
        if let Some((frame, facts)) = hit {
            let raw = save_raw(options, 2, &frame)?;
            break (
                d,
                facts,
                raw,
                (frame.info.surface_generation, frame.info.sequence),
            );
        }
        step(cx, rt, receiver, stats, out, &mut composition).await?;
    };
    let info2 = probe.info()?;
    if info2.hwnd != info0.hwnd || info2.pid != info0.pid || probe.owner_thread()? != thread {
        return Err("visibility restored owned HWND/thread changed".into());
    }
    out.paint = Some(d2.clone());
    let recovery2 = recovery_fields(rt, id);
    let text2 = stage_json(
        2,
        "restored-source-and-desktop",
        id,
        scale,
        window_json(&info2, thread),
        facts2,
        metrics_json(&d2),
        raw2,
        recovery2,
        bridge.evidence_json(),
        false,
        vec![
            (
                "bridge_recovered_witness",
                object(vec![
                    ("generation", fresh.info.surface_generation.to_string()),
                    ("sequence", fresh.info.sequence.to_string()),
                ]),
            ),
            (
                "recovered_paint_identity",
                object(vec![
                    ("generation", witness.0.to_string()),
                    ("sequence", witness.1.to_string()),
                ]),
            ),
            (
                "desktop",
                object(vec![
                    ("required", bool_json(true)),
                    ("captured_by", quote("external observer")),
                    (
                        "expected_external_file",
                        quote("visibility-stage-2-desktop.bgra"),
                    ),
                    ("verified_here", bool_json(false)),
                    (
                        "note",
                        quote(
                            "host source raw is not a desktop image; the full desktop capture and comparison remain external",
                        ),
                    ),
                ]),
            ),
            ("desktop_image_verified_here", bool_json(false)),
        ],
    );
    publish(options, 2, &text2)?;
    wait_release(
        cx,
        rt,
        receiver,
        stats,
        out,
        &mut composition,
        options,
        2,
        deadline,
    )
    .await?;

    // Retire and drop every host-owned pixel lease before the green stage.
    // Stage 3 must show the genuinely retired bridge, so only the after-retire
    // evidence is used there; the pre-retire snapshot is kept for stage2 only.
    bridge.retire()?;
    let retired_bridge = bridge.evidence_json();
    drop(fresh);
    drop(bridge);

    let weak = entity.downgrade();
    let (released, renders_before_detach) = update(cx, root, |host, w, app| {
        let view = host.browser.take().ok_or("visibility host lost View")?;
        let diagnostics = view.update(app, |v, app| {
            v.release_image(w, app)
                .map_err(|e| format!("visibility release: {e:?}"))?;
            Ok::<_, String>(v.diagnostics())
        })?;
        host.detached = true;
        app.notify();
        Ok::<_, String>((diagnostics, host.renders))
    })??;
    if !composition.unregister(id) {
        return Err("visibility route was not registered at release".into());
    }
    drop(entity);
    if released.created_images == 0
        || released.created_images != released.released_images
        || released.last_error.is_some()
    {
        return Err("visibility explicit image release unbalanced".into());
    }
    out.released_view = Some(released.clone());

    // A focused GPUI input handler can hold the old entity through the
    // rendered frame. Observe the original root drawing without that View,
    // and still require the old weak entity to expire before stage 3.
    let retire_deadline = (Instant::now() + WAIT).min(deadline);
    loop {
        if !app_update(cx, |app| app.windows().contains(&root.into()))? {
            return Err("visibility original root disappeared before detached paint".into());
        }
        if probe.owned_identity()? != (info0.hwnd, info0.pid, thread) {
            return Err("visibility original HWND owner changed before detached paint".into());
        }
        let root_repainted = update(cx, root, |host, _, _| {
            host.detached && host.browser.is_none() && host.renders > renders_before_detach
        })?;
        let entity_released = weak.upgrade().is_none();
        if Instant::now() >= retire_deadline {
            return Err(format!(
                "visibility retirement timeout: root_repainted={root_repainted} entity_released={entity_released}"
            ));
        }
        if root_repainted && entity_released {
            out.detached_alive = true;
            break;
        }
        step(cx, rt, receiver, stats, out, &mut composition).await?;
    }
    while !matches!(
        handle.snapshot().state,
        SessionState::Closing | SessionState::Closed
    ) {
        if Instant::now() >= deadline {
            return Err("visibility release did not request Closing".into());
        }
        step(cx, rt, receiver, stats, out, &mut composition).await?;
    }
    out.view_release_observed_closing = true;

    // Stage 3 must report the terminal source actually observed after the
    // explicit release. Closing is not proof the retained source is hidden, so
    // wait for the real Closed state inside the bounded 20 s/deadline, then
    // snapshot. No value is filled in from the release request.
    let until = (Instant::now() + WAIT).min(deadline);
    loop {
        if handle.snapshot().state == SessionState::Closed {
            break;
        }
        if Instant::now() >= until {
            return Err("visibility released Browser did not observe Closed".into());
        }
        step(cx, rt, receiver, stats, out, &mut composition).await?;
    }
    let info3 = probe.info()?;
    if info3.hwnd != info0.hwnd || info3.pid != info0.pid {
        return Err("visibility green-stage owned HWND changed".into());
    }
    let (source3, closed3, visible3) = {
        let s = rt
            .frame_source(id)
            .map_err(|e| format!("visibility terminal source: {e:?}"))?
            .snapshot();
        if !s.closed {
            return Err("visibility terminal source is not closed".into());
        }
        if s.view.visible {
            return Err("visibility terminal source View is still visible".into());
        }
        let facts = SourceMeta::ok(
            s.closed,
            s.view.visible,
            s.popup.visible,
            s.popup.frame.is_some(),
            s.view.generation,
            s.viewport,
        );
        (facts, s.closed, s.view.visible)
    };
    let recovery3 = recovery_fields(rt, id);
    let text3 = stage_json(
        3,
        "release-view-and-green",
        id,
        scale,
        window_json(&info3, thread),
        source3,
        metrics_json(&released),
        "null".into(),
        recovery3,
        retired_bridge.clone(),
        true,
        vec![
            ("released_images_balanced", bool_json(true)),
            ("observed_closed", bool_json(closed3)),
            ("observed_source_visible_false", bool_json(!visible3)),
            (
                "entity_and_route_retired",
                quote(
                    "unregister + release_image + remove + drop host entity + detached root repaint + Weak None + Closing",
                ),
            ),
        ],
    );
    publish(options, 3, &text3)?;
    wait_release(
        cx,
        rt,
        receiver,
        stats,
        out,
        &mut composition,
        options,
        3,
        deadline,
    )
    .await?;
    while !options.gate.is_file() {
        if Instant::now() >= deadline {
            return Err("visibility final observer timeout".into());
        }
        step(cx, rt, receiver, stats, out, &mut composition).await?;
    }
    if std::fs::read_to_string(&options.gate)
        .map_err(|e| e.to_string())?
        .trim()
        != "release"
    {
        return Err("visibility final observer gate mismatch".into());
    }
    out.visibility_detail = Some(object(vec![
        ("kind", quote("host-owned-visibility-slice")),
        ("scope", quote("host-owned-visibility-slice")),
        ("success", bool_json(true)),
        ("stages", "4".into()),
        ("task_11_3_accepted", bool_json(false)),
        ("native_acceptance_claimed", bool_json(false)),
        ("minimize_observed", bool_json(true)),
        ("hidden_quiet_verified", bool_json(true)),
        ("source_recovered", bool_json(true)),
        ("desktop_stage_external", bool_json(true)),
        ("popup_clear_transition_verified", bool_json(false)),
        ("input_sent", bool_json(false)),
        ("foreground_forced", bool_json(false)),
        ("observer_resized", bool_json(false)),
        (
            "stage_files",
            format!(
                "[{}]",
                (0..=3)
                    .map(|stage| quote(&format!("visibility-stage-{stage}.json")))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        ),
        ("final_bridge", retired_bridge),
        ("initial_foreground", initial_foreground.json(options)),
    ]));
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
    if !options.visibility_operations_approved {
        return Err(
            "visibility scene requires explicit Visibility-only window-operation approval".into(),
        );
    }
    let mut probe = PointerProbe::install(TITLE)?;
    let mut initial_foreground = super::exhaustion::InitialForeground::default();
    let result = async {
        if options.visibility_manual_foreground {
            super::exhaustion::observe_manual_foreground(
                cx,
                rt,
                receiver,
                root,
                &probe,
                &mut initial_foreground,
                "Visibility",
            )
            .await?;
        }
        inner(
            cx,
            rt,
            receiver,
            stats,
            root,
            options,
            out,
            &probe,
            &initial_foreground,
        )
        .await
    }
    .await;
    match (result, probe.retire()) {
        (Err(primary), Err(secondary)) => {
            eprintln!("visibility secondary probe retirement failure: {secondary}");
            Err(primary)
        }
        (Ok(()), Err(secondary)) => Err(format!("visibility probe retirement: {secondary}")),
        (result, Ok(())) => result,
    }
}
