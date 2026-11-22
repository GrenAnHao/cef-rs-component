//! Public-only 10.3 POPUP/peer-window scenario.
//!
//! The host owns both real GPUI windows: the window passed in by `main` (A) and
//! a second window opened here (B), each with its own `HostRoot` and one
//! `CefBrowser`. Both Browsers come from the same `Runtime`; the only frame
//! route is the host's single wake receiver feeding one explicit
//! `UiComposition`. Nothing here pumps the Runtime, polls a timer for frames,
//! submits a `BrowserCommand`, touches CEF directly, or creates a second
//! Application.
//!
//! POPUP geometry is never guessed. Every option click is derived from
//! `Runtime::frame_source(browser).snapshot().popup` and must land in the
//! visible VIEW clip; a clipped-away middle row is a hard failure, not a reason
//! to pick another row or widen a threshold.
//!
//! A click reaches CEF only through real OS input: the external driver sends a
//! complete left gesture at the coordinates published in `next_click`, GPUI
//! delivers it to the component's weak view/input path, and that path submits
//! the atomic `MouseClick` command. This module only observes the pointer
//! probe's WM pair and the component's `clicks_submitted` growth.
//!
//! Stage 3 removes A's real window while its select popup is shown, keeps the
//! A entity strongly owned locally, and only after the real Closed callback /
//! `Source.closed` lets one dispatch prune the retired route. A's counters must
//! not move and no old window may be accessed. Stage 7 explicitly releases B's
//! VIEW and POPUP images before dropping its entity; stage 8 is the release-only
//! green host.

use std::num::NonZeroUsize;

use gpui::{AnyWindowHandle, point};
use rust_cef_gpui::{
    CefBrowser, CompositionError, UiComposition, ViewDiagnostics, ViewError,
    contracts::{BrowserConfig, BrowserId, SessionState, Viewport},
    ports::{BrowserHandle, OwnedFrame},
};

use super::pointer_probe::{PointerProbe, PointerSnapshot};
use super::*;

const PEER_TITLE: &str = "GPUI-CEF popup peer";
const HEADER_DIP: f32 = 48.0;
const A_EXTENT: (f32, f32) = (320.0, 240.0);
const B_EXTENT: (f32, f32) = (360.0, 260.0);
const A_REGION: (f32, f32, f32, f32) = (0.0, HEADER_DIP, A_EXTENT.0, A_EXTENT.1);
const B_REGION: (f32, f32, f32, f32) = (0.0, HEADER_DIP, B_EXTENT.0, B_EXTENT.1);
/// Fixed view-space select click points (inside the view, not the popup).
const A_OPEN: (i32, i32) = (270, 40);
const B_OPEN: (i32, i32) = (80, 215);
const A_BLUE: [u8; 3] = [26, 76, 126];
const B_PURPLE: [u8; 3] = [110, 40, 180];
const STAGES: u32 = 9;
/// Per-stage external-capture bound; the launcher allows 20 s per stage.
const STAGE_WAIT: Duration = Duration::from_secs(20);
/// Whole-scenario bound for the nine stages, leaving shutdown headroom inside
/// the launcher's process timeout.
const SCENARIO_BUDGET: Duration = Duration::from_secs(100);
const OBSERVER_WAIT: Duration = Duration::from_secs(20);
/// dwExtraInfo marker the external SendInput driver must stamp.
const INPUT_MARKER: u64 = 0x4346_4530;

// ---------------------------------------------------------------------------
// Fixed trusted fixtures
// ---------------------------------------------------------------------------

