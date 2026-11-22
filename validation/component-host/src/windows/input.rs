//! 11.1 actual GPUI -> component -> Runtime input, not a direct CEF harness.
//! The fixed data fixture exposes only counters, UTF-16 length and an FNV-1a
//! digest of its own fixed ASCII text in document.title. Never log input text.
//! Source BGRA is expected pixels only: the external observer must capture a
//! separate real owned-client RGB PNG and compare its entire VIEW clip exactly.
//!
//! Parent integration: mod input; mod input_probe; HostScenario::Input with
//! explicit approval; body() in scenario; Outcome.input_detail in JSON; add
//! the dedicated Input stage loop to the launcher. Keep the existing two-
//! Browser shutdown, 2-created/2-closed and released-view checks unchanged.

mod ime_observe;

use super::input_probe::{Event, InputProbe, Trace, WindowFacts};
use super::*;
use gpui::{AnyWindowHandle, point};
use rust_cef_gpui::{
    CefBrowser, InputSnapshot, NativeKeyboardDiagnostics, NativeKeyboardServices, UiComposition,
    ViewError,
    contracts::{BrowserConfig, SessionState, Viewport},
    ports::OwnedFrame,
};
use std::num::NonZeroUsize;

const PEER_TITLE: &str = "GPUI-CEF input peer";
const HEADER: i32 = 48;
const EXTENT: (f32, f32) = (320., 240.);
const WAIT: Duration = Duration::from_secs(20);
const BUDGET: Duration = Duration::from_secs(105);
const STAGES: u32 = 6;
const FIXED: &str = "abc";

