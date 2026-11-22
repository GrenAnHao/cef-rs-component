//! Partial11.3: real owned-peer foreground, visible inactive-window animation.
//! No input/IME/minimize or automatic SetVisible policy; no foreground workaround.
use super::pointer_probe::{PointerProbe, WindowInfo};
use super::*;
use gpui::{AnyWindowHandle, point};
use rust_cef_gpui::{
    UiComposition,
    contracts::SessionState,
    ports::{BrowserCommand, BrowserHandle},
};
use std::num::NonZeroUsize;
const PEER: &str = "GPUI-CEF animation peer";
const WAIT: Duration = Duration::from_secs(20);
const LAST_TICK: u64 = 96;

fn page() -> String {
    let html = "<html><head><title>Unfocused|tick=0|done=0</title></head><body style='margin:0;background:rgb(26,76,126)'><div style='position:fixed;left:0;top:0;width:4px;height:100%;background:rgb(190,90,20)'></div><div style='position:fixed;left:0;top:0;width:100%;height:4px;background:rgb(110,40,180)'></div><div id='p' style='position:fixed;left:16px;top:16px;width:32px;height:32px;background:rgb(190,90,20)'></div><script>let n=0;const p=document.getElementById('p');const t=setInterval(()=>{n++;p.style.background=n%2?'rgb(190,90,20)':'rgb(20,150,170)';document.title='Unfocused|tick='+n+'|done='+(n===96?1:0);if(n===96)clearInterval(t)},50);</script></body></html>";
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
fn tick(title: &str) -> Result<(u64, bool), String> {
    let (n, done) = title
        .strip_prefix("Unfocused|tick=")
        .and_then(|s| s.split_once("|done="))
        .ok_or("animation numeric title missing")?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return Err("animation tick is not numeric".into());
    }
    let n = n.parse::<u64>().map_err(|_| "animation tick overflow")?;
    if n > LAST_TICK || !matches!(done, "0" | "1") || (done == "1") != (n == LAST_TICK) {
        return Err("animation terminal state invalid".into());
    }
    Ok((n, done == "1"))
}
fn digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(14695981039346656037, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(1099511628211)
    })
}
#[derive(Clone)]
struct Sample {
    elapsed_ms: u64,
    sequence: u64,
    generation: u64,
    digest: u64,
    tick: u64,
    width: u32,
    height: u32,
    painted_sequence: u64,
    painted_generation: u64,
    paint_calls: u64,
    active: bool,
    visible: bool,
    source_visible: bool,
}
fn progresses(s: &Sample, previous: Option<&Sample>) -> bool {
    !s.active
        && s.visible
        && s.source_visible
        && s.sequence > 0
        && s.generation > 0
        && s.tick > 0
        && s.tick < LAST_TICK
        && s.width > 0
        && s.height > 0
        && s.painted_sequence == s.sequence
        && s.painted_generation == s.generation
        && s.paint_calls > 0
        && previous.is_none_or(|p| {
            s.elapsed_ms.saturating_sub(p.elapsed_ms) >= 500
                && s.sequence > p.sequence
                && s.generation == p.generation
                && s.digest != p.digest
                && s.tick > p.tick
                && s.width == p.width
                && s.height == p.height
                && s.paint_calls > p.paint_calls
        })
}
fn sample_json(s: &Sample) -> String {
    object(vec![
        ("elapsed_ms", s.elapsed_ms.to_string()),
        ("sequence", s.sequence.to_string()),
        ("generation", s.generation.to_string()),
        ("digest", quote(&format!("{:016x}", s.digest))),
        (
            "file",
            quote(&format!("unfocused-sample-{}.bgra", s.sequence)),
        ),
        ("width", s.width.to_string()),
        ("height", s.height.to_string()),
        ("painted_sequence", s.painted_sequence.to_string()),
        ("painted_generation", s.painted_generation.to_string()),
        ("paint_calls", s.paint_calls.to_string()),
        ("observed_document_tick", s.tick.to_string()),
        ("active", bool_json(s.active)),
        ("visible", bool_json(s.visible)),
        ("source_visible", bool_json(s.source_visible)),
    ])
}
fn fact(f: &WindowInfo, thread: u32) -> String {
    object(vec![
        ("hwnd", f.hwnd.to_string()),
        ("pid", f.pid.to_string()),
        ("thread", thread.to_string()),
        ("dpi", f.dpi.to_string()),
        ("left", f.left.to_string()),
        ("top", f.top.to_string()),
        ("width", f.width.to_string()),
        ("height", f.height.to_string()),
    ])
}
fn atomic(path: &Path, value: &str) -> Result<(), String> {
    let temp = path.with_extension("pending");
    std::fs::write(&temp, value).map_err(|e| e.to_string())?;
    std::fs::rename(temp, path).map_err(|e| e.to_string())
}
fn empty(p: &PointerProbe) -> Result<(), String> {
    let s = p.snapshot()?;
    if s.down != 0 || s.up != 0 || !s.messages.is_empty() {
        return Err("unexpected input during non-input animation observation".into());
    }
    Ok(())
}