fn percent_encode(html: &str) -> String {
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

/// Fixed data page. No eval, no CDP, no network: the only behaviour is a
/// `select` whose three options rewrite `document.title` on change. A's `one`
/// label is deliberately longer than its view so only clipping, never
/// translation, can be observed.
fn page_uri(slot: char, color: [u8; 3], left: i32, top: i32, width: i32) -> String {
    let label = if slot == 'A' {
        "one - a deliberately long option label that leaves the view"
    } else {
        "one"
    };
    let html = format!(
        "<html><head><title>Popup {slot}|one</title><style>\
html,body{{margin:0;background:rgb({r},{g},{b});font:16px/16px monospace;color:#ffffff}}\
select{{position:absolute;left:{left}px;top:{top}px;width:{width}px;height:30px;\
font:16px/16px monospace}}\
option{{font:16px/16px monospace}}</style></head><body>\
<select onchange=\"document.title='Popup {slot}|'+this.value\">\
<option value=\"one\" selected>{label}</option>\
<option value=\"two\">two</option>\
<option value=\"three\">three</option>\
</select></body></html>",
        r = color[0],
        g = color[1],
        b = color[2],
    );
    percent_encode(&html)
}

fn browser_config(
    url: String,
    backend: RenderPreference,
    extent: (f32, f32),
    scale: f32,
) -> BrowserConfig {
    BrowserConfig {
        initial_url: url,
        profile: None,
        render_preference: backend,
        viewport: Viewport {
            width: extent.0 as i32,
            height: extent.1 as i32,
            scale,
        },
        options: Default::default(),
    }
}

// ---------------------------------------------------------------------------
// Small host helpers
// ---------------------------------------------------------------------------

fn atomic_write(path: &Path, text: &str) -> Result<(), String> {
    let extension = path.extension().and_then(|v| v.to_str()).unwrap_or("tmp");
    let temporary = path.with_extension(format!("{extension}.tmp"));
    std::fs::write(&temporary, text).map_err(|e| e.to_string())?;
    std::fs::rename(&temporary, path).map_err(|e| e.to_string())
}

fn browser_json(browser: BrowserId) -> String {
    format!("[{},{}]", browser.runtime().value(), browser.local())
}

fn click_json(click: Option<(i32, i32)>) -> String {
    click.map_or_else(
        || "null".into(),
        |(x, y)| object(vec![("x", x.to_string()), ("y", y.to_string())]),
    )
}

/// The launcher writes the original observer gate only after all nine stage
/// captures. Seeing it earlier means the driver gave up: fail immediately
/// instead of burning the current stage timeout.
fn early_gate(options: &Options) -> Result<(), String> {
    if options.gate.is_file() {
        return Err(
            "external observer gate appeared before this popup stage (early validation failure)"
                .into(),
        );
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct WindowFacts {
    hwnd: u64,
    pid: u32,
    dpi: u32,
    left: i32,
    top: i32,
    width: u32,
    height: u32,
}

fn facts_of(probe: &PointerProbe) -> Result<WindowFacts, String> {
    let info = probe.info()?;
    Ok(WindowFacts {
        hwnd: info.hwnd,
        pid: info.pid,
        dpi: info.dpi,
        left: info.left,
        top: info.top,
        width: info.width,
        height: info.height,
    })
}

/// Client area in screen physical pixels, as the external capture expects.
fn window_json(facts: WindowFacts) -> String {
    object(vec![
        ("hwnd", facts.hwnd.to_string()),
        ("pid", facts.pid.to_string()),
        ("dpi", facts.dpi.to_string()),
        ("left", facts.left.to_string()),
        ("top", facts.top.to_string()),
        ("width", facts.width.to_string()),
        ("height", facts.height.to_string()),
    ])
}

type WindowEntry = (&'static str, &'static str, AnyWindowHandle, u64);

/// Only windows still really registered in the App are listed; each entry is a
/// plain handle plus the observed HWND, never an entity.
fn windows_json(cx: &mut AsyncApp, known: &[WindowEntry]) -> Result<String, String> {
    let live = app_update(&mut *cx, |app| app.windows())?;
    Ok(format!(
        "[{}]",
        known
            .iter()
            .filter(|entry| live.contains(&entry.2))
            .map(|entry| {
                let (slot, title, _, hwnd) = *entry;
                object(vec![
                    ("slot", quote(slot)),
                    ("title", quote(title)),
                    ("hwnd", hwnd.to_string()),
                ])
            })
            .collect::<Vec<_>>()
            .join(",")
    ))
}

fn pointer_json(snapshot: &PointerSnapshot) -> String {
    object(vec![
        ("down", snapshot.down.to_string()),
        ("up", snapshot.up.to_string()),
        (
            "messages",
            format!(
                "[{}]",
                snapshot
                    .messages
                    .iter()
                    .map(|m| object(vec![
                        ("x", m.x.to_string()),
                        ("y", m.y.to_string()),
                        ("up", bool_json(m.up)),
                        ("extra_info", m.extra_info.to_string()),
                    ]))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        ),
    ])
}

fn popup_json(diag: &ViewDiagnostics) -> String {
    object(vec![
        ("visible", bool_json(diag.popup.visible)),
        ("generation", diag.popup.generation.to_string()),
        (
            "bounds",
            format!(
                "[{},{},{},{}]",
                diag.popup.bounds.0, diag.popup.bounds.1, diag.popup.bounds.2, diag.popup.bounds.3
            ),
        ),
    ])
}

/// Raw evidence descriptor. `file` is a case-local name; the external strict
/// compositor joins it with the case directory and hashes it itself.
fn raw_json(frame: &OwnedFrame, file_name: &str, surface: &str) -> String {
    object(vec![
        ("file", quote(file_name)),
        ("browser", browser_json(frame.info.browser)),
        ("surface", quote(surface)),
        ("generation", frame.info.surface_generation.to_string()),
        ("sequence", frame.info.sequence.to_string()),
        ("width", frame.info.width.to_string()),
        ("height", frame.info.height.to_string()),
        ("bytes", frame.pixels.as_slice().len().to_string()),
    ])
}

fn released_ok(diag: &ViewDiagnostics) -> bool {
    diag.last_error.is_none()
        && diag.created_images > 0
        && diag.created_images == diag.released_images
        && diag.popup.created_images > 0
        && diag.popup.created_images == diag.popup.released_images
}

// ---------------------------------------------------------------------------
// Authoritative source snapshot
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct RawState {
    view_visible: bool,
    view_generation: u64,
    view_sequence: Option<u64>,
    view_pixels: Option<(u32, u32)>,
    popup_visible: bool,
    popup_generation: u64,
    popup_bounds: (i32, i32, i32, i32),
    popup_sequence: Option<u64>,
    popup_pixels: Option<(u32, u32)>,
    closed: bool,
}

struct Source {
    raw: RawState,
    view_frame: Option<Arc<OwnedFrame>>,
    popup_frame: Option<Arc<OwnedFrame>>,
}

fn read_source(rt: &Runtime, browser: BrowserId) -> Result<Source, String> {
    let source = rt
        .frame_source(browser)
        .map_err(|e| format!("frame source {browser:?}: {e:?}"))?;
    let snapshot = source.snapshot();
    let view = snapshot.view;
    let popup = snapshot.popup;
    let view_frame = view.frame.clone();
    let popup_frame = popup.frame.clone();
    let raw = RawState {
        view_visible: view.visible,
        view_generation: view.generation,
        view_sequence: view_frame.as_ref().map(|f| f.info.sequence),
        view_pixels: view_frame.as_ref().map(|f| (f.info.width, f.info.height)),
        popup_visible: popup.visible,
        popup_generation: popup.generation,
        popup_bounds: (
            popup.bounds.x,
            popup.bounds.y,
            popup.bounds.width,
            popup.bounds.height,
        ),
        popup_sequence: popup_frame.as_ref().map(|f| f.info.sequence),
        popup_pixels: popup_frame.as_ref().map(|f| (f.info.width, f.info.height)),
        closed: snapshot.closed,
    };
    Ok(Source {
        raw,
        view_frame,
        popup_frame,
    })
}

#[derive(Clone, Copy)]
enum Need {
    Hidden,
    Visible,
    /// Visible with a generation strictly above the popup opened at this stage.
    ReopenAfter(u32),
}

impl Need {
    fn shows_popup(self) -> bool {
        !matches!(self, Self::Hidden)
    }
}

fn check_view(
    diag: &ViewDiagnostics,
    raw: &RawState,
    region: (f32, f32, f32, f32),
) -> Result<(), String> {
    let fail = |what: String| Err(format!("VIEW mismatch: {what}"));
    if diag.last_error.is_some() {
        return Err(format!("VIEW diagnostic error: {:?}", diag.last_error));
    }
    if diag.pending_click {
        return fail("a click is still pending".into());
    }
    if !raw.view_visible {
        return fail("source VIEW is hidden".into());
    }
    if diag.binding_retired {
        return fail("binding is already retired".into());
    }
    if diag.consumed_updates == 0 || diag.paint_calls == 0 {
        return fail("no consumed update or successful paint_calls yet".into());
    }
    if diag.painted_generation != raw.view_generation {
        return fail(format!(
            "painted generation {} != source {}",
            diag.painted_generation, raw.view_generation
        ));
    }
    if raw.view_sequence != Some(diag.painted_sequence) {
        return fail(format!(
            "painted sequence {} != source {:?}",
            diag.painted_sequence, raw.view_sequence
        ));
    }
    if raw.view_pixels != Some((diag.frame_width, diag.frame_height)) {
        return fail(format!(
            "presented {}x{} != source {:?}",
            diag.frame_width, diag.frame_height, raw.view_pixels
        ));
    }
    if diag.surface_bounds != region {
        return fail(format!(
            "surface bounds {:?} != fixed region {region:?}",
            diag.surface_bounds
        ));
    }
    Ok(())
}

fn check_popup(
    diag: &ViewDiagnostics,
    raw: &RawState,
    need: Need,
    opened: &[u64],
) -> Result<(), String> {
    let min_generation = match need {
        Need::Hidden => {
            if diag.popup.visible || raw.popup_visible {
                return Err("POPUP mismatch: diagnostic/source popup is still visible".into());
            }
            if diag.popup.generation != raw.popup_generation {
                return Err(format!(
                    "POPUP mismatch: hidden generation {} != source {}",
                    diag.popup.generation, raw.popup_generation
                ));
            }
            if raw.popup_sequence.is_some() {
                return Err("POPUP mismatch: source popup slot was not cleared".into());
            }
            if diag.popup.created_images > 0
                && diag.popup.created_images != diag.popup.released_images
            {
                return Err(format!(
                    "POPUP mismatch: hidden slot has {} created / {} released",
                    diag.popup.created_images, diag.popup.released_images
                ));
            }
            return Ok(());
        }
        Need::Visible => 1,
        Need::ReopenAfter(stage) => opened.get(stage as usize).copied().unwrap_or(0) + 1,
    };
    if !diag.popup.visible || !raw.popup_visible {
        return Err("POPUP mismatch: popup is not visible".into());
    }
    if raw.popup_generation < min_generation {
        return Err(format!(
            "POPUP mismatch: generation {} < required {min_generation}",
            raw.popup_generation
        ));
    }
    for (label, actual, expected) in [
        ("diagnostic", diag.popup.generation, raw.popup_generation),
        (
            "painted",
            diag.popup.painted_generation,
            raw.popup_generation,
        ),
        (
            "presented",
            u64::from(diag.popup.frame_width) << 32 | u64::from(diag.popup.frame_height),
            raw.popup_pixels
                .map_or(0, |(w, h)| u64::from(w) << 32 | u64::from(h)),
        ),
    ] {
        if actual != expected {
            return Err(format!(
                "POPUP mismatch: {label} {actual} != source {expected}"
            ));
        }
    }
    if diag.popup.paint_calls == 0 {
        return Err("POPUP mismatch: no successful popup paint".into());
    }
    if raw.popup_sequence != Some(diag.popup.painted_sequence) {
        return Err(format!(
            "POPUP mismatch: painted sequence {} != source {:?}",
            diag.popup.painted_sequence, raw.popup_sequence
        ));
    }
    if diag.popup.bounds != raw.popup_bounds {
        return Err(format!(
            "POPUP mismatch: diagnostic bounds {:?} != source {:?}",
            diag.popup.bounds, raw.popup_bounds
        ));
    }
    Ok(())
}

/// Middle row of the raw popup rect, clipped to the VIEW. The x is the centre
/// of the visible intersection. A clipped-away middle row fails outright.
fn popup_click_point(raw: (i32, i32, i32, i32), view: (i32, i32)) -> Result<(i32, i32), String> {
    let (x, y, width, height) = raw;
    if width <= 0 || height <= 0 {
        return Err(format!("popup rect has non-positive extent: {raw:?}"));
    }
    let (x0, y0) = (x.max(0), y.max(0));
    let (x1, y1) = ((x + width).min(view.0), (y + height).min(view.1));
    if x1 <= x0 || y1 <= y0 {
        return Err(format!(
            "popup rect {raw:?} has no visible intersection with view {view:?}"
        ));
    }
    let middle = y + height / 2;
    if middle < y0 || middle >= y1 {
        return Err(format!(
            "popup middle row y={middle} is outside the visible view clip [{y0},{y1})"
        ));
    }
    let centre = (x0 + x1) / 2;
    if centre < 0 || centre >= view.0 {
        return Err(format!(
            "popup clip centre x={centre} is outside view {view:?}"
        ));
    }
    Ok((centre, middle))
}

/// The exact decimal the metadata carries. It must stay a JSON number for the
/// external driver, and arming uses the same parsed value so its `SendInput`
/// client point matches the armed one exactly under its own rounding.
fn scale_text(scale: f32) -> String {
    format!("{scale:?}")
}

/// View DIP point -> physical client pixels, computed like the driver:
/// `floor((origin + dip) * parsed_scale + 0.5)` in f64.
fn physical_client(region: (f32, f32, f32, f32), click: (i32, i32), scale: f32) -> (i32, i32) {
    let parsed: f64 = scale_text(scale).parse().unwrap_or(f64::from(scale));
    let x = f64::from(region.0) + f64::from(click.0);
    let y = f64::from(region.1) + f64::from(click.1);
    (
        (x * parsed + 0.5).floor() as i32,
        (y * parsed + 0.5).floor() as i32,
    )
}

// ---------------------------------------------------------------------------
// Execution context: one wake source, one composition, one deadline
// ---------------------------------------------------------------------------

struct Exec<'a> {
    cx: &'a mut AsyncApp,
    rt: &'a mut Runtime,
    receiver: &'a UiWakeReceiver,
    stats: &'a Stats,
    out: &'a mut Outcome,
    options: &'a Options,
    composition: UiComposition,
    deadline: Instant,
}

impl<'a> Exec<'a> {
    fn limit(&self) -> Instant {
        (Instant::now() + STAGE_WAIT).min(self.deadline)
    }

    fn dispatch(&mut self) -> Result<(), String> {
        let composition = &mut self.composition;
        let (report, routes) = app_update(&mut *self.cx, |app| {
            let report = composition.dispatch(app);
            (report, composition.len())
        })?;
        if !report.failures.is_empty() {
            return Err(format!("dispatch failures: {:?}", report.failures));
        }
        if report.removed > 0 {
            return Err(format!(
                "unexpected route removal before A teardown: {} (routes {routes})",
                report.removed
            ));
        }
        Ok(())
    }

    /// One real wake turn; a wake is dispatched exactly once through the App.
    async fn step(&mut self, require_wake: bool) -> Result<bool, String> {
        let woke = turn(&mut *self.cx, &mut *self.rt, self.receiver, require_wake).await?;
        self.stats.check()?;
        if woke {
            self.out.wake_consumed += 1;
            self.dispatch()?;
        }
        Ok(woke)
    }

    /// Waits for the external stage release. Only PID-exact releases count.
    async fn wait_release(&mut self, stage: u32, deadline: Instant) -> Result<(), String> {
        let release = self
            .options
            .gate
            .with_extension(format!("popup-{stage}.release"));
        while !release.is_file() {
            early_gate(self.options)?;
            if Instant::now() >= deadline {
                return Err(format!("popup stage {stage} release timeout"));
            }
            let _ = self.step(false).await?;
        }
        let marker = std::fs::read_to_string(&release).map_err(|e| e.to_string())?;
        if marker.trim() != std::process::id().to_string() {
            return Err(format!("popup stage {stage} release PID mismatch"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Stage capture
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct SlotCtx {
    root: WindowHandle<HostRoot>,
    browser: BrowserId,
    region: (f32, f32, f32, f32),
    scale: f32,
    os_title: &'static str,
}

#[derive(Clone, Copy)]
enum Click {
    None,
    Fixed(i32, i32),
    PopupMiddle,
}

/// Post-click sanity for the same live surface. The click legitimately triggers
/// new CEF paints, so an in-flight newer sequence is expected here; only a
/// retired binding, a foreign generation (paint from the future) or a different
/// surface geometry is a failure. The next stage re-proves exact identity after
/// its own real frame.
fn checks_click_fresh(
    raw: &RawState,
    diag: &ViewDiagnostics,
    region: (f32, f32, f32, f32),
) -> Result<(), String> {
    if diag.paint_calls == 0 {
        return Err("post-click VIEW has no successful paint".into());
    }
    if diag.painted_generation > raw.view_generation {
        return Err(format!(
            "post-click painted generation {} is ahead of source {}",
            diag.painted_generation, raw.view_generation
        ));
    }
    if diag.surface_bounds != region {
        return Err(format!(
            "post-click surface bounds {:?} != {region:?}",
            diag.surface_bounds
        ));
    }
    if diag.binding_retired {
        return Err("post-click binding is retired".into());
    }
    Ok(())
}

struct Shot {
    diag: ViewDiagnostics,
    raw: RawState,
    /// DOM title admitted for this stage, before any click of this stage ran.
    title: String,
    next_click: Option<(i32, i32)>,
}

fn read_view(
    cx: &mut AsyncApp,
    root: WindowHandle<HostRoot>,
    entity: &Entity<CefBrowser>,
) -> Result<ViewDiagnostics, String> {
    update(cx, root, |_, _, cx| entity.read(cx).diagnostics())
}

fn read_view_erased(
    cx: &mut AsyncApp,
    entity: &Entity<CefBrowser>,
) -> Result<ViewDiagnostics, String> {
    app_update(cx, |app| entity.read(app).diagnostics())
}

/// One stage's visible state. It is admitted only after a real GPUI frame plus
/// component diagnostics that exactly match the authoritative source; the
/// stage's own window is then activated, the pointer probe armed, raw CPU
/// pixels + metadata + ready written, and the external release awaited.
#[allow(clippy::too_many_arguments)]
async fn capture(
    exec: &mut Exec<'_>,
    stage: u32,
    slot: &'static str,
    ctx: &SlotCtx,
    entity: &Entity<CefBrowser>,
    handle: &BrowserHandle,
    probe: &mut PointerProbe,
    known: &[WindowEntry],
    need: Need,
    opened: &[u64],
    dom_title: &str,
    click: Click,
) -> Result<Shot, String> {
    let mut last = String::new();
    let deadline = exec.limit();
    let (diag, source, title) = loop {
        early_gate(exec.options)?;
        if Instant::now() >= deadline {
            return Err(format!(
                "popup stage {stage} ({slot}) frame target timeout: {last}"
            ));
        }
        rendered(&mut *exec.cx, ctx.root).await?;
        let diag = read_view(&mut *exec.cx, ctx.root, entity)?;
        let source = read_source(&mut *exec.rt, ctx.browser)?;
        let nav = handle.snapshot();
        if matches!(nav.state, SessionState::Closed | SessionState::CreateFailed)
            || source.raw.closed
        {
            return Err(format!(
                "popup stage {stage}: browser {slot} closed before its frame gate"
            ));
        }
        let view_ok = check_view(&diag, &source.raw, ctx.region);
        let popup_ok = check_popup(&diag, &source.raw, need, opened);
        let title_ok = nav.navigation.title == dom_title && !nav.navigation.is_loading;
        if view_ok.is_ok() && popup_ok.is_ok() && title_ok {
            if diag.clicks_rejected > 0 {
                return Err(format!(
                    "popup stage {stage}: component rejected {} clicks (last error {:?})",
                    diag.clicks_rejected, diag.last_error
                ));
            }
            break (diag, source, nav.navigation.title.clone());
        }
        last = view_ok.err().or_else(|| popup_ok.err()).unwrap_or_else(|| {
            format!(
                "page title {:?} != expected {dom_title:?} (loading {})",
                nav.navigation.title, nav.navigation.is_loading
            )
        });
        // A pending wake dispatches through the composition; otherwise the
        // loop only asks GPUI for another real presentation frame. Neither
        // path polls CEF, and Threaded never pumps.
        let _ = exec.step(false).await?;
    };

    // Overlapping windows are allowed; external capture and injected input
    // must both target this stage's own window.
    // GPUI-pre-windows 0.3.12 activate() injects unmarked ALT globally.
    // Never call it in this attributed-input fixture. A normal owned-HWND
    // request must succeed without synthetic keys or policy bypass.
    probe.request_foreground()?;
    rendered(&mut *exec.cx, ctx.root).await?;

    let view_int = (ctx.region.2 as i32, ctx.region.3 as i32);
    let next_click = match click {
        Click::None => None,
        Click::Fixed(x, y) => Some((x, y)),
        Click::PopupMiddle => Some(popup_click_point(source.raw.popup_bounds, view_int)?),
    };
    if let Some((x, y)) = next_click {
        let (px, py) = physical_client(ctx.region, (x, y), ctx.scale);
        probe
            .arm(px, py)
            .map_err(|e| format!("popup stage {stage}: pointer arm ({px},{py}): {e}"))?;
    }

    let facts = facts_of(probe)?;
    let case = exec
        .options
        .report
        .parent()
        .ok_or_else(|| "report path has no case directory".to_owned())?
        .to_path_buf();
    let view_name = format!("popup-stage-{stage}-view.bgra");
    let view_frame = source
        .view_frame
        .as_ref()
        .ok_or_else(|| format!("popup stage {stage}: matched VIEW has no frame"))?;
    std::fs::write(case.join(&view_name), view_frame.pixels.as_slice())
        .map_err(|e| e.to_string())?;
    let view_raw = raw_json(view_frame, &view_name, "View");
    let popup_raw = if need.shows_popup() {
        let popup_name = format!("popup-stage-{stage}-popup.bgra");
        let frame = source
            .popup_frame
            .as_ref()
            .ok_or_else(|| format!("popup stage {stage}: visible POPUP has no frame"))?;
        std::fs::write(case.join(&popup_name), frame.pixels.as_slice())
            .map_err(|e| e.to_string())?;
        raw_json(frame, &popup_name, "Popup")
    } else {
        "null".into()
    };

    let meta = object(vec![
        ("schema", "1".into()),
        ("stage", stage.to_string()),
        ("pid", std::process::id().to_string()),
        ("target", quote(slot)),
        // Top-level title is the real OS window title; the DOM title is
        // separate evidence of the actual selection.
        ("title", quote(ctx.os_title)),
        ("dom_title", quote(dom_title)),
        ("window", window_json(facts)),
        ("scale_factor", scale_text(ctx.scale)),
        (
            "view_bounds",
            format!("[0,{},{},{}]", HEADER_DIP as i32, view_int.0, view_int.1),
        ),
        ("browser", browser_json(ctx.browser)),
        ("windows", windows_json(&mut *exec.cx, known)?),
        ("diagnostics", metrics_json(&diag)),
        ("popup", popup_json(&diag)),
        ("view_raw", view_raw),
        ("popup_raw", popup_raw),
        ("next_click", click_json(next_click)),
    ]);
    write_json(&case.join(format!("popup-stage-{stage}.json")), meta)?;
    let pid = std::process::id().to_string();
    if stage == 0 {
        // The stage-0 frame is the moment the external helper-process observer
        // may start. Same atomic PID file form the launcher validates.
        atomic_write(&exec.options.gate.with_extension("ready"), &pid)?;
    }
    atomic_write(
        &exec
            .options
            .gate
            .with_extension(format!("popup-{stage}.ready")),
        &pid,
    )?;
    let wait_deadline = (Instant::now() + STAGE_WAIT).min(exec.deadline);
    exec.wait_release(stage, wait_deadline).await?;

    Ok(Shot {
        diag,
        raw: source.raw,
        title,
        next_click,
    })
}

/// A real OS left gesture must produce exactly the expected cumulative complete
/// probe pairs and new component submissions, with zero rejections, before the
/// probe may disarm.
#[allow(clippy::too_many_arguments)]
async fn finish_click(
    exec: &mut Exec<'_>,
    ctx: &SlotCtx,
    entity: &Entity<CefBrowser>,
    probe: &mut PointerProbe,
    base: u64,
    expected: u64,
    armed: (i32, i32),
    what: &str,
) -> Result<PointerSnapshot, String> {
    let deadline = exec.limit();
    let snapshot = loop {
        early_gate(exec.options)?;
        if Instant::now() >= deadline {
            return Err(format!("{what}: click/pointer pair timeout"));
        }
        let diag = read_view(&mut *exec.cx, ctx.root, entity)?;
        if diag.clicks_rejected > 0 {
            return Err(format!(
                "{what}: component rejected {} clicks",
                diag.clicks_rejected
            ));
        }
        let snapshot = probe
            .snapshot()
            .map_err(|e| format!("{what}: pointer snapshot: {e}"))?;
        if snapshot.down >= expected && snapshot.up >= expected && diag.clicks_submitted > base {
            break snapshot;
        }
        let _ = exec.step(false).await?;
    };
    let (source_fresh, diag) = {
        let source = read_source(&mut *exec.rt, ctx.browser)?;
        let diag = read_view(&mut *exec.cx, ctx.root, entity)?;
        (source.raw, diag)
    };
    checks_click_fresh(&source_fresh, &diag, ctx.region)?;
    if diag.clicks_rejected != 0 || diag.pending_click {
        return Err(format!(
            "{what}: click state is unhealthy ({} rejected, pending {}) with {:?}",
            diag.clicks_rejected, diag.pending_click, diag.last_error
        ));
    }
    if diag.clicks_submitted != expected {
        return Err(format!(
            "{what}: clicks_submitted {} -> {} is not exactly one new click (expected {expected})",
            base, diag.clicks_submitted
        ));
    }
    if snapshot.down != expected || snapshot.up != expected {
        return Err(format!(
            "{what}: expected exactly {expected} complete down/up pairs, probe has {}/{}",
            snapshot.down, snapshot.up
        ));
    }
    let tail = snapshot
        .messages
        .get(snapshot.messages.len().saturating_sub(2)..)
        .unwrap_or_default();
    if tail.len() != 2 || tail[0].up || !tail[1].up {
        return Err(format!(
            "{what}: probe tail is not one down-then-up pair: {tail:?}"
        ));
    }
    for message in tail {
        if message.extra_info != INPUT_MARKER || message.x != armed.0 || message.y != armed.1 {
            return Err(format!(
                "{what}: WM pair is not the external {INPUT_MARKER:#x} gesture at {armed:?}: {message:?}"
            ));
        }
    }
    probe
        .disarm()
        .map_err(|e| format!("{what}: pointer disarm: {e}"))?;
    Ok(snapshot)
}

// ---------------------------------------------------------------------------
// Stage sequencing
// ---------------------------------------------------------------------------

struct Plan {
    stage: u32,
    slot: &'static str,
    need: Need,
    dom_title: &'static str,
    click: Click,
    /// Cumulative complete clicks expected in this slot after its click.
    cumulative: u64,
}

const A_PLANS: [Plan; 4] = [
    Plan {
        stage: 0,
        slot: "a",
        need: Need::Hidden,
        dom_title: "Popup A|one",
        click: Click::Fixed(A_OPEN.0, A_OPEN.1),
        cumulative: 1,
    },
    Plan {
        stage: 1,
        slot: "a",
        need: Need::Visible,
        dom_title: "Popup A|one",
        click: Click::PopupMiddle,
        cumulative: 2,
    },
    Plan {
        stage: 2,
        slot: "a",
        need: Need::Hidden,
        dom_title: "Popup A|two",
        click: Click::Fixed(A_OPEN.0, A_OPEN.1),
        cumulative: 3,
    },
    Plan {
        stage: 3,
        slot: "a",
        need: Need::ReopenAfter(1),
        dom_title: "Popup A|two",
        click: Click::None,
        cumulative: 3,
    },
];
const B_PLANS: [Plan; 4] = [
    Plan {
        stage: 4,
        slot: "b",
        need: Need::Hidden,
        dom_title: "Popup B|one",
        click: Click::Fixed(B_OPEN.0, B_OPEN.1),
        cumulative: 1,
    },
    Plan {
        stage: 5,
        slot: "b",
        need: Need::Visible,
        dom_title: "Popup B|one",
        click: Click::PopupMiddle,
        cumulative: 2,
    },
    Plan {
        stage: 6,
        slot: "b",
        need: Need::Hidden,
        dom_title: "Popup B|two",
        click: Click::Fixed(B_OPEN.0, B_OPEN.1),
        cumulative: 3,
    },
    Plan {
        stage: 7,
        slot: "b",
        need: Need::ReopenAfter(5),
        dom_title: "Popup B|two",
        click: Click::None,
        cumulative: 3,
    },
];

/// Shared per-window bookkeeping between stages.
struct Track {
    /// Popup generation observed at each stage that showed a popup.
    opened: [u64; 9],
    /// Completed probe result of the last clicked stage for this slot.
    snapshot: Option<PointerSnapshot>,
    /// DOM title seen at the previous stage of this slot.
    last_title: Option<String>,
}

impl Track {
    fn new() -> Self {
        Self {
            opened: [0; 9],
            snapshot: None,
            last_title: None,
        }
    }
}

/// One stage: capture it, then (for clicked stages) prove the real OS gesture
/// produced exactly one new component submission.
#[allow(clippy::too_many_arguments)]
async fn stage(
    exec: &mut Exec<'_>,
    checks: &mut Checks,
    plan: &Plan,
    ctx: &SlotCtx,
    entity: &Entity<CefBrowser>,
    handle: &BrowserHandle,
    probe: &mut PointerProbe,
    known: &[WindowEntry],
    track: &mut Track,
) -> Result<(), String> {
    let shot = capture(
        exec,
        plan.stage,
        plan.slot,
        ctx,
        entity,
        handle,
        probe,
        known,
        plan.need,
        &track.opened,
        plan.dom_title,
        plan.click,
    )
    .await?;
    let prefix = if plan.slot == "a" {
        "popup_a"
    } else {
        "popup_b"
    };
    match plan.need {
        Need::Visible | Need::ReopenAfter(_) => {
            track.opened[plan.stage as usize] = shot.raw.popup_generation;
            checks.add(
                format!("{prefix}_visible_{}", plan.stage),
                shot.raw.popup_visible,
            );
            checks.add(
                format!("{prefix}_painted_{}", plan.stage),
                shot.diag.popup.paint_calls > 0
                    && shot.diag.popup.painted_generation == shot.raw.popup_generation
                    && shot.diag.popup.frame_width > 0
                    && shot.diag.popup.frame_height > 0,
            );
            if let Need::ReopenAfter(base) = plan.need {
                checks.add(
                    format!("{prefix}_reopen_generation_{}", plan.stage),
                    track
                        .opened
                        .get(base as usize)
                        .is_some_and(|previous| shot.raw.popup_generation > *previous),
                );
            }
        }
        Need::Hidden => {
            let cleared = !shot.raw.popup_visible && shot.raw.popup_sequence.is_none();
            if matches!(plan.stage, 2 | 6) {
                // Follows the real option click: the previously created popup
                // image must be released and the DOM title must have changed
                // through the selection, not by a synthetic post.
                checks.add(
                    format!("stage{}_popup_hidden", plan.stage),
                    cleared
                        && shot.diag.popup.created_images > 0
                        && shot.diag.popup.created_images == shot.diag.popup.released_images,
                );
                checks.add(
                    format!("selection_{}_title_two", plan.slot),
                    track
                        .last_title
                        .as_deref()
                        .is_some_and(|previous| previous != shot.title.as_str())
                        && shot.title == plan.dom_title,
                );
            } else {
                checks.add(format!("stage{}_popup_hidden", plan.stage), cleared);
            }
        }
    }
    track.last_title = Some(shot.title.clone());

    let armed = match (plan.click, shot.next_click) {
        (Click::None, None) => {
            checks.add(format!("stage{}_release_only", plan.stage), true);
            return Ok(());
        }
        (Click::None, Some(_)) => {
            return Err(format!("stage {} armed an unrequested click", plan.stage));
        }
        (_, None) => {
            return Err(format!("stage {} armed no click it requested", plan.stage));
        }
        (_, Some(point)) => physical_client(ctx.region, point, ctx.scale),
    };
    let snapshot = finish_click(
        exec,
        ctx,
        entity,
        probe,
        shot.diag.clicks_submitted,
        plan.cumulative,
        armed,
        &format!("stage {} {}", plan.stage, plan.slot),
    )
    .await?;
    track.snapshot = Some(snapshot);
    Ok(())
}

// ---------------------------------------------------------------------------
// Explicit entity teardown
// ---------------------------------------------------------------------------

struct LateA {
    before: ViewDiagnostics,
    after: ViewDiagnostics,
    closed_callbacks: u64,
    owned_wakes: u64,
    attempted: usize,
    removed: usize,
}

impl LateA {
    fn json(&self) -> String {
        object(vec![
            (
                "before",
                object(vec![
                    ("consumed", self.before.consumed_updates.to_string()),
                    ("paint", self.before.paint_calls.to_string()),
                    ("retired", bool_json(false)),
                    ("native_closed", bool_json(false)),
                ]),
            ),
            (
                "after",
                object(vec![
                    ("consumed", self.after.consumed_updates.to_string()),
                    ("paint", self.after.paint_calls.to_string()),
                    ("retired", bool_json(self.after.binding_retired)),
                    ("native_closed", bool_json(true)),
                ]),
            ),
            ("closed_callbacks", self.closed_callbacks.to_string()),
            ("owned_wakes", self.owned_wakes.to_string()),
            ("pruning_attempted", self.attempted.to_string()),
            ("pruning_removed", self.removed.to_string()),
        ])
    }
}

/// Stage-3 A boundary: retire the input probe while the popup is still shown,
/// remove the real window, wait for the real terminal notification, then let
/// exactly one dispatch prune the retired route without touching the old window.
#[allow(clippy::too_many_arguments)]
async fn tear_down_a(
    exec: &mut Exec<'_>,
    checks: &mut Checks,
    a_probe: &mut PointerProbe,
    a_retired: &mut bool,
    root: WindowHandle<HostRoot>,
    a_anchor: AnyWindowHandle,
    entity: &Entity<CefBrowser>,
    handle: &BrowserHandle,
) -> Result<LateA, String> {
    a_probe
        .retire()
        .map_err(|e| format!("A probe retire: {e}"))?;
    *a_retired = true;
    let browser = handle.snapshot().browser;
    // The popup stays shown while the probe is retired and the window is
    // destroyed: that is the exact late-notification boundary under test.
    let before = read_view(&mut *exec.cx, root, entity)?;
    if before.binding_retired {
        return Err("A binding was retired before its window was removed".into());
    }
    if !before.popup.visible {
        return Err("A popup was not visible when its window was removed".into());
    }
    exec.out.paint = Some(before.clone());

    let closed_before = exec.stats.closed.load(Ordering::Acquire);
    update(&mut *exec.cx, root, |_, window, _| window.remove_window())?;
    if app_update(&mut *exec.cx, |app| app.windows().contains(&a_anchor))? {
        return Err("A window was still registered after remove_window".into());
    }
    let close_deadline = exec.limit();
    let mut owned_wakes = 0_u64;
    loop {
        if handle.snapshot().state == SessionState::Closed
            || read_source(&mut *exec.rt, browser)?.raw.closed
        {
            break;
        }
        early_gate(exec.options)?;
        if Instant::now() >= close_deadline {
            return Err("A did not reach Closed/Source.closed after its window was removed".into());
        }
        // Retain real wake credits while waiting for the terminal state; a
        // timer may advance the deadline but cannot authorize consumption.
        if turn(&mut *exec.cx, &mut *exec.rt, exec.receiver, false).await? {
            owned_wakes += 1;
            exec.out.wake_consumed += 1;
        }
        exec.stats.check()?;
    }
    // The Closed event is emitted once native cleanup finishes, after
    // `Source.closed`; a bounded wait attributes the real callback without
    // dispatching through the retired route.
    let callback_deadline = exec.limit();
    while exec.stats.closed.load(Ordering::Acquire) == closed_before
        && Instant::now() < callback_deadline
    {
        early_gate(exec.options)?;
        if turn(&mut *exec.cx, &mut *exec.rt, exec.receiver, false).await? {
            owned_wakes += 1;
            exec.out.wake_consumed += 1;
        }
        exec.stats.check()?;
    }
    while owned_wakes == 0 {
        early_gate(exec.options)?;
        if Instant::now() >= callback_deadline {
            return Err("A retired-window notification had no real owned wake".into());
        }
        if turn(&mut *exec.cx, &mut *exec.rt, exec.receiver, false).await? {
            owned_wakes += 1;
            exec.out.wake_consumed += 1;
        }
        exec.stats.check()?;
    }
    let closed_callbacks = exec
        .stats
        .closed
        .load(Ordering::Acquire)
        .saturating_sub(closed_before);

    let (report, routes) = {
        let composition = &mut exec.composition;
        app_update(&mut *exec.cx, |app| {
            let report = composition.dispatch(app);
            (report, composition.len())
        })?
    };
    // The App window-closed observer retires A's token when the real window is
    // gone, so pruning happens before any route attempt: only B may be
    // attempted, and A must appear in neither `attempted` nor `failures`.
    if !report.failures.is_empty() || report.removed != 1 || report.attempted != 1 || routes != 1 {
        return Err(format!(
            "A route was not pruned without old-window access: attempted {} removed {} pending {} routes {} failures {}",
            report.attempted,
            report.removed,
            report.pending,
            routes,
            report.failures.len()
        ));
    }
    let after = read_view_erased(&mut *exec.cx, entity)?;
    let counters_same = after.consumed_updates == before.consumed_updates
        && after.paint_calls == before.paint_calls;
    if !counters_same || !after.binding_retired || before.binding_retired {
        return Err(format!(
            "A late-notification boundary moved: before {} consumed/{} paint retired {}; after {} consumed/{} paint retired {}",
            before.consumed_updates,
            before.paint_calls,
            before.binding_retired,
            after.consumed_updates,
            after.paint_calls,
            after.binding_retired
        ));
    }
    checks.add("a_window_destroyed", true);
    checks.add("a_native_closed_observed", true);
    checks.add("a_closed_callback_observed", closed_callbacks >= 1);
    checks.add("a_route_pruned_without_window_access", true);
    checks.add("a_counters_unchanged", counters_same);
    checks.add(
        "a_binding_retired",
        after.binding_retired && !before.binding_retired,
    );
    Ok(LateA {
        before,
        after,
        closed_callbacks,
        owned_wakes,
        attempted: report.attempted,
        removed: report.removed,
    })
}

/// Stage-7 B teardown: explicit two-layer release while the real window is
/// alive, unregister, drop the strong entity, then require a real rendered
/// green frame before stage 8.
#[allow(clippy::too_many_arguments)]
async fn tear_down_b(
    exec: &mut Exec<'_>,
    checks: &mut Checks,
    b_probe: &mut PointerProbe,
    b_retired: &mut bool,
    b_root: WindowHandle<HostRoot>,
    b_anchor: AnyWindowHandle,
    entity: Entity<CefBrowser>,
    b_id: BrowserId,
) -> Result<(ViewDiagnostics, bool), String> {
    b_probe
        .retire()
        .map_err(|e| format!("B probe retire: {e}"))?;
    *b_retired = true;
    let renders_before = update(&mut *exec.cx, b_root, |host, _, _| host.renders)?;
    let released = update(&mut *exec.cx, b_root, |host, window, cx| {
        // The component's explicit release clears the VIEW slot and then the
        // POPUP slot. Call it twice on purpose: the first clears both real
        // layers, and the second proves the release path is idempotent with
        // both owners already gone instead of silently resurrecting one.
        let mut errors = Vec::new();
        for attempt in ["first", "second"] {
            if let Err(error) =
                entity.update(cx, |view, view_cx| view.release_image(window, view_cx))
            {
                errors.push(format!("{attempt} release: {error:?}"));
            }
        }
        host.browser = None;
        host.detached = true;
        cx.notify();
        match errors.into_iter().next() {
            Some(error) => Err(error),
            None => Ok(entity.read(cx).diagnostics()),
        }
    })?
    .map_err(|e| format!("B release: {e:?}"))?;
    if !released_ok(&released) {
        return Err(format!(
            "B explicit release is not balanced: view {} created/{} released, popup {} created/{} released",
            released.created_images,
            released.released_images,
            released.popup.created_images,
            released.popup.released_images
        ));
    }
    checks.add("b_two_layer_release_balanced", true);

    // Explicit order: release both layers, remove the host slot (done above),
    // drop the local strong handle, then unregister the route. No dispatch runs
    // between these steps, so no half-torn route is ever observed.
    let weak = entity.downgrade();
    // The caller moved its only strong handle in here; the host slot is empty,
    // so this drop is what lets the real GPUI release observer run.
    drop(entity);
    if !exec.composition.unregister(b_id) {
        return Err("B unregister did not find its route".into());
    }
    if !exec.composition.is_empty() {
        return Err(format!(
            "composition still has {} routes after B unregister",
            exec.composition.len()
        ));
    }
    checks.add("final_routes_empty", true);
    // A weak entity may already be gone before the notified host renders; the
    // frame is required unconditionally, never skipped on WeakEntity::None.
    rendered(&mut *exec.cx, b_root).await?;
    let release_deadline = exec.limit();
    while weak.upgrade().is_some() {
        if Instant::now() >= release_deadline {
            return Err("B entity was not released by a real GPUI frame".into());
        }
        rendered(&mut *exec.cx, b_root).await?;
    }
    let (slots_empty, detached, renders_after) = update(&mut *exec.cx, b_root, |host, _, _| {
        (
            host.browser.is_none() && host.secondary.is_none(),
            host.detached,
            host.renders,
        )
    })?;
    let green_alive = slots_empty
        && detached
        && renders_after > renders_before
        && app_update(&mut *exec.cx, |app| app.windows().contains(&b_anchor))?;
    checks.add("b_entity_released", weak.upgrade().is_none());
    checks.add("b_host_green_alive", green_alive);
    if !green_alive {
        return Err(format!(
            "B host did not stay alive with the green fallback: slots_empty {slots_empty} detached {detached} renders {renders_before}->{renders_after}"
        ));
    }
    Ok((released, green_alive))
}

/// Stage-8 metadata: inactive green host, release only, no raw frame.
#[allow(clippy::too_many_arguments)]
async fn stage_eight(
    exec: &mut Exec<'_>,
    checks: &mut Checks,
    b_root: WindowHandle<HostRoot>,
    b_anchor: AnyWindowHandle,
    b_facts: WindowFacts,
    b_scale: f32,
    b_id: BrowserId,
    released: &ViewDiagnostics,
    dom_title: &str,
    known: &[WindowEntry],
) -> Result<(), String> {
    rendered(&mut *exec.cx, b_root).await?;
    let deadline = exec.limit();
    if !app_update(&mut *exec.cx, |app| app.windows().contains(&b_anchor))? {
        return Err("B window disappeared before stage 8".into());
    }
    let (empty, detached, renders) = update(&mut *exec.cx, b_root, |host, _, _| {
        (host.browser.is_none(), host.detached, host.renders)
    })?;
    if !(empty && detached && renders > 0) {
        return Err("stage 8 host is not the green fallback".into());
    }
    let meta = object(vec![
        ("schema", "1".into()),
        ("stage", "8".into()),
        ("pid", std::process::id().to_string()),
        ("target", quote("b")),
        ("title", quote(PEER_TITLE)),
        ("dom_title", quote(dom_title)),
        ("window", window_json(b_facts)),
        ("scale_factor", scale_text(b_scale)),
        (
            "view_bounds",
            format!(
                "[0,{},{},{}]",
                HEADER_DIP as i32, B_EXTENT.0 as i32, B_EXTENT.1 as i32
            ),
        ),
        ("browser", browser_json(b_id)),
        ("windows", windows_json(&mut *exec.cx, known)?),
        ("diagnostics", metrics_json(released)),
        ("popup", popup_json(released)),
        ("view_raw", "null".into()),
        ("popup_raw", "null".into()),
        ("next_click", "null".into()),
    ]);
    let case = exec
        .options
        .report
        .parent()
        .ok_or_else(|| "report path has no case directory".to_owned())?
        .to_path_buf();
    write_json(&case.join("popup-stage-8.json"), meta)?;
    // Release-only capture needs visibility, not foreground/input. B's input
    // observer is already retired; do not use GPUI's synthetic-ALT activation.
    atomic_write(
        &exec.options.gate.with_extension("popup-8.ready"),
        &std::process::id().to_string(),
    )?;
    exec.wait_release(8, deadline).await?;
    checks.add("stage8_green_host_alive", true);
    Ok(())
}

// ---------------------------------------------------------------------------
// Body
// ---------------------------------------------------------------------------

pub(super) async fn body(
    cx: &mut AsyncApp,
    rt: &mut Runtime,
    receiver: &UiWakeReceiver,
    stats: &Stats,
    root: WindowHandle<HostRoot>,
    options: &Options,
    out: &mut Outcome,
) -> Result<(), String> {
    if !options.interactive_input_approved {
        return Err("10.3 popup scenario requires interactive input approval".into());
    }
    let mut a_probe = PointerProbe::install(TITLE).map_err(|e| format!("A pointer probe: {e}"))?;
    let mut a_retired = false;
    let mut b_probe: Option<PointerProbe> = None;
    let mut b_retired = false;
    let result = run(
        cx,
        rt,
        receiver,
        stats,
        root,
        options,
        &mut *out,
        &mut a_probe,
        &mut a_retired,
        &mut b_probe,
        &mut b_retired,
    )
    .await;
    if result.is_err() {
        // The parent owns windows/entities/routes; retire only this module's
        // raw-input observers so no original window procedure survives.
        if !a_retired {
            let _ = a_probe.retire();
        }
        if let Some(probe) = b_probe.as_mut()
            && !b_retired
        {
            let _ = probe.retire();
        }
        if out.popup_detail.is_none() {
            out.popup_detail = Some(object(vec![
                ("success", bool_json(false)),
                ("stages", STAGES.to_string()),
                ("error", quote(&format!("{:?}", result.as_ref().err()))),
            ]));
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn run(
    cx: &mut AsyncApp,
    rt: &mut Runtime,
    receiver: &UiWakeReceiver,
    stats: &Stats,
    root: WindowHandle<HostRoot>,
    options: &Options,
    out: &mut Outcome,
    a_probe: &mut PointerProbe,
    a_retired: &mut bool,
    b_probe_slot: &mut Option<PointerProbe>,
    b_retired: &mut bool,
) -> Result<(), String> {
    let started = Instant::now();
    let scenario_deadline = started + SCENARIO_BUDGET;
    let ready_deadline = (started + Duration::from_secs(25)).min(scenario_deadline);
    while rt.state() != RuntimeState::Ready {
        if Instant::now() >= ready_deadline {
            return Err("Runtime readiness timeout".into());
        }
        let _ = turn(cx, rt, receiver, false).await?;
        stats.check()?;
    }

    let a_anchor = AnyWindowHandle::from(root);
    let a_facts = facts_of(a_probe)?;

    // Real second GPUI window with the same HostRoot type and one view.
    let b_root = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    point(px(760.), px(40.)),
                    size(px(520.), px(400.)),
                ))),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some(PEER_TITLE.into()),
                    ..Default::default()
                }),
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
                    renders: 0,
                    detached: false,
                    extent: B_EXTENT,
                })
            },
        )
        .map_err(|e| format!("peer window: {e}"))?;
    out.additional_roots.push(b_root);
    let b_anchor = AnyWindowHandle::from(b_root);
    *b_probe_slot =
        Some(PointerProbe::install(PEER_TITLE).map_err(|e| format!("B pointer probe: {e}"))?);
    let b_probe = b_probe_slot.as_mut().ok_or("B pointer probe missing")?;
    let b_facts = facts_of(b_probe)?;

    let a_scale = update(cx, root, |_, window, _| window.scale_factor())?;
    let b_scale = update(cx, b_root, |_, window, _| window.scale_factor())?;
    for (slot, scale) in [("a", a_scale), ("b", b_scale)] {
        if !scale.is_finite() || scale <= 0.0 {
            return Err(format!("invalid {slot} window scale {scale}"));
        }
    }

    let a_handle = rt
        .create_browser(browser_config(
            page_uri('A', A_BLUE, 220, 24, 400),
            options.backend,
            A_EXTENT,
            a_scale,
        ))
        .map_err(|e| format!("create A: {e:?}"))?;
    let b_handle = rt
        .create_browser(browser_config(
            page_uri('B', B_PURPLE, 24, 200, 140),
            options.backend,
            B_EXTENT,
            b_scale,
        ))
        .map_err(|e| format!("create B: {e:?}"))?;
    let a_id = a_handle.snapshot().browser;
    let b_id = b_handle.snapshot().browser;
    if a_id == b_id {
        return Err("Runtime returned the same BrowserId for both Browsers".into());
    }
    out.browser = Some(a_id);
    out.additional_browsers = vec![b_id];

    let a_attachment = rt
        .attach_browser(&a_handle)
        .map_err(|e| format!("attach A: {e:?}"))?;
    let b_attachment = rt
        .attach_browser(&b_handle)
        .map_err(|e| format!("attach B: {e:?}"))?;
    let duplicate_refused =
        rt.attach_browser(&a_handle).is_err() && rt.attach_browser(&b_handle).is_err();
    if !duplicate_refused {
        return Err("duplicate Runtime attachment was accepted".into());
    }
    out.duplicate_refused = duplicate_refused;

    let a_entity = update(cx, root, move |host, window, cx| {
        let entity = CefBrowser::new(a_attachment, window, cx)?;
        host.browser = Some(entity.clone());
        cx.notify();
        Ok::<_, ViewError>(entity)
    })?
    .map_err(|e| format!("A view construction: {e:?}"))?;
    let b_entity = update(cx, b_root, move |host, window, cx| {
        let entity = CefBrowser::new(b_attachment, window, cx)?;
        host.browser = Some(entity.clone());
        cx.notify();
        Ok::<_, ViewError>(entity)
    })?
    .map_err(|e| format!("B view construction: {e:?}"))?;

    let mut composition = UiComposition::new(NonZeroUsize::new(2).expect("nonzero capacity"));
    update(cx, root, |_, window, cx| {
        composition.register(&a_entity, window, &*cx)
    })?
    .map_err(|e| format!("register A: {e:?}"))?;
    // The peer entity is bound to the peer window; the same table must refuse
    // it through A's window before a slot is taken for B.
    let peer_wrong = update(cx, root, |_, window, cx| {
        composition.register(&b_entity, window, &*cx)
    })?;
    if !matches!(&peer_wrong, Err(CompositionError::WrongWindow { .. })) {
        return Err(format!(
            "peer registration through A's window was not refused: {:?}",
            peer_wrong.as_ref().err()
        ));
    }
    update(cx, b_root, |_, window, cx| {
        composition.register(&b_entity, window, &*cx)
    })?
    .map_err(|e| format!("register B: {e:?}"))?;
    if composition.len() != 2 {
        return Err(format!(
            "composition has {} routes after both registrations",
            composition.len()
        ));
    }

    let known: [WindowEntry; 2] = [
        ("a", TITLE, a_anchor, a_facts.hwnd),
        ("b", PEER_TITLE, b_anchor, b_facts.hwnd),
    ];
    let ctx_a = SlotCtx {
        root,
        browser: a_id,
        region: A_REGION,
        scale: a_scale,
        os_title: TITLE,
    };
    let ctx_b = SlotCtx {
        root: b_root,
        browser: b_id,
        region: B_REGION,
        scale: b_scale,
        os_title: PEER_TITLE,
    };
    let mut exec = Exec {
        cx,
        rt,
        receiver,
        stats,
        out,
        options,
        composition,
        deadline: scenario_deadline,
    };
    let live_windows = app_update(&mut *exec.cx, |app| app.windows().len())?;
    let mut checks = Checks::new();
    checks.add("interactive_input_approved", true);
    checks.add("runtime_ready", true);
    checks.add("two_gpui_windows", live_windows == 2);
    checks.add("two_browsers_distinct", a_id != b_id);
    checks.add("duplicate_attachment_refused", duplicate_refused);
    checks.add("peer_window_identity_refused", true);
    checks.add("composition_capacity_two", true);

    let mut a_track = Track::new();
    let mut b_track = Track::new();

    // -----------------------------------------------------------------
    // A: baseline -> popup -> selection -> independent reopen -> teardown.
    // -----------------------------------------------------------------
    for plan in &A_PLANS {
        stage(
            &mut exec,
            &mut checks,
            plan,
            &ctx_a,
            &a_entity,
            &a_handle,
            &mut *a_probe,
            &known,
            &mut a_track,
        )
        .await?;
    }
    let late = tear_down_a(
        &mut exec,
        &mut checks,
        &mut *a_probe,
        &mut *a_retired,
        root,
        a_anchor,
        &a_entity,
        &a_handle,
    )
    .await?;
    if b_handle.snapshot().state != SessionState::Alive {
        return Err(format!(
            "B is not Alive after A's removal: {:?}",
            b_handle.snapshot().state
        ));
    }
    checks.add("b_alive_after_a_removed", true);
    let post_a = app_update(&mut *exec.cx, |app| {
        let live = app.windows();
        (live.contains(&b_anchor), live.contains(&a_anchor))
    })?;
    checks.add("only_b_window_after_stage3", post_a == (true, false));
    drop(a_entity);

    // -----------------------------------------------------------------
    // B: same chain in the peer window, still alive after A closed.
    // -----------------------------------------------------------------
    for plan in &B_PLANS {
        stage(
            &mut exec,
            &mut checks,
            plan,
            &ctx_b,
            &b_entity,
            &b_handle,
            &mut *b_probe,
            &known,
            &mut b_track,
        )
        .await?;
    }
    let (b_released, green_alive) = tear_down_b(
        &mut exec,
        &mut checks,
        &mut *b_probe,
        &mut *b_retired,
        b_root,
        b_anchor,
        b_entity,
        b_id,
    )
    .await?;
    exec.out.released_view = Some(b_released.clone());
    let b_title = b_track.last_title.clone().unwrap_or_default();

    // -----------------------------------------------------------------
    // Stage 8: inactive green host, release only.
    // -----------------------------------------------------------------
    stage_eight(
        &mut exec,
        &mut checks,
        b_root,
        b_anchor,
        b_facts,
        b_scale,
        b_id,
        &b_released,
        &b_title,
        &known,
    )
    .await?;

    // The launcher releases the original observer gate in its finally block,
    // only after all nine captures. Never return before it.
    let observer_deadline = Instant::now() + OBSERVER_WAIT;
    while !exec.options.gate.is_file() {
        if Instant::now() >= observer_deadline {
            return Err("original observer release timeout".into());
        }
        let _ = exec.step(false).await?;
    }
    checks.add("observer_release_observed", true);

    let a_snapshot = a_track.snapshot.as_ref();
    let b_snapshot = b_track.snapshot.as_ref();
    checks.add("clicks_a_three", late.before.clicks_submitted == 3);
    checks.add("clicks_b_three", b_released.clicks_submitted == 3);
    checks.add(
        "clicks_rejected_zero",
        late.before.clicks_rejected == 0 && b_released.clicks_rejected == 0,
    );
    checks.add(
        "input_a_pairs",
        a_snapshot.is_some_and(|s| s.down == 3 && s.up == 3),
    );
    checks.add(
        "input_b_pairs",
        b_snapshot.is_some_and(|s| s.down == 3 && s.up == 3),
    );
    checks.add(
        "exactly_two_browsers_created",
        exec.stats.created.load(Ordering::Acquire) == 2,
    );
    checks.add(
        "no_borrow_violations",
        exec.stats.borrow_violations.load(Ordering::Acquire) == 0,
    );
    checks.add("stages_completed", true);

    let success = checks.all();
    let detail = object(vec![
        ("success", bool_json(success)),
        ("stages", STAGES.to_string()),
        ("checks", checks.json()),
        ("window_a_closed", bool_json(true)),
        (
            "browser_ids",
            object(vec![("a", browser_json(a_id)), ("b", browser_json(b_id))]),
        ),
        ("late", late.json()),
        (
            "input",
            format!(
                "[{},{}]",
                a_snapshot.map_or_else(|| "null".into(), pointer_json),
                b_snapshot.map_or_else(|| "null".into(), pointer_json)
            ),
        ),
        (
            "views",
            format!(
                "[{},{}]",
                metrics_json(&late.before),
                metrics_json(&b_released)
            ),
        ),
    ]);
    exec.out.popup_detail = Some(detail);
    exec.out.view_release_observed_closing = true;
    exec.out.detached_alive = green_alive;

    if !success {
        return Err("10.3 popup validation checks failed".into());
    }
    Ok(())
}

/// Minimal ordered check list shared with the detailed JSON.
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