fn uri(html: &str) -> String {
    let mut out = String::from("data:text/html,");
    for b in html.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'~') {
            out.push(b as char)
        } else {
            out.push_str(&format!("%{b:02X}"))
        }
    }
    out
}
fn page(peer: bool) -> String {
    if peer {
        return uri(
            "<html><body style='margin:0;background:rgb(26,76,126)'><title>Input peer</title></body></html>",
        );
    }
    uri(
        "<html><head><title>Input|n=0|h=2166136261|f=0|b=0|d=0|u=0|kd=0|ku=0|i=0|m=0|w=0|cs=0|ce=0</title><style>html,body{margin:0;background:rgb(26,76,126)}input{position:absolute;left:32px;top:32px;width:180px;height:30px;font:16px monospace}</style></head><body><input id='v' autocomplete='off' spellcheck='false'><script>const v=document.getElementById('v');let c={f:0,b:0,d:0,u:0,kd:0,ku:0,i:0,m:0,w:0,cs:0,ce:0};function title(){let h=2166136261;for(let i=0;i<v.value.length;i++){h=Math.imul(h^v.value.charCodeAt(i),16777619)>>>0}document.title='Input|n='+v.value.length+'|h='+h+Object.keys(c).map(k=>'|'+k+'='+c[k]).join('')}for(const [name,key,target] of [['focus','f',v],['blur','b',v],['mousedown','d',document],['mouseup','u',document],['keydown','kd',document],['keyup','ku',document],['input','i',v],['mousemove','m',document],['wheel','w',document],['compositionstart','cs',v],['compositionend','ce',v]])target.addEventListener(name,()=>{c[key]++;title()});title()</script></body></html>",
    )
}
fn fnv(text: &str) -> u32 {
    text.encode_utf16().fold(2166136261u32, |h, c| {
        (h ^ u32::from(c)).wrapping_mul(16777619)
    })
}
fn field(title: &str, key: &str) -> Result<u64, String> {
    title
        .strip_prefix("Input|")
        .ok_or("not input fixture DOM title")?
        .split('|')
        .find_map(|part| part.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
        .ok_or_else(|| format!("DOM title missing {key}"))?
        .parse()
        .map_err(|_| format!("DOM title {key} is not numeric"))
}
fn expected_title(
    title: &str,
    n: usize,
    focus: u64,
    blur: u64,
    down: u64,
    keys: u64,
    input: u64,
    wheel: u64,
) -> Result<(), String> {
    let expected = &FIXED[..n];
    for (name, value) in [
        ("n", n as u64),
        ("h", u64::from(fnv(expected))),
        ("f", focus),
        ("b", blur),
        ("d", down),
        ("kd", keys),
        ("ku", keys),
        ("i", input),
        ("w", wheel),
        ("cs", 0),
        ("ce", 0),
    ] {
        let got = field(title, name)?;
        if got != value {
            return Err(format!("DOM {name}={got}, expected {value}"));
        }
    }
    // A release outside the VIEW may or may not be delivered into DOM after
    // capture is lost. Never turn an unobserved DOM mouseup into evidence.
    if field(title, "u")? > down {
        return Err("DOM mouseup exceeded actual mouse downs".into());
    }
    let _ = field(title, "m")?;
    Ok(())
}
fn config(url: String, backend: RenderPreference, scale: f32) -> BrowserConfig {
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
fn atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension(format!(
        "{}.pending",
        path.extension().and_then(|s| s.to_str()).unwrap_or("gate")
    ));
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}
fn input_json(s: &InputSnapshot) -> String {
    object(vec![
        ("epoch", s.epoch.to_string()),
        ("admitted", s.admitted.to_string()),
        ("executed", s.executed.to_string()),
        ("rejected", s.rejected.to_string()),
        ("inflight", s.inflight.to_string()),
        (
            "native_execution_unconfirmed",
            bool_json(s.native_execution_unconfirmed),
        ),
        ("pressed_buttons", s.pressed_buttons.to_string()),
        ("pressed_keys", s.pressed_keys.to_string()),
        ("composition_active", bool_json(s.composition_active)),
        ("cleanup_pending", bool_json(s.cleanup_pending)),
        ("cleanup_failed", bool_json(s.cleanup_failed)),
        ("accepts_text", bool_json(s.accepts_text)),
        (
            "last_native_error",
            s.last_native_error
                .as_ref()
                .map_or_else(|| "null".into(), |e| quote(&format!("{e:?}"))),
        ),
    ])
}
fn native_keyboard_json(d: &NativeKeyboardDiagnostics) -> String {
    object(vec![
        ("intercepted_down", d.intercepted_down.to_string()),
        ("intercepted_up", d.intercepted_up.to_string()),
        ("routed_down", d.routed_down.to_string()),
        ("routed_up", d.routed_up.to_string()),
        ("host_shortcuts", d.host_shortcuts.to_string()),
        ("refused", d.refused.to_string()),
        ("cleanup_attempts", d.cleanup_attempts.to_string()),
        ("cleanup_failures", d.cleanup_failures.to_string()),
        (
            "last_cleanup_error",
            d.last_cleanup_error
                .as_ref()
                .map_or_else(|| "null".into(), |e| quote(&format!("{e:?}"))),
        ),
        ("unclaimed", d.unclaimed.to_string()),
        ("system_text_skipped", d.system_text_skipped.to_string()),
        (
            "unrepresented_lock_state",
            d.unrepresented_lock_state.to_string(),
        ),
        ("orphan_up", d.orphan_up.to_string()),
        ("panics", d.panics.to_string()),
        ("hook_live", bool_json(d.hook_live)),
        ("properties_live", d.properties_live.to_string()),
        ("lease_live", bool_json(d.lease_live)),
        ("retired", bool_json(d.retired)),
        ("cancelled_keys", d.cancelled_keys.to_string()),
        (
            "last_failure",
            d.last_failure
                .as_ref()
                .map_or_else(|| "null".into(), |e| quote(&format!("{e:?}"))),
        ),
    ])
}
fn callbacks_json(diag: &rust_cef_gpui::ViewDiagnostics) -> String {
    let c = &diag.input.callbacks;
    object(vec![
        ("key_down_entered", c.key_down_entered.to_string()),
        ("key_up_entered", c.key_up_entered.to_string()),
        ("replace_text_entered", c.replace_text_entered.to_string()),
        (
            "replace_and_mark_text_entered",
            c.replace_and_mark_text_entered.to_string(),
        ),
        (
            "focused_handler_paint_attempts",
            c.focused_handler_paint_attempts.to_string(),
        ),
        ("key_down_rejected", c.key_down_rejected.to_string()),
        ("key_up_rejected", c.key_up_rejected.to_string()),
        ("replace_text_rejected", c.replace_text_rejected.to_string()),
        (
            "replace_and_mark_text_rejected",
            c.replace_and_mark_text_rejected.to_string(),
        ),
        (
            "last_rejection",
            c.last_rejection
                .as_ref()
                .map_or_else(|| "null".into(), |r| quote(&format!("{r:?}"))),
        ),
    ])
}
fn facts_json(f: WindowFacts) -> String {
    object(vec![
        ("hwnd", f.hwnd.to_string()),
        ("pid", f.pid.to_string()),
        ("thread", f.thread.to_string()),
        ("dpi", f.dpi.to_string()),
        ("left", f.left.to_string()),
        ("top", f.top.to_string()),
        ("width", f.width.to_string()),
        ("height", f.height.to_string()),
        (
            "ime",
            object(vec![
                ("keyboard_layout", f.ime.keyboard_layout.to_string()),
                ("is_ime", bool_json(f.ime.is_ime)),
                ("associated", bool_json(f.ime.associated)),
                ("open", f.ime.open.map_or_else(|| "null".into(), bool_json)),
                (
                    "conversion",
                    f.ime
                        .conversion
                        .map_or_else(|| "null".into(), |v| v.to_string()),
                ),
                (
                    "query_origin",
                    quote(
                        "verified HWND owner thread; IMM snapshot, not an atomic future guarantee",
                    ),
                ),
            ]),
        ),
    ])
}
fn raw_json(frame: &OwnedFrame, file: &str) -> String {
    object(vec![
        ("file", quote(file)),
        (
            "browser",
            format!(
                "[{},{}]",
                frame.info.browser.runtime().value(),
                frame.info.browser.local()
            ),
        ),
        ("surface", quote("View")),
        ("generation", frame.info.surface_generation.to_string()),
        ("sequence", frame.info.sequence.to_string()),
        ("width", frame.info.width.to_string()),
        ("height", frame.info.height.to_string()),
        ("bytes", frame.pixels.as_slice().len().to_string()),
    ])
}
fn event_json(event: Event) -> String {
    match event {
        Event::Down(x, y) => object(vec![
            ("kind", quote("down")),
            ("x", x.to_string()),
            ("y", y.to_string()),
        ]),
        Event::RightDown(x, y) => object(vec![
            ("kind", quote("right-down")),
            ("x", x.to_string()),
            ("y", y.to_string()),
        ]),
        Event::RightUp(x, y) => object(vec![
            ("kind", quote("right-up")),
            ("x", x.to_string()),
            ("y", y.to_string()),
        ]),
        Event::Move(x, y) => object(vec![
            ("kind", quote("move")),
            ("x", x.to_string()),
            ("y", y.to_string()),
        ]),
        Event::Up(x, y) => object(vec![
            ("kind", quote("up")),
            ("x", x.to_string()),
            ("y", y.to_string()),
        ]),
        Event::Wheel(x, y, delta) => object(vec![
            ("kind", quote("wheel")),
            ("x", x.to_string()),
            ("y", y.to_string()),
            ("delta", delta.to_string()),
        ]),
        Event::KeyDown(v) => object(vec![("kind", quote("keydown")), ("code", v.to_string())]),
        Event::Char(v) => object(vec![("kind", quote("char")), ("code", v.to_string())]),
        Event::KeyUp(v) => object(vec![("kind", quote("keyup")), ("code", v.to_string())]),
    }
}
fn events_json(events: impl IntoIterator<Item = Event>) -> String {
    format!(
        "[{}]",
        events
            .into_iter()
            .map(event_json)
            .collect::<Vec<_>>()
            .join(",")
    )
}
pub(super) fn trace_json(trace: &Trace) -> String {
    object(vec![
        ("expected", events_json(trace.expected.iter().copied())),
        (
            "observed",
            events_json(
                trace
                    .records
                    .iter()
                    .filter(|m| m.accepted)
                    .filter_map(|m| m.event),
            ),
        ),
        (
            "raw",
            format!(
                "[{}]",
                trace
                    .records
                    .iter()
                    .map(|m| object(vec![
                        ("message", m.message.to_string()),
                        ("wparam", m.wparam.to_string()),
                        ("lparam", m.lparam.to_string()),
                        ("raw_extra_info", m.raw_extra_info.to_string()),
                        ("observation_point", quote(m.observation_point)),
                        ("event", m.event.map_or_else(|| "null".into(), event_json)),
                        ("char_associated", bool_json(m.char_associated)),
                        ("accepted", bool_json(m.accepted)),
                        ("during_manual_pump", bool_json(m.during_manual_pump)),
                        (
                            "input_snapshot_before_forward",
                            m.input_snapshot
                                .as_ref()
                                .map_or_else(|| "null".into(), input_json)
                        ),
                    ]))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        ),
        ("complete", bool_json(trace.complete)),
        (
            "error",
            trace
                .error
                .as_ref()
                .map_or_else(|| "null".into(), |e| quote(e)),
        ),
    ])
}
/// Persist both complete raw FIFOs even on a rejected/incomplete stage.
/// A missing probe snapshot is a failure, never a fabricated empty FIFO.
fn observed(
    case: &Path,
    stage: u32,
    a: &InputProbe,
    b: &InputProbe,
    action_error: Option<&str>,
) -> Result<(), String> {
    let a_trace = a.trace();
    let b_trace = b.trace();
    let successful = action_error.is_none()
        && a_trace.as_ref().is_ok_and(|t| t.complete)
        && b_trace.as_ref().is_ok_and(|t| t.complete);
    let window_json = |slot: &str, probe: &InputProbe, result: &Result<Trace, String>| {
        object(vec![
            ("slot", quote(slot)),
            (
                "hwnd",
                probe
                    .info()
                    .map_or_else(|_| "null".into(), |f| f.hwnd.to_string()),
            ),
            (
                "trace",
                result.as_ref().map_or_else(
                    |e| object(vec![("error", quote(e)), ("complete", bool_json(false))]),
                    trace_json,
                ),
            ),
        ])
    };
    let text = object(vec![
        ("schema", "1".into()),
        ("kind", quote("component-input-wm")),
        ("pid", std::process::id().to_string()),
        ("stage", stage.to_string()),
        ("expected_marker", "1128678706".into()), // 0x43464532, expectation only
        (
            "windows",
            format!(
                "[{},{}]",
                window_json("a", a, &a_trace),
                window_json("b", b, &b_trace)
            ),
        ),
        ("success", bool_json(successful)),
        ("error", action_error.map_or_else(|| "null".into(), quote)),
    ]);
    write_json(&case.join(format!("input-observed-{stage}.json")), text)?;
    if !successful {
        return Err(format!(
            "Input stage {stage} raw queue/WNDPROC evidence incomplete or rejected"
        ));
    }
    Ok(())
}
fn good(s: &InputSnapshot) -> bool {
    s.inflight == 0
        && !s.native_execution_unconfirmed
        && s.pressed_buttons == 0
        && s.pressed_keys == 0
        && !s.composition_active
        && !s.cleanup_pending
        && !s.cleanup_failed
        && s.last_native_error.is_none()
        && s.rejected == 0
}
fn view(
    cx: &mut AsyncApp,
    root: WindowHandle<HostRoot>,
    entity: &Entity<CefBrowser>,
) -> Result<ViewDiagnostics, String> {
    update(cx, root, |_, _, cx| entity.read(cx).diagnostics())
}
fn physical(x: i32, y: i32, scale: f32) -> (i32, i32) {
    (
        ((x as f64) * scale as f64 + 0.5).floor() as i32,
        ((y as f64) * scale as f64 + 0.5).floor() as i32,
    )
}
fn events(stage: u32, scale: f32) -> Vec<Event> {
    let p = |x, y| physical(x, y, scale);
    let ((ix, iy), (ox, oy), (wx, wy)) = (p(90, HEADER + 46), p(90, 24), p(250, HEADER + 160));
    match stage {
        0 => vec![
            Event::Down(ix, iy),
            Event::Up(ix, iy),
            Event::KeyDown(0x41),
            Event::Char(0x61),
            Event::KeyUp(0x41),
        ],
        1 => vec![
            Event::KeyDown(0x42),
            Event::Char(0x62),
            Event::KeyUp(0x42),
            Event::Down(ix, iy),
            Event::Move(ox, oy),
            Event::Up(ox, oy),
            Event::Wheel(wx, wy, -120),
        ],
        2 => {
            let (x, y) = p(440, 24);
            vec![Event::Down(x, y), Event::Up(x, y)]
        }
        3 => vec![
            Event::Down(ix, iy),
            Event::Up(ix, iy),
            Event::KeyDown(0x43),
            Event::Char(0x63),
            Event::KeyUp(0x43),
        ],
        _ => vec![],
    }
}
struct Exec<'a> {
    cx: &'a mut AsyncApp,
    rt: &'a mut Runtime,
    receiver: &'a UiWakeReceiver,
    stats: &'a Stats,
    out: &'a mut Outcome,
    options: &'a Options,
    composition: UiComposition,
    deadline: Instant,
    pump_markers: [std::rc::Rc<Cell<bool>>; 2],
    native_keyboard: NativeKeyboardServices,
    completed: bool,
}
impl Drop for Exec<'_> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        // Preserve early failure resource truth as well as the original run
        // error. In particular foreground denial occurs before stage JSONs.
        let retirement = self.native_keyboard.retire();
        let diagnostics = self.native_keyboard.diagnostics();
        self.out.input_detail = Some(object(vec![
            ("success", bool_json(false)),
            (
                "stages",
                if self.options.scenario == HostScenario::ImeObserve {
                    2
                } else {
                    STAGES
                }
                .to_string(),
            ),
            ("native_keyboard", native_keyboard_json(&diagnostics)),
            (
                "retirement_error",
                retirement
                    .err()
                    .map_or_else(|| "null".into(), |e| quote(&format!("{e:?}"))),
            ),
        ]));
    }
}
impl Exec<'_> {
    async fn step(&mut self) -> Result<(), String> {
        let woke = turn_observed(
            self.cx,
            self.rt,
            self.receiver,
            false,
            Some(&self.pump_markers),
        )
        .await?;
        self.stats.check()?;
        let native = self.native_keyboard.diagnostics();
        if native.last_failure.is_some() || native.panics != 0 || native.cleanup_failures != 0 {
            self.out.input_detail = Some(object(vec![
                ("success", bool_json(false)),
                ("native_keyboard", native_keyboard_json(&native)),
            ]));
            return Err(format!("native keyboard ingress unconfirmed: {native:?}"));
        }
        if woke {
            self.out.wake_consumed += 1;
            let report = app_update(self.cx, |app| self.composition.dispatch(app))?;
            if !report.failures.is_empty() || report.removed != 0 {
                return Err(format!("input dispatch failure: {:?}", report.failures));
            }
        }
        Ok(())
    }
    async fn retry_once(&mut self) -> Result<(), String> {
        // Only on this subsequent explicit GPUI/UI turn; no Runtime pump or
        // self-retry is hidden in the component. The actual Runtime snapshot
        // and DOM, rather than this report's accepted count, close the loop.
        let report = app_update(self.cx, |app| self.composition.retry_input(app))?;
        if !report.failures.is_empty() || report.removed != 0 {
            return Err(format!("input retry failed: {:?}", report.failures));
        }
        Ok(())
    }
    async fn release(
        &mut self,
        stage: u32,
        owner: &InputProbe,
        peer: Option<&InputProbe>,
        root: WindowHandle<HostRoot>,
    ) -> Result<(), String> {
        let release = self
            .options
            .gate
            .with_extension(format!("input-{stage}.release"));
        let until = (Instant::now() + WAIT).min(self.deadline);
        let case = self
            .options
            .report
            .parent()
            .ok_or("owner response has no case directory")?;
        let request = case.join(format!("input-owner-ime-{stage}.request"));
        let response = case.join(format!("input-owner-ime-{stage}.json"));
        let mut served = false;
        let activate_request = case.join(format!("input-owner-foreground-{stage}.request"));
        let activate_response = case.join(format!("input-owner-foreground-{stage}.json"));
        let mut activated = false;
        while !release.is_file() {
            if self.options.gate.is_file() {
                return Err("observer released before Input stage finished".into());
            }
            if Instant::now() >= until {
                return Err(format!("Input stage {stage} release timeout"));
            }
            self.step().await?;
            if activate_request.is_file() && !activated {
                if stage != 2 {
                    return Err("unexpected peer activation request outside stage 2".into());
                }
                let peer = peer.ok_or("peer activation target missing")?;
                let source = owner.info()?;
                let target = peer.info()?;
                let expected = format!(
                    "{}:{stage}:{}:{}",
                    std::process::id(),
                    source.hwnd,
                    target.hwnd
                );
                if std::fs::read_to_string(&activate_request)
                    .map_err(|e| e.to_string())?
                    .trim()
                    != expected
                {
                    return Err("peer activation request PID/stage/HWND mismatch".into());
                }
                for probe in [owner, peer] {
                    let trace = probe.trace()?;
                    if trace.error.is_some() || !trace.records.is_empty() {
                        return Err("peer activation request after unexpected input".into());
                    }
                }
                // Only the already-foreground owner process requests its own
                // peer, after the driver saved the pre-blur screenshot. No
                // foreground-policy bypass, foreign activation or retries.
                if !owner.is_foreground()? {
                    return Err("peer activation source no longer foreground".into());
                }
                peer.foreground()?;
                atomic(
                    &activate_response,
                    &object(vec![
                        ("schema", "1".into()),
                        ("kind", quote("input-owner-foreground")),
                        ("pid", std::process::id().to_string()),
                        ("stage", stage.to_string()),
                        ("request", quote(&expected)),
                        ("source", facts_json(source)),
                        ("target", facts_json(target)),
                        ("source_was_foreground", bool_json(true)),
                        ("target_is_foreground", bool_json(peer.is_foreground()?)),
                        ("raw_count", "0".into()),
                    ]),
                )?;
                activated = true;
            }
            if request.is_file() && !served {
                if !matches!(stage, 0 | 1 | 3) {
                    return Err("unexpected keyboard owner request in pointer-only stage".into());
                }
                // Install the focused handler, then take all response facts
                // without another await; a pre-draw trace is not a cutpoint.
                rendered(self.cx, root).await?;
                let expected_raw = if stage == 1 { 0 } else { 2 };
                let trace = owner.trace()?;
                if trace.error.is_some() {
                    return Err("owner request after rejected raw input".into());
                }
                if trace.records.len() < expected_raw {
                    continue;
                }
                let keyboard_seen = trace.records.iter().any(|message| {
                    matches!(
                        message.event,
                        Some(Event::KeyDown(_) | Event::Char(_) | Event::KeyUp(_))
                    )
                });
                if trace.records.len() != expected_raw || keyboard_seen {
                    return Err(
                        "owner request did not precede keyboard at the exact pointer cutpoint"
                            .into(),
                    );
                }
                if !owner.accepts_text()? {
                    continue;
                }
                let facts = owner.info()?;
                let expected = format!("{}:{stage}:{}", std::process::id(), facts.hwnd);
                if std::fs::read_to_string(&request)
                    .map_err(|e| e.to_string())?
                    .trim()
                    != expected
                {
                    return Err("owner IME request PID/stage/HWND mismatch".into());
                }
                atomic(
                    &response,
                    &object(vec![
                        ("schema", "1".into()),
                        ("kind", quote("input-owner-ime")),
                        ("pid", std::process::id().to_string()),
                        ("stage", stage.to_string()),
                        ("request", quote(&expected)),
                        ("window", facts_json(facts)),
                        ("raw_count", trace.records.len().to_string()),
                        ("keyboard_seen", bool_json(false)),
                        ("accepts_text", bool_json(true)),
                    ]),
                )?;
                served = true;
            }
        }
        if std::fs::read_to_string(release)
            .map_err(|e| e.to_string())?
            .trim()
            != std::process::id().to_string()
        {
            return Err("Input release PID mismatch".into());
        }
        Ok(())
    }
}