struct Run<'a> {
    cx: &'a mut AsyncApp,
    rt: &'a mut Runtime,
    receiver: &'a UiWakeReceiver,
    stats: &'a Stats,
    out: &'a mut Outcome,
    options: &'a Options,
    root: WindowHandle<HostRoot>,
    a: &'a PointerProbe,
    b: &'a PointerProbe,
    composition: UiComposition,
    deadline: Instant,
    peer_foreground: bool,
    last: String,
}
impl Run<'_> {
    async fn step(&mut self) -> Result<(), String> {
        empty(self.a)?;
        empty(self.b)?;
        let owned = if self.peer_foreground {
            self.b.is_foreground()?
        } else {
            self.a.is_foreground()?
        };
        if !owned {
            return Err("animation owned foreground lost; no activation retry".into());
        }
        if turn(self.cx, self.rt, self.receiver, false).await? {
            self.out.wake_consumed += 1;
            let report = app_update(self.cx, |app| self.composition.dispatch(app))?;
            if report.removed != 0 || !report.failures.is_empty() {
                return Err(format!("animation dispatch: {report:?}"));
            }
        }
        self.stats.check()
    }
    fn frame(
        &mut self,
        entity: &Entity<CefBrowser>,
        handle: &BrowserHandle,
    ) -> Result<Option<(Arc<OwnedFrame>, ViewDiagnostics, (bool, bool))>, String> {
        let (d, active, visible, scale) = update(self.cx, self.root, |_, w, app| {
            (
                entity.read(app).diagnostics(),
                w.is_window_active(),
                w.is_visible(),
                w.scale_factor(),
            )
        })?;
        let browser = handle.snapshot();
        let id = browser.browser;
        let source = self
            .rt
            .frame_source(id)
            .map_err(|e| format!("animation source: {e:?}"))?
            .snapshot();
        self.last = object(vec![
            ("active", bool_json(active)),
            ("visible", bool_json(visible)),
            ("source_closed", bool_json(source.closed)),
            ("source_visible", bool_json(source.view.visible)),
            ("title", quote(&browser.navigation.title)),
            ("diagnostics", metrics_json(&d)),
        ]);
        let Some(f) = source.view.frame.as_ref() else {
            return Ok(None);
        };
        let wanted = (
            (320_f32 * scale).ceil() as u32,
            (240_f32 * scale).ceil() as u32,
        );
        if browser.state != SessionState::Alive
            || browser.navigation.is_loading
            || source.closed
            || !source.view.visible
            || source.viewport
                != Some(Viewport {
                    width: 320,
                    height: 240,
                    scale,
                })
            || f.info.browser != id
            || f.info.surface != SurfaceKind::View
            || source.view.generation != f.info.surface_generation
            || (f.info.width, f.info.height) != wanted
            || (d.frame_width, d.frame_height) != wanted
            || d.painted_sequence != f.info.sequence
            || d.painted_generation != f.info.surface_generation
            || d.surface_bounds != (0., 48., 320., 240.)
            || d.scale_factor != scale
            || d.last_error.is_some()
            || d.pending_resync
            || d.paint_calls == 0
        {
            return Ok(None);
        }
        Ok(Some((f.clone(), d, (active, visible))))
    }
    async fn capture(
        &mut self,
        stage: u32,
        handle: &BrowserHandle,
        d: &ViewDiagnostics,
        frame: Option<&OwnedFrame>,
        samples: &[Sample],
    ) -> Result<(), String> {
        empty(self.a)?;
        empty(self.b)?;
        let a = self.a.info()?;
        let b = self.b.info()?;
        let thread = self.a.owner_thread()?;
        if self.b.owner_thread() != Ok(thread) || a.hwnd == b.hwnd {
            return Err("animation window pair/thread mismatch".into());
        }
        if (stage == 0 && !self.a.is_foreground()?) || (stage > 0 && !self.b.is_foreground()?) {
            return Err("animation stage foreground mismatch".into());
        }
        let (active, visible, scale) = update(self.cx, self.root, |_, w, _| {
            (w.is_window_active(), w.is_visible(), w.scale_factor())
        })?;
        if !visible || active != (stage == 0) {
            return Err("animation active/visible state mismatch".into());
        }
        let case = self
            .options
            .report
            .parent()
            .ok_or("animation case missing")?;
        let id = handle.snapshot().browser;
        let raw = if let Some(f) = frame {
            // Keep the existing fixed-320 capture helper's exact local raw contract.
            let name = format!("input-stage-{stage}-view.bgra");
            std::fs::write(case.join(&name), f.pixels.as_slice()).map_err(|e| e.to_string())?;
            object(vec![
                ("file", quote(&name)),
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
        } else {
            "null".into()
        };
        let text = object(vec![
            ("schema", "1".into()),
            ("kind", quote("unfocused-animation-slice")),
            ("pid", std::process::id().to_string()),
            ("stage", stage.to_string()),
            (
                "browser",
                format!("[{},{}]", id.runtime().value(), id.local()),
            ),
            ("window", fact(&a, thread)),
            ("peer", fact(&b, thread)),
            ("hwnds", format!("[{},{}]", a.hwnd, b.hwnd)),
            ("peer_title", quote(PEER)),
            ("scale_factor", scale.to_string()),
            ("view_bounds", "[0,48,320,240]".into()),
            ("active", bool_json(active)),
            ("visible", bool_json(visible)),
            ("diagnostics", metrics_json(d)),
            ("view_raw", raw),
            (
                "samples",
                format!(
                    "[{}]",
                    samples
                        .iter()
                        .map(sample_json)
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            ),
            ("detached", bool_json(stage == 2)),
            ("keyboard_or_pointer_sent", bool_json(false)),
            ("task_11_3_accepted", bool_json(false)),
        ]);
        atomic(&case.join(format!("unfocused-stage-{stage}.json")), &text)?;
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
                .with_extension(format!("unfocused-{stage}.ready")),
            &std::process::id().to_string(),
        )?;
        let release = self
            .options
            .gate
            .with_extension(format!("unfocused-{stage}.release"));
        let until = (Instant::now() + WAIT).min(self.deadline);
        while !release.is_file() {
            if Instant::now() >= until {
                return Err(format!("animation stage {stage} observer timeout"));
            }
            self.step().await?;
        }
        if std::fs::read_to_string(release)
            .map_err(|e| e.to_string())?
            .trim()
            != std::process::id().to_string()
        {
            return Err("animation stage release PID mismatch".into());
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
    peer: WindowHandle<HostRoot>,
    options: &Options,
    out: &mut Outcome,
    a: &PointerProbe,
    b: &PointerProbe,
) -> Result<(), String> {
    if !a.is_foreground()? {
        return Err(
            "initial owned A is not already foreground; no activation request permitted".into(),
        );
    }
    let mut r = Run {
        cx,
        rt,
        receiver,
        stats,
        root,
        options,
        out,
        a,
        b,
        composition: UiComposition::new(NonZeroUsize::new(1).ok_or("zero route")?),
        deadline: Instant::now() + Duration::from_secs(75),
        peer_foreground: false,
        last: "none".into(),
    };
    while r.rt.state() != RuntimeState::Ready {
        if Instant::now() >= r.deadline {
            return Err("animation runtime readiness timeout".into());
        }
        r.step().await?;
    }
    let scale = update(r.cx, root, |_, w, _| w.scale_factor())?;
    let handle =
        r.rt.create_browser(BrowserConfig {
            initial_url: URL.into(),
            profile: None,
            render_preference: options.backend,
            viewport: Viewport {
                width: 320,
                height: 240,
                scale,
            },
            options: Default::default(),
        })
        .map_err(|e| format!("animation create: {e:?}"))?;
    let id = handle.snapshot().browser;
    r.out.browser = Some(id);
    let until = (Instant::now() + WAIT).min(r.deadline);
    while handle.snapshot().state != SessionState::Alive {
        if handle.snapshot().state != SessionState::Creating || Instant::now() >= until {
            return Err("animation Browser creation incomplete".into());
        }
        r.step().await?;
    }
    let attachment =
        r.rt.attach_browser(&handle)
            .map_err(|e| format!("animation attach: {e:?}"))?;
    r.out.duplicate_refused = r.rt.attach_browser(&handle).is_err();
    if !r.out.duplicate_refused {
        return Err("animation duplicate attachment accepted".into());
    }
    let entity = update(r.cx, root, move |host, w, app| {
        let e = CefBrowser::new(attachment, w, app)?;
        host.browser = Some(e.clone());
        app.notify();
        Ok::<_, rust_cef_gpui::ViewError>(e)
    })?
    .map_err(|e| format!("animation View: {e:?}"))?;
    update(r.cx, root, |_, w, app| {
        r.composition.register(&entity, w, &*app)
    })?
    .map_err(|e| format!("animation register: {e:?}"))?;
    let report = app_update(r.cx, |app| r.composition.dispatch(app))?;
    if report.attempted != 1 || report.removed != 0 || !report.failures.is_empty() {
        return Err(format!("animation initial dispatch: {report:?}"));
    }
    let until = (Instant::now() + WAIT).min(r.deadline);
    loop {
        if Instant::now() >= until {
            return Err(format!("animation initial frame timeout: {}", r.last));
        }
        rendered(r.cx, root).await?;
        if let Some((f, d, (active, visible))) = r.frame(&entity, &handle)? {
            if active && visible {
                r.capture(0, &handle, &d, Some(&f), &[]).await?;
                break;
            }
        }
        r.step().await?;
    }
    // Only the currently foreground owned A requests its own B once. No ALT,
    // thread attachment, focus retry, policy changes or foreign activation.
    if !a.is_foreground()? {
        return Err("original owner not foreground before peer request".into());
    }
    b.request_foreground()?;
    r.peer_foreground = true;
    let url = page();
    handle
        .submit(BrowserCommand::Navigate(url.clone()))
        .map_err(|e| format!("animation fixture navigate: {e:?}"))?;
    let started = Instant::now();
    let until = (started + WAIT).min(r.deadline);
    let mut samples = Vec::with_capacity(3);
    loop {
        if Instant::now() >= until {
            return Err(format!(
                "unfocused animation timeout: {} qualifying samples; last {}",
                samples.len(),
                r.last
            ));
        }
        rendered(r.cx, root).await?;
        if let Some((f, d, (active, visible))) = r.frame(&entity, &handle)? {
            if active || !visible || !b.is_foreground()? {
                r.step().await?;
                continue;
            }
            let nav = handle.snapshot();
            if nav.navigation.address != url {
                r.step().await?;
                continue;
            }
            let (n, done) = match tick(&nav.navigation.title) {
                Ok(v) => v,
                Err(_) => {
                    r.step().await?;
                    continue;
                }
            };
            let at = ((24_f32 * scale).floor() as usize * f.info.width as usize
                + (24_f32 * scale).floor() as usize)
                * 4;
            let patch = f.pixels.get(at..at + 4);
            if samples.len() < 3
                && matches!(patch, Some([20, 90, 190, 255]) | Some([170, 150, 20, 255]))
            {
                let s = Sample {
                    elapsed_ms: u64::try_from(started.elapsed().as_millis())
                        .map_err(|_| "elapsed overflow")?,
                    sequence: f.info.sequence,
                    generation: f.info.surface_generation,
                    digest: digest(f.pixels.as_slice()),
                    tick: n,
                    width: f.info.width,
                    height: f.info.height,
                    painted_sequence: d.painted_sequence,
                    painted_generation: d.painted_generation,
                    paint_calls: d.paint_calls,
                    active,
                    visible,
                    source_visible: true,
                };
                if progresses(&s, samples.last()) {
                    let case = options
                        .report
                        .parent()
                        .ok_or("animation sample case missing")?;
                    std::fs::write(
                        case.join(format!("unfocused-sample-{}.bgra", s.sequence)),
                        f.pixels.as_slice(),
                    )
                    .map_err(|e| e.to_string())?;
                    samples.push(s);
                }
            }
            if done && samples.len() == 3 && patch == Some(&[170, 150, 20, 255]) {
                r.out.paint = Some(d.clone());
                r.capture(1, &handle, &d, Some(&f), &samples).await?;
                break;
            }
        }
        r.step().await?;
    }
    let weak = entity.downgrade();
    let (released, before) = update(r.cx, root, |host, w, app| {
        let e = host.browser.take().ok_or("animation host lost View")?;
        let d = e.update(app, |v, app| {
            v.release_image(w, app)
                .map_err(|e| format!("animation release: {e:?}"))?;
            Ok::<_, String>(v.diagnostics())
        })?;
        let before = host.renders;
        host.detached = true;
        app.notify();
        Ok::<_, String>((d, before))
    })??;
    r.composition.unregister(id);
    drop(entity);
    if app_update(r.cx, |app| {
        weak.upgrade().is_some()
            || !app.windows().contains(&AnyWindowHandle::from(root))
            || !app.windows().contains(&AnyWindowHandle::from(peer))
    })? {
        return Err("animation release/window ownership mismatch".into());
    }
    if released.created_images == 0
        || released.created_images != released.released_images
        || released.last_error.is_some()
    {
        return Err("animation image release unbalanced".into());
    }
    rendered(r.cx, root).await?;
    r.out.detached_alive = update(r.cx, root, |host, _, _| {
        host.detached && host.renders > before
    })?;
    r.out.released_view = Some(released.clone());
    r.capture(2, &handle, &released, None, &samples).await?;
    while !matches!(
        handle.snapshot().state,
        SessionState::Closing | SessionState::Closed
    ) {
        if Instant::now() >= r.deadline {
            return Err("animation released View did not request close".into());
        }
        r.step().await?;
    }
    r.out.view_release_observed_closing = true;
    while !options.gate.is_file() {
        if Instant::now() >= r.deadline {
            return Err("animation final observer timeout".into());
        }
        r.step().await?;
    }
    if std::fs::read_to_string(&options.gate)
        .map_err(|e| e.to_string())?
        .trim()
        != "release"
    {
        return Err("animation final gate mismatch".into());
    }
    r.out.unfocused_detail = Some(object(vec![
        ("kind", quote("unfocused-animation-slice")),
        ("success", bool_json(true)),
        ("stages", "3".into()),
        (
            "samples",
            format!(
                "[{}]",
                samples
                    .iter()
                    .map(sample_json)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        ),
        ("unfocused_animation_verified", bool_json(true)),
        ("minimize_verified", bool_json(false)),
        ("task_11_3_accepted", bool_json(false)),
        ("keyboard_or_pointer_sent", bool_json(false)),
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
    let peer = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    point(px(760.), px(40.)),
                    size(px(520.), px(400.)),
                ))),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some(PEER.into()),
                    ..Default::default()
                }),
                focus: false,
                ..Default::default()
            },
            |_, app| {
                app.new(|_| HostRoot {
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
        .map_err(|e| format!("animation peer: {e}"))?;
    out.additional_roots.push(peer);
    let mut a = PointerProbe::install(TITLE)?;
    let mut b = PointerProbe::install(PEER)?;
    let result = inner(cx, rt, receiver, stats, root, peer, options, out, &a, &b).await;
    let retire_b = b.retire();
    let retire_a = a.retire();
    if let Err(e) = retire_b.and(retire_a) {
        return Err(format!("animation probe retirement: {e}; prior {result:?}"));
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn progress_requires_real_inactive_visible_changes_and_elapsed_observation() {
        assert_eq!(tick("Unfocused|tick=96|done=1"), Ok((96, true)));
        assert!(tick("Unfocused|tick=97|done=1").is_err());
        assert!(tick("Unfocused|tick=4|done=1").is_err());
        let a = Sample {
            elapsed_ms: 10,
            sequence: 1,
            generation: 2,
            digest: 10,
            tick: 1,
            width: 480,
            height: 360,
            painted_sequence: 1,
            painted_generation: 2,
            paint_calls: 1,
            active: false,
            visible: true,
            source_visible: true,
        };
        assert!(progresses(&a, None));
        let mut b = a.clone();
        b.elapsed_ms = 510;
        b.sequence = 2;
        b.painted_sequence = 2;
        b.paint_calls = 2;
        b.digest = 11;
        b.tick = 11;
        assert!(progresses(&b, Some(&a)));
        b.active = true;
        assert!(!progresses(&b, Some(&a)));
        b.active = false;
        b.visible = false;
        assert!(!progresses(&b, Some(&a)));
        b.visible = true;
        b.source_visible = false;
        assert!(!progresses(&b, Some(&a)));
        b.source_visible = true;
        b.digest = 10;
        assert!(!progresses(&b, Some(&a)));
        b.digest = 11;
        b.elapsed_ms = 509;
        assert!(!progresses(&b, Some(&a)));
    }
}