/// The parent's outer scenario owns shutdown and retains both roots on error.
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
        return Err("11.1 real input requires explicit interactive approval".into());
    }
    let mut a = InputProbe::install(TITLE, 0)?;
    let mut b: Option<InputProbe> = None;
    let mut result = run(cx, rt, receiver, stats, root, options, out, &mut a, &mut b).await;
    if let Err(error) = &result {
        // The last released stage's trace is not the end-of-scenario trace.
        // Preserve late/unarmed messages and sticky rejection before retirement.
        let trace = |probe: &InputProbe| {
            probe.trace().map_or_else(
                |e| object(vec![("query_error", quote(&e))]),
                |t| trace_json(&t),
            )
        };
        let evidence = object(vec![
            ("schema", "1".into()),
            ("kind", quote("input-final-failure-wm")),
            ("success", bool_json(false)),
            ("pid", std::process::id().to_string()),
            ("original_error", quote(error)),
            ("a", trace(&a)),
            ("b", b.as_ref().map_or_else(|| "null".into(), trace)),
        ]);
        let saved = options
            .report
            .parent()
            .ok_or_else(|| "input case directory missing".to_string())
            .and_then(|case| atomic(&case.join("input-final-failure-wm.json"), &evidence));
        if let Err(save_error) = saved {
            result = Err(format!(
                "{error}; final WM observation save failed: {save_error}"
            ));
        }
    }
    // On all exits restore only our own WNDPROCs; a failed restore keeps its
    // state allocated through RAII rather than freeing a reachable subclass.
    let retire_a = a.retire();
    let retire_b = b.as_mut().map(InputProbe::retire).transpose();
    if let Err(e) = retire_a {
        return Err(format!("input A probe retire: {e}; prior: {result:?}"));
    }
    if let Err(e) = retire_b {
        return Err(format!("input B probe retire: {e}; prior: {result:?}"));
    }
    if result.is_err() && out.input_detail.is_none() {
        out.input_detail = Some(object(vec![
            ("success", bool_json(false)),
            (
                "stages",
                if options.scenario == HostScenario::ImeObserve {
                    2
                } else {
                    STAGES
                }
                .to_string(),
            ),
        ]));
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
    a: &mut InputProbe,
    b: &mut Option<InputProbe>,
) -> Result<(), String> {
    let deadline = Instant::now() + BUDGET;
    while rt.state() != RuntimeState::Ready {
        if Instant::now() >= deadline {
            return Err("input runtime readiness timeout".into());
        }
        turn(cx, rt, receiver, false).await?;
        stats.check()?
    }
    let peer = cx
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
                    extent: EXTENT,
                })
            },
        )
        .map_err(|e| format!("input peer window: {e}"))?;
    out.additional_roots.push(peer);
    *b = Some(InputProbe::install(PEER_TITLE, 1)?);
    let a_fact = a.info()?;
    let b_fact = b.as_ref().ok_or("missing input peer probe")?.info()?;
    let a_scale = update(cx, root, |_, w, _| w.scale_factor())?;
    let b_scale = update(cx, peer, |_, w, _| w.scale_factor())?;
    if a_scale <= 0. || !a_scale.is_finite() || b_scale <= 0. || !b_scale.is_finite() {
        return Err("invalid input window scale".into());
    }
    let handle = rt
        .create_browser(config(page(false), options.backend, a_scale))
        .map_err(|e| format!("input A create: {e:?}"))?;
    let peer_handle = rt
        .create_browser(config(
            page(options.scenario != HostScenario::ImeObserve),
            options.backend,
            b_scale,
        ))
        .map_err(|e| format!("input B create: {e:?}"))?;
    let id = handle.snapshot().browser;
    let peer_id = peer_handle.snapshot().browser;
    if id == peer_id {
        return Err("same BrowserId for two input fixtures".into());
    }
    a.observe_browser(handle.clone())?;
    b.as_ref()
        .ok_or("missing input peer probe")?
        .observe_browser(peer_handle.clone())?;
    out.browser = Some(id);
    out.additional_browsers.push(peer_id);
    if options.scenario == HostScenario::ImeObserve {
        // No input-bound View exists during Creating in this environment-only
        // scenario. Keep every subsequent zero-rejection/FIFO gate unchanged.
        ime_observe::wait_created(
            cx,
            rt,
            receiver,
            stats,
            deadline,
            [&handle, &peer_handle],
            &options.report,
        )
        .await?;
    }
    let attach = rt
        .attach_browser(&handle)
        .map_err(|e| format!("input A attach: {e:?}"))?;
    let peer_attach = rt
        .attach_browser(&peer_handle)
        .map_err(|e| format!("input B attach: {e:?}"))?;
    out.duplicate_refused =
        rt.attach_browser(&handle).is_err() && rt.attach_browser(&peer_handle).is_err();
    if !out.duplicate_refused {
        return Err("input duplicate attachment accepted".into());
    }
    let entity = update(cx, root, move |host, window, cx| {
        let v = CefBrowser::new(attach, window, cx)?;
        host.browser = Some(v.clone());
        cx.notify();
        Ok::<_, ViewError>(v)
    })?
    .map_err(|e| format!("input A view: {e:?}"))?;
    let other = update(cx, peer, move |host, window, cx| {
        let v = CefBrowser::new(peer_attach, window, cx)?;
        host.browser = Some(v.clone());
        cx.notify();
        Ok::<_, ViewError>(v)
    })?
    .map_err(|e| format!("input B view: {e:?}"))?;
    let mut native_keyboard = app_update(cx, |app| {
        NativeKeyboardServices::new(NonZeroUsize::new(2).ok_or("zero native routes")?, app)
            .map_err(|e| format!("input native keyboard service: {e:?}"))
    })??;
    update(cx, root, |_, window, app| {
        native_keyboard.attach(&entity, window, app)
    })?
    .map_err(|e| format!("input A native keyboard attach: {e:?}"))?;
    update(cx, peer, |_, window, app| {
        native_keyboard.attach(&other, window, app)
    })?
    .map_err(|e| format!("input B native keyboard attach: {e:?}"))?;
    let mut composition = UiComposition::new(NonZeroUsize::new(2).ok_or("zero routes")?);
    update(cx, root, |_, w, cx| composition.register(&entity, w, &*cx))?
        .map_err(|e| format!("input A register: {e:?}"))?;
    update(cx, peer, |_, w, cx| composition.register(&other, w, &*cx))?
        .map_err(|e| format!("input B register: {e:?}"))?;
    if composition.len() != 2 {
        return Err("input composition does not have two routes".into());
    }
    if options.scenario == HostScenario::ImeObserve {
        // The creation wait may have consumed an early shared wake before any
        // route existed. Consume once now; do not wait for another frame/wake
        // while its mailbox is still armed. Later turns retain the real-wake gate.
        let initial = app_update(cx, |app| composition.dispatch(app))?;
        if initial.attempted != 2 || initial.removed != 0 || !initial.failures.is_empty() {
            return Err(format!("IME initial dispatch failed: {initial:?}"));
        }
        atomic(
            &options
                .report
                .parent()
                .ok_or("IME initial route report missing")?
                .join("ime-observe-routes.json"),
            &object(vec![
                ("schema", "1".into()),
                ("kind", quote("ime-environment-initial-dispatch")),
                ("pid", std::process::id().to_string()),
                ("attempted", initial.attempted.to_string()),
                ("updated", initial.updated.to_string()),
                ("pending", initial.pending.to_string()),
                ("removed", initial.removed.to_string()),
                ("task_11_2_accepted", bool_json(false)),
            ]),
        )?;
    }
    let mut exec = Exec {
        cx,
        rt,
        receiver,
        stats,
        out,
        options,
        composition,
        deadline,
        pump_markers: [
            a.manual_pump_marker()?,
            b.as_ref().ok_or("missing peer")?.manual_pump_marker()?,
        ],
        native_keyboard,
        completed: false,
    };
    if options.scenario == HostScenario::ImeObserve {
        return ime_observe::run(
            &mut exec,
            root,
            peer,
            entity,
            other,
            handle,
            peer_handle,
            a,
            b.as_ref().ok_or("missing IME observation peer")?,
        )
        .await;
    }
    let mut prior: Option<InputSnapshot> = None;
    let mut prior_frame_sequence: Option<u64> = None;
    let mut prior_paint = None;
    for stage in 0..5 {
        let (len, focus, blur, down, keys, inputs, wheel) = match stage {
            0 => (0, 0, 0, 0, 0, 0, 0),
            1 => (1, 1, 0, 1, 1, 1, 0),
            2 => (2, 1, 0, 2, 2, 2, 1),
            3 => (2, 1, 1, 2, 2, 2, 1),
            _ => (3, 2, 1, 3, 3, 3, 1),
        };
        let until = (Instant::now() + WAIT).min(exec.deadline);
        let mut last = String::new();
        let mut last_observation: Option<String> = None;
        let (diag, source, title, snapshot) = loop {
            if Instant::now() >= until {
                let case = options
                    .report
                    .parent()
                    .ok_or("input report has no case directory")?;
                let diagnostic = object(vec![
                    ("schema", "1".into()),
                    ("stage", stage.to_string()),
                    ("kind", quote("input-timeout-observation")),
                    ("success", bool_json(false)),
                    (
                        "sample",
                        last_observation.clone().unwrap_or_else(|| "null".into()),
                    ),
                ]);
                let saved = atomic(
                    &case.join(format!("input-timeout-stage-{stage}.json")),
                    &diagnostic,
                );
                return Err(format!(
                    "Input stage {stage} real DOM/source/snapshot timeout: {last}; diagnostic_save={saved:?}"
                ));
            }
            rendered(exec.cx, root).await?;
            let diag = view(exec.cx, root, &entity)?;
            let source = exec
                .rt
                .frame_source(id)
                .map_err(|e| format!("input source: {e:?}"))?
                .snapshot();
            let nav = handle.snapshot();
            let snapshot = handle
                .input_snapshot()
                .map_err(|e| format!("typed native input snapshot: {e:?}"))?;
            if nav.state == SessionState::Closed
                || nav.state == SessionState::CreateFailed
                || source.closed
            {
                return Err("input Browser prematurely closed".into());
            }
            let title = nav.navigation.title;
            let title_ok = expected_title(&title, len, focus, blur, down, keys, inputs, wheel);
            let frame = source.view.frame.as_ref();
            let frame_ok = frame.is_some_and(|f| {
                source.view.visible
                    && diag.paint_calls > 0
                    && diag.painted_generation == f.info.surface_generation
                    && diag.painted_sequence == f.info.sequence
                    && diag.frame_width == f.info.width
                    && diag.frame_height == f.info.height
                    && diag.surface_bounds == (0., 48., 320., 240.)
                    && (!matches!(stage, 1 | 2 | 4)
                        || prior_frame_sequence.is_some_and(|old| f.info.sequence > old))
            });
            let native_ok = good(&snapshot)
                && diag.input.snapshot.as_ref() == Ok(&snapshot)
                && (!matches!(stage, 1 | 2 | 4)
                    || prior.as_ref().is_some_and(|old| {
                        snapshot.admitted > old.admitted && snapshot.executed > old.executed
                    }));
            let cleaned = stage != 3
                || prior.as_ref().is_some_and(|s| {
                    snapshot.epoch > s.epoch
                        && snapshot.pressed_buttons == 0
                        && snapshot.pressed_keys == 0
                });
            if title_ok.is_ok()
                && frame_ok
                && native_ok
                && cleaned
                && !nav.navigation.is_loading
                && diag.last_error.is_none()
                && !diag.input.pending
                && diag.input.rejected == 0
            {
                break (diag, source, title, snapshot);
            }
            last = format!(
                "title={:?}, frame={frame_ok}, native={native_ok}, cleanup={cleaned}, diag={:?}, title_error={:?}",
                title,
                diag.input,
                title_ok.err()
            );
            last_observation = Some(object(vec![
                ("pid", std::process::id().to_string()),
                ("dom_title", quote(&title)),
                ("input_snapshot", input_json(&snapshot)),
                ("component_callbacks", callbacks_json(&diag)),
                (
                    "native_keyboard",
                    native_keyboard_json(&exec.native_keyboard.diagnostics()),
                ),
                ("frame_matches", bool_json(frame_ok)),
                ("native_matches", bool_json(native_ok)),
                ("cleanup_matches", bool_json(cleaned)),
            ]));
            exec.step().await?;
            // Exactly one explicit retry per subsequent UI turn when the
            // component says a slot is pending; no artificial backpressure.
            if diag.input.pending {
                exec.retry_once().await?
            }
        };
        if stage == 0 && options.manual_foreground {
            let case = options
                .report
                .parent()
                .ok_or("missing manual setup directory")?;
            atomic(
                &case.join("input-manual-foreground.json"),
                &object(vec![
                    ("schema", "1".into()),
                    ("pid", std::process::id().to_string()),
                    ("window", facts_json(a.info()?)),
                    ("title", quote(TITLE)),
                    ("input_sent", bool_json(false)),
                    (
                        "instruction",
                        quote(
                            "Activate main test window from taskbar; no client click, ALT, input-mode or foreground-policy changes.",
                        ),
                    ),
                ]),
            )?;
            // Share the original stage deadline; manual setup does not add
            // time or relax the existing DOM/geometry/input/image gates.
            while !a.is_foreground()? {
                if Instant::now() >= until {
                    return Err(
                        "manual foreground setup not confirmed within original stage deadline"
                            .into(),
                    );
                }
                exec.step().await?;
            }
        }
        if stage == 0 || stage == 2 || stage == 3 {
            a.foreground()?
        }
        // Stage 2 starts on A. After saving the pre-blur screenshot the driver
        // asks the foreground owner process to request its own peer normally.
        // It still verifies actual peer foreground before sending the click. Both
        // probes are armed on every stage, including empty expected input:
        // neither the idle window nor release-only stages fabricate records.
        let expected = events(stage, if stage == 2 { b_scale } else { a_scale });
        a.arm(if stage == 2 { &[] } else { &expected })?;
        b.as_ref()
            .ok_or("peer observer missing")?
            .arm(if stage == 2 { &expected } else { &[] })?;
        let case = options
            .report
            .parent()
            .ok_or("input report has no case directory")?;
        let frame = source
            .view
            .frame
            .as_ref()
            .ok_or("input source VIEW missing")?;
        let raw = format!("input-stage-{stage}-view.bgra");
        std::fs::write(case.join(&raw), frame.pixels.as_slice()).map_err(|e| e.to_string())?;
        let meta = object(vec![
            ("schema", "1".into()),
            ("stage", stage.to_string()),
            ("pid", std::process::id().to_string()),
            ("title", quote(TITLE)),
            ("peer_title", quote(PEER_TITLE)),
            ("dom_title", quote(&title)),
            ("window", facts_json(a.info()?)),
            (
                "peer",
                facts_json(b.as_ref().ok_or("peer missing")?.info()?),
            ),
            ("scale_factor", format!("{a_scale:?}")),
            ("peer_scale", format!("{b_scale:?}")),
            ("view_bounds", format!("[0,{HEADER},320,240]")),
            (
                "browser",
                format!("[{},{}]", id.runtime().value(), id.local()),
            ),
            ("diagnostics", metrics_json(&diag)),
            ("input_snapshot", input_json(&snapshot)),
            ("component_callbacks", callbacks_json(&diag)),
            (
                "native_keyboard",
                native_keyboard_json(&exec.native_keyboard.diagnostics()),
            ),
            ("view_raw", raw_json(frame, &raw)),
            (
                "action",
                quote(match stage {
                    0 => "focus-a",
                    1 => "type-b-drag-wheel",
                    2 => "focus-peer",
                    3 => "focus-a-again",
                    _ => "none",
                }),
            ),
        ]);
        write_json(&case.join(format!("input-stage-{stage}.json")), meta)?;
        if stage == 0 {
            atomic(
                &options.gate.with_extension("ready"),
                &std::process::id().to_string(),
            )?
        }
        atomic(
            &options.gate.with_extension(format!("input-{stage}.ready")),
            &std::process::id().to_string(),
        )?;
        let action = async {
            exec.release(stage, a, b.as_ref(), root).await?;
            let observed_until = (Instant::now() + WAIT).min(exec.deadline);
            while !a.complete()? || !b.as_ref().ok_or("peer observer missing")?.complete()? {
                if Instant::now() >= observed_until {
                    return Err(format!("Input stage {stage} marked WM sequence timeout"));
                }
                exec.step().await?;
            }
            Ok::<_, String>(())
        }
        .await;
        let evidence = observed(
            case,
            stage,
            a,
            b.as_ref().ok_or("peer observer missing")?,
            action.as_ref().err().map(String::as_str),
        );
        action?;
        evidence?;
        a.finish()?;
        b.as_ref().ok_or("peer observer missing")?.finish()?;
        prior_frame_sequence = source.view.frame.as_ref().map(|f| f.info.sequence);
        prior = Some(snapshot);
        prior_paint = Some(diag);
    }
    // Stage 4 releases A; B must remain Alive through the real green-host
    // stage 5. After that observation explicitly release/drop B as well.
    let before = update(exec.cx, root, |host, _, _| host.renders)?;
    let released = update(exec.cx, root, |host, window, cx| {
        entity.update(cx, |view, cx| view.release_image(window, cx))?;
        host.browser = None;
        host.detached = true;
        cx.notify();
        Ok::<_, ViewError>(entity.read(cx).diagnostics())
    })?
    .map_err(|e| format!("input release: {e:?}"))?;
    exec.out.released_view = Some(released.clone());
    // `paint` is the last actually verified live frame; `released_view`
    // separately proves release. A post-release diagnostic has size zero.
    exec.out.paint = prior_paint;
    if released.created_images == 0 || released.created_images != released.released_images {
        return Err("input VIEW release unbalanced".into());
    }
    let weak = entity.downgrade();
    drop(entity);
    if !exec.composition.unregister(id) {
        return Err("released input A route missing".into());
    }
    rendered(exec.cx, root).await?;
    if weak.upgrade().is_some() {
        return Err("input view still strongly retained after GPUI frame".into());
    }
    let alive = update(exec.cx, root, |host, _, _| {
        host.browser.is_none() && host.detached && host.renders > before
    })?;
    if !alive
        || !app_update(exec.cx, |app| {
            app.windows().contains(&AnyWindowHandle::from(root))
        })?
    {
        return Err("green host did not remain alive".into());
    }
    exec.out.detached_alive = true;
    let stage = 5;
    let case = options
        .report
        .parent()
        .ok_or("input report has no case directory")?;
    write_json(
        &case.join("input-stage-5.json"),
        object(vec![
            ("schema", "1".into()),
            ("stage", "5".into()),
            ("pid", std::process::id().to_string()),
            ("title", quote(TITLE)),
            ("peer_title", quote(PEER_TITLE)),
            ("action", quote("none")),
            ("window", facts_json(a_fact)),
            ("peer", facts_json(b_fact)),
            ("scale_factor", format!("{a_scale:?}")),
            ("peer_scale", format!("{b_scale:?}")),
            ("view_bounds", format!("[0,{HEADER},320,240]")),
            ("view_raw", "null".into()),
            ("detached", bool_json(true)),
        ]),
    )?;
    a.arm(&[])?;
    b.as_ref().ok_or("peer observer missing")?.arm(&[])?;
    atomic(
        &options.gate.with_extension("input-5.ready"),
        &std::process::id().to_string(),
    )?;
    let release = exec.release(stage, a, b.as_ref(), root).await;
    let evidence = observed(
        case,
        stage,
        a,
        b.as_ref().ok_or("peer observer missing")?,
        release.as_ref().err().map(String::as_str),
    );
    release?;
    evidence?;
    a.finish()?;
    b.as_ref().ok_or("peer observer missing")?.finish()?;
    let until = Instant::now() + WAIT;
    while !options.gate.is_file() {
        if Instant::now() >= until {
            return Err("input observer final release timeout".into());
        }
        exec.step().await?
    }
    if peer_handle.snapshot().state != SessionState::Alive || exec.composition.len() != 1 {
        return Err(
            "input peer did not remain the sole live route through final validation".into(),
        );
    }
    let peer_released = update(exec.cx, peer, |host, window, cx| {
        other.update(cx, |view, cx| view.release_image(window, cx))?;
        host.browser = None;
        host.detached = true;
        cx.notify();
        Ok::<_, ViewError>(other.read(cx).diagnostics())
    })?
    .map_err(|e| format!("input peer release: {e:?}"))?;
    if peer_released.created_images == 0
        || peer_released.created_images != peer_released.released_images
    {
        return Err("input peer VIEW release unbalanced".into());
    }
    let weak_peer = other.downgrade();
    drop(other);
    if !exec.composition.unregister(peer_id) {
        return Err("released input peer route missing".into());
    }
    rendered(exec.cx, peer).await?;
    if weak_peer.upgrade().is_some() {
        return Err("input peer still strongly retained after GPUI frame".into());
    }
    if !exec.composition.is_empty() {
        return Err("input composition retained route after view release".into());
    }
    let closing_until = (Instant::now() + WAIT).min(exec.deadline);
    while ![handle.snapshot().state, peer_handle.snapshot().state]
        .iter()
        .all(|state| matches!(state, SessionState::Closing | SessionState::Closed))
    {
        if Instant::now() >= closing_until {
            return Err("released input view never requested native close".into());
        }
        exec.step().await?;
    }
    exec.out.view_release_observed_closing = true;
    let last_live = prior.as_ref().ok_or("no last live native input snapshot")?;
    if !good(last_live) {
        return Err("last confirmed native input state was not clean".into());
    }
    let retired = exec
        .native_keyboard
        .retire()
        .map_err(|error| format!("native keyboard retirement unconfirmed: {error:?}"))?;
    if !retired.retired || retired.hook_live || retired.lease_live || retired.properties_live != 0 {
        return Err(format!(
            "native keyboard retirement incomplete: {retired:?}"
        ));
    }
    exec.out.input_detail = Some(object(vec![
        ("success", bool_json(true)),
        ("stages", STAGES.to_string()),
        (
            "manual_foreground_setup",
            bool_json(options.manual_foreground),
        ),
        ("fixture_length", FIXED.len().to_string()),
        ("fixture_digest", fnv(FIXED).to_string()),
        ("peer_release", metrics_json(&peer_released)),
        ("peer_entity_released", bool_json(true)),
        ("peer_release_observed_closing", bool_json(true)),
        ("last_live_native", input_json(last_live)),
        ("native_keyboard", native_keyboard_json(&retired)),
    ]));
    exec.completed = true;
    Ok(())
}
