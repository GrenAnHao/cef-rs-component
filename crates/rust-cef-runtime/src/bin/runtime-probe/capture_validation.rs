//! Fixed Native frame validation driver for task 9.3.
//!
//! The parent's dedicated `NativeScenario::Frames` process creates exactly one
//! Browser through `Runtime::probe_create_capture_browser`; only then does it
//! call [`run`]. This module owns no window, no DLL and no GPUI/desktop surface.
//! Every pixel it saves is an actual CEF offscreen VIEW callback frame taken
//! from the same `Runtime::frame_source` instance the host observes, already
//! charged to that Browser's CPU frame budget. PNGs derived later by
//! `scripts/compare-component-frames.py` are reference images of that real
//! capture output, never desktop or GPUI presentation screenshots.
//!
//! `DropView`/`RejectIncompleteAndHold` are test-support injections into the
//! real capture protocol. They are reported as injections, not as natural
//! Chromium letterboxing, device loss or frame dropping.
//!
//! The legacy twelve-byte fingerprint keeps the exact original algorithm
//! (`[0, len/3, len/2, len-4]`, three bytes each) through
//! `rust_cef_core::frame_store::fingerprint_bytes`; it is compatibility
//! evidence only and never replaces the full BGRA/alpha/size assertions.

use rust_cef_core::frame_store::fingerprint_bytes;
use rust_cef_runtime::{
    contracts::{BrowserId, ThreadMode, Viewport},
    ports::{BrowserCommand, BrowserHandle, FrameSource, OwnedFrame, PixelFormat, SurfaceKind},
    runtime::{FrameRecoveryDiagnostics, Runtime},
    test_support::{FrameProbeControl, FrameProbeState},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// Premultiplied BGRA of the fixed four-quadrant, text-free, animation-free
/// fixture. TL/TR are opaque, BL is 50% alpha premultiplied, BR transparent.
const TL: [u8; 4] = [60, 60, 220, 255];
const TR: [u8; 4] = [60, 200, 60, 255];
const BL: [u8; 4] = [100, 40, 20, 128];
const BR: [u8; 4] = [0, 0, 0, 0];

const TOTAL_LIMIT: Duration = Duration::from_secs(40);
const SEGMENT_LIMIT: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_millis(3);
const HIDDEN_OBSERVATION: Duration = Duration::from_millis(250);
const MIN_SPACING_US: u64 = 100_000;
const MAX_ATTEMPTS: usize = 8;
const EXPECTED_STAGE_IDS: [&str; 8] = [
    "initial",
    "resize-352x264",
    "resize-480x360",
    "shrink-20x20",
    "grow-600x420",
    "final-320x240",
    "exhaustion-recovery",
    "incomplete-recovery",
];
const CHECK_IDS: [&str; 21] = [
    "initial_frame_valid_320x240",
    "resize_352x264_recovered",
    "resize_480x360_recovered",
    "shrink_20x20_recovered",
    "grow_600x420_recovered",
    "final_320x240_recovered",
    "exhaustion_attempts_8",
    "exhaustion_request_spacing_ge_100ms",
    "exhaustion_native_invalidate_8",
    "exhaustion_requests_total_plus_8",
    "exhaustion_acks_unchanged",
    "exhaustion_old_good_frame_retained",
    "exhaustion_no_new_generation_frame",
    "hidden_no_requests_250ms",
    "reattach_recovered_new_generation",
    "incomplete_rejected_once",
    "incomplete_old_good_frame_retained",
    "incomplete_target_pending",
    "incomplete_acks_unchanged",
    "incomplete_recovered_new_generation",
    "samples_alpha_non_black_all_stages",
];

/// Fixed data URI: percent-encoded HTML/CSS only, no script, no text, no
/// animation. Body/html are transparent with zero margin and hidden overflow;
/// four absolutely positioned 50% divs cover the four quadrants exactly.
pub(super) fn fixture_url() -> String {
    const HTML: &str = concat!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><style>",
        "html,body{margin:0;padding:0;width:100%;height:100%;",
        "background:transparent;overflow:hidden}",
        "div{position:absolute;width:50%;height:50%}",
        "#tl{left:0;top:0;background:rgb(220,60,60)}",
        "#tr{left:50%;top:0;background:rgb(60,200,60)}",
        "#bl{left:0;top:50%;background:rgba(40,80,200,0.5)}",
        "#br{left:50%;top:50%;background:transparent}",
        "</style></head><body>",
        "<div id=\"tl\"></div><div id=\"tr\"></div>",
        "<div id=\"bl\"></div><div id=\"br\"></div>",
        "</body></html>"
    );
    format!("data:text/html,{}", percent_encode(HTML))
}

/// Completion-safe `data:` encoder: only RFC 3986 unreserved characters stay
/// literal. The fixture has no non-ASCII input and no escaping ambiguity.
fn percent_encode(input: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(input.len() * 3 / 2);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

fn expected_color(x: u32, y: u32, width: u32, height: u32) -> [u8; 4] {
    match (x * 2 < width, y * 2 < height) {
        (true, true) => TL,
        (false, true) => TR,
        (true, false) => BL,
        (false, false) => BR,
    }
}

fn quadrant(x: u32, y: u32, width: u32, height: u32) -> &'static str {
    match (x * 2 < width, y * 2 < height) {
        (true, true) => "TL",
        (false, true) => "TR",
        (true, false) => "BL",
        (false, false) => "BR",
    }
}

/// Strict validity of one actual VIEW frame: premultiplied BGRA, even selected
/// dimensions, complete length, and every checked pixel exactly equal to the
/// fixed fixture (including alpha), with both opaque regions and the
/// semitransparent region non-black. Every fixed stage has even dimensions
/// and exact half-size quadrants: no border/seam pixel is exempted.
pub(super) fn valid_frame(frame: &OwnedFrame) -> bool {
    let info = frame.info;
    if info.format != PixelFormat::PremultipliedBgra8 {
        return false;
    }
    let (width, height) = (info.width, info.height);
    if width < 4 || height < 4 || width % 2 != 0 || height % 2 != 0 {
        return false;
    }
    let Some(expected_len) = (width as usize)
        .checked_mul(height as usize)
        .and_then(|len| len.checked_mul(4))
    else {
        return false;
    };
    let Some(pixels) = frame.cpu_pixels() else {
        return false;
    };
    if pixels.len() != expected_len {
        return false;
    }
    let bgra = pixels.as_slice();
    let stride = width as usize * 4;
    for y in 0..height {
        for x in 0..width {
            let offset = y as usize * stride + x as usize * 4;
            let expected = expected_color(x, y, width, height);
            if bgra.get(offset..offset + 4) != Some(expected.as_slice()) {
                return false;
            }
        }
    }
    samples_match(bgra, width, height)
}

fn sample_positions(width: u32, height: u32) -> impl Iterator<Item = (u32, u32)> {
    const EIGHTHS: [u32; 4] = [1, 3, 5, 7];
    let grid = EIGHTHS.into_iter().flat_map(move |ny| {
        EIGHTHS.into_iter().map(move |nx| {
            let x = (width as u64 * nx as u64 / 8) as u32;
            let y = (height as u64 * ny as u64 / 8) as u32;
            (x.min(width - 1), y.min(height - 1))
        })
    });
    // Four extra interior points two pixels off the quadrant seam: the strict
    // scan tolerates one seam pixel, these prove the seam is not a hole.
    let mid_x = width / 2;
    let mid_y = height / 2;
    let seam = [
        (mid_x.saturating_sub(3), height / 8),
        ((mid_x + 2).min(width - 1), height / 8),
        (width / 8, mid_y.saturating_sub(3)),
        (width / 8, (mid_y + 2).min(height - 1)),
    ];
    grid.chain(seam)
}

fn samples_match(bgra: &[u8], width: u32, height: u32) -> bool {
    for (x, y) in sample_positions(width, height) {
        let offset = (y as usize * width as usize + x as usize) * 4;
        let expected = expected_color(x, y, width, height);
        if bgra.get(offset..offset + 4) != Some(expected.as_slice()) {
            return false;
        }
        if expected != BR && bgra[offset] == 0 && bgra[offset + 1] == 0 && bgra[offset + 2] == 0 {
            return false;
        }
    }
    true
}

fn sample_points(frame: &OwnedFrame) -> Vec<Value> {
    let (width, height) = (frame.info.width, frame.info.height);
    let bgra = frame.cpu_pixels().map_or(&[][..], |px| px.as_slice());
    sample_positions(width, height)
        .map(|(x, y)| {
            let offset = (y as usize * width as usize + x as usize) * 4;
            let expected = expected_color(x, y, width, height);
            let actual = bgra.get(offset..offset + 4).unwrap_or(&[0; 4]);
            json!({
                "x": x,
                "y": y,
                "quadrant": quadrant(x, y, width, height),
                "actual": [actual[0], actual[1], actual[2], actual[3]],
                "expected": expected,
                "passed": actual == expected.as_slice(),
            })
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex(&digest[..])
}

struct Ctx {
    browser: BrowserId,
    source: Arc<dyn FrameSource>,
    directory: std::path::PathBuf,
    deadline: Instant,
}

#[derive(Clone, Debug)]
struct SavedFrame {
    sequence: u64,
    generation: u64,
    width: u32,
    height: u32,
    sha256: String,
    fingerprint: [u8; 12],
}

impl SavedFrame {
    fn json(&self) -> Value {
        json!({
            "sequence": self.sequence,
            "generation": self.generation,
            "width": self.width,
            "height": self.height,
            "sha256": self.sha256,
            "fingerprint": self.fingerprint,
        })
    }
}

fn snapshot_frame(source: &Arc<dyn FrameSource>) -> Result<SavedFrame, String> {
    let snapshot = source.snapshot();
    let frame = snapshot
        .view
        .frame
        .as_ref()
        .ok_or_else(|| "no retained VIEW frame".to_string())?;
    let bytes = frame
        .cpu_pixels()
        .ok_or_else(|| "retained VIEW frame is not CPU pixels".to_string())?
        .as_slice();
    Ok(SavedFrame {
        sequence: frame.info.sequence,
        generation: frame.info.surface_generation,
        width: frame.info.width,
        height: frame.info.height,
        sha256: sha256_hex(bytes),
        fingerprint: fingerprint_bytes(bytes)
            .ok_or_else(|| "legacy fingerprint unavailable".to_string())?,
    })
}

fn probe_state(runtime: &Runtime, browser: BrowserId) -> Result<FrameProbeState, String> {
    runtime
        .probe_frame_state(browser)
        .ok_or_else(|| "frame probe state unavailable".to_string())
}

fn recovery_diagnostics(
    runtime: &Runtime,
    browser: BrowserId,
) -> Result<FrameRecoveryDiagnostics, String> {
    runtime
        .frame_recovery_diagnostics(browser)
        .map_err(|error| format!("recovery diagnostics: {error:?}"))
}

fn status(runtime: &Runtime, browser: BrowserId) -> Result<String, String> {
    Ok(recovery_diagnostics(runtime, browser)?.status.to_string())
}

fn diagnostics_json(diagnostics: &FrameRecoveryDiagnostics) -> Value {
    json!({
        "status": diagnostics.status.to_string(),
        "generation": diagnostics.generation,
        "expected_width": diagnostics.expected_width,
        "expected_height": diagnostics.expected_height,
        "attempts": diagnostics.attempts,
        "requests_total": diagnostics.requests_total,
        "acks": diagnostics.acks,
        "exhaustions": diagnostics.exhaustions,
        "post_failures": diagnostics.post_failures,
        "queued": diagnostics.queued,
        "request_offsets_us": diagnostics.request_offsets_us.clone(),
        "native_invalidate_entries": diagnostics.native_invalidate_entries,
        "native_thread": diagnostics.native_thread,
        "native_offsets_us": diagnostics.native_offsets_us.clone(),
        "min_spacing_us": diagnostics.min_spacing_us,
        "probe_dropped": diagnostics.probe_dropped,
        "incomplete_rejected": diagnostics.incomplete_rejected,
    })
}

fn tick(runtime: &mut Runtime, mode: ThreadMode) -> Result<(), String> {
    // Integrated: the only pump is the existing Runtime::pump. Threaded owns a
    // CEF UI thread and must never call pump here.
    if mode == ThreadMode::Integrated {
        runtime.pump().map_err(|error| format!("pump: {error:?}"))?;
    }
    thread::sleep(TICK);
    Ok(())
}

fn save_abnormal(source: &Arc<dyn FrameSource>, directory: &Path, label: &str) -> Option<String> {
    let snapshot = source.snapshot();
    let frame = snapshot.view.frame.as_ref()?;
    let info = frame.info;
    let path = directory.join(format!(
        "frame-{label}-abnormal-seq{}-gen{}-{}x{}.bgra",
        info.sequence, info.surface_generation, info.width, info.height
    ));
    std::fs::write(&path, frame.cpu_pixels()?.as_slice()).ok()?;
    Some(path.display().to_string())
}

fn timeout_error(runtime: &Runtime, ctx: &Ctx, label: &str) -> String {
    let diagnostics = runtime
        .frame_recovery_diagnostics(ctx.browser)
        .map(|value| format!("{value:?}"))
        .unwrap_or_else(|error| format!("recovery diagnostics: {error:?}"));
    let snapshot = ctx.source.snapshot();
    let view = &snapshot.view;
    let frame = view
        .frame
        .as_ref()
        .map(|frame| {
            format!(
                "gen={} seq={} {}x{} valid={}",
                frame.info.surface_generation,
                frame.info.sequence,
                frame.info.width,
                frame.info.height,
                valid_frame(frame)
            )
        })
        .unwrap_or_else(|| "<none>".to_string());
    let artifact = save_abnormal(&ctx.source, &ctx.directory, label)
        .unwrap_or_else(|| "<unavailable>".to_string());
    format!(
        "{label}: deadline; source view generation={} visible={} frame={frame}; recovery={diagnostics}; abnormal_artifact={artifact}",
        view.generation, view.visible
    )
}

fn wait<F>(
    runtime: &mut Runtime,
    mode: ThreadMode,
    ctx: &Ctx,
    label: &str,
    mut predicate: F,
) -> Result<(), String>
where
    F: FnMut(&Runtime, &Ctx) -> Result<bool, String>,
{
    let segment_deadline = Instant::now() + SEGMENT_LIMIT;
    loop {
        if predicate(runtime, ctx).map_err(|error| format!("{label}: {error}"))? {
            return Ok(());
        }
        let now = Instant::now();
        if now >= segment_deadline || now >= ctx.deadline {
            return Err(timeout_error(runtime, ctx, label));
        }
        tick(runtime, mode)?;
    }
}

fn submit(handle: &BrowserHandle, command: BrowserCommand, label: &str) -> Result<(), String> {
    handle
        .submit(command)
        .map(|_| ())
        .map_err(|error| format!("{label}: submit rejected: {error:?}"))
}

fn capture_stage(
    runtime: &Runtime,
    ctx: &Ctx,
    id: &str,
    expected: (u32, u32),
    viewport: Viewport,
) -> Result<Value, String> {
    let snapshot = ctx.source.snapshot();
    let view = &snapshot.view;
    if !view.visible {
        return Err(format!("{id}: VIEW hidden while saving stage"));
    }
    let frame = view
        .frame
        .as_ref()
        .ok_or_else(|| format!("{id}: no retained VIEW frame at capture"))?;
    let info = frame.info;
    if info.surface != SurfaceKind::View {
        return Err(format!("{id}: frame surface is not VIEW"));
    }
    if info.surface_generation != view.generation {
        return Err(format!(
            "{id}: frame generation {} != current VIEW generation {}",
            info.surface_generation, view.generation
        ));
    }
    if (info.width, info.height) != expected {
        return Err(format!(
            "{id}: frame {}x{} does not match target {}x{}",
            info.width, info.height, expected.0, expected.1
        ));
    }
    if !valid_frame(frame) {
        return Err(format!(
            "{id}: BGRA/alpha/size fixture mismatch at {}x{}",
            info.width, info.height
        ));
    }
    let bytes = frame
        .cpu_pixels()
        .ok_or_else(|| "retained VIEW frame is not CPU pixels".to_string())?
        .as_slice();
    let fingerprint =
        fingerprint_bytes(bytes).ok_or_else(|| format!("{id}: legacy fingerprint unavailable"))?;
    let sha256 = sha256_hex(bytes);
    let file = ctx.directory.join(format!("frame-{id}.bgra"));
    // Borrow the immutable budgeted slice directly: no extra pixel Vec and no
    // per-stage `pixels` reference is retained past this function.
    std::fs::write(&file, bytes)
        .map_err(|error| format!("{id}: raw BGRA write {}: {error}", file.display()))?;
    let samples = sample_points(frame);
    let diagnostics = diagnostics_json(&recovery_diagnostics(runtime, ctx.browser)?);
    Ok(json!({
        "id": id,
        "viewport": {
            "width": viewport.width,
            "height": viewport.height,
            "scale": (viewport.scale as f64 * 1000.0).round() / 1000.0,
        },
        "target": {"width": expected.0, "height": expected.1},
        "generation": info.surface_generation,
        "sequence": info.sequence,
        "width": info.width,
        "height": info.height,
        "bytes": bytes.len(),
        "fingerprint": fingerprint,
        "fingerprint_hex": hex(&fingerprint),
        "sha256": sha256,
        "raw_bgra": file.display().to_string(),
        "samples": samples,
        "recovery": diagnostics,
    }))
}

fn stage_valid(stage: &Value, width: u32, height: u32) -> bool {
    stage["width"].as_u64() == Some(width as u64)
        && stage["height"].as_u64() == Some(height as u64)
        && stage["bytes"].as_u64() == Some(width as u64 * height as u64 * 4)
        && stage["fingerprint"]
            .as_array()
            .is_some_and(|value| value.len() == 12)
        && stage["samples"].as_array().is_some_and(|samples| {
            !samples.is_empty() && samples.iter().all(|sample| sample["passed"] == true)
        })
}

fn stage_recovered(stage: &Value, width: u32, height: u32) -> bool {
    stage_valid(stage, width, height) && stage["recovery"]["status"].as_str() == Some("Recovered")
}

fn observed_backend(runtime: &Runtime, browser: BrowserId) -> Option<String> {
    runtime
        .frame_capture_diagnostics(browser)
        .ok()
        .and_then(|diagnostics| diagnostics.observed_backend)
        .map(str::to_string)
}

/// Arms the parent's one-shot allowance for one controlled ResizeExhausted.
/// The observer consumes it atomically; it never whitelists IncompleteFrame.
/// Real metadata refusals need separate strict witness accounting. Drop also
/// disarms the allowance on every early-return path.
struct FaultWindow<'a>(&'a AtomicBool);

impl<'a> FaultWindow<'a> {
    fn new(flag: &'a AtomicBool) -> Self {
        flag.store(true, Ordering::Release);
        Self(flag)
    }
}

impl Drop for FaultWindow<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct Report {
    mode: &'static str,
    backend: Option<String>,
    stages: Vec<Value>,
    checks: Vec<Value>,
}

impl Report {
    fn new(mode: ThreadMode) -> Self {
        Self {
            mode: match mode {
                ThreadMode::Integrated => "Integrated",
                ThreadMode::Threaded => "Threaded",
            },
            backend: None,
            stages: Vec::new(),
            checks: Vec::new(),
        }
    }

    fn stage(&mut self, stage: Value) {
        self.stages.push(stage);
    }

    fn check(&mut self, id: &'static str, passed: bool, evidence: Value) {
        self.checks
            .push(json!({"id": id, "passed": passed, "evidence": evidence}));
    }

    fn all_passed(&self) -> bool {
        self.stages.len() == EXPECTED_STAGE_IDS.len()
            && self
                .stages
                .iter()
                .zip(EXPECTED_STAGE_IDS)
                .all(|(stage, id)| stage["id"].as_str() == Some(id) && stage["samples"].is_array())
            && self.checks.len() == CHECK_IDS.len()
            && self.checks.iter().zip(CHECK_IDS).all(|(check, id)| {
                check["id"].as_str() == Some(id) && check["passed"].as_bool() == Some(true)
            })
    }

    fn failed_ids(&self) -> Vec<String> {
        let mut failed = Vec::new();
        for (index, id) in CHECK_IDS.iter().enumerate() {
            let passed = self
                .checks
                .get(index)
                .is_some_and(|check| check["id"].as_str() == Some(*id) && check["passed"] == true);
            if !passed {
                failed.push((*id).to_string());
            }
        }
        if self.stages.len() != EXPECTED_STAGE_IDS.len() {
            failed.push(format!(
                "stages({}/{})",
                self.stages.len(),
                EXPECTED_STAGE_IDS.len()
            ));
        }
        failed
    }

    fn value(&self, success: bool, error: Option<&str>) -> Value {
        json!({
            "schema": 1,
            "kind": "capture-validation",
            "success": success,
            "error": error,
            "mode": self.mode,
            "backend": self.backend,
            "fixture": fixture_url(),
            "source": "same Runtime::frame_source instance; actual CEF offscreen VIEW callback premultiplied BGRA charged to the Browser frame budget",
            "boundary": {
                "raw_frames": "actual CEF OSR capture frames; PNGs derived by scripts/compare-component-frames.py are reference images of these captures, not desktop or GPUI window screenshots",
                "injection": "DropView and RejectIncompleteAndHold are test-support protocol injections, not natural Chromium letterboxing, device loss or frame dropping",
                "dpi": "same-size DPI changes advance the local mailbox generation; this is not a native resize epoch association",
            },
            "stages_expected": EXPECTED_STAGE_IDS.len(),
            "stages": self.stages,
            "checks": self.checks,
        })
    }
}

fn write_failure(
    runtime: &Runtime,
    browser: BrowserId,
    directory: &Path,
    report: &Report,
    error: &str,
) -> String {
    let mut value = report.value(false, Some(error));
    if let Ok(diagnostics) = runtime.frame_recovery_diagnostics(browser) {
        value["failure_recovery"] = diagnostics_json(&diagnostics);
    }
    if let Ok(source) = runtime.frame_source(browser) {
        if let Some(artifact) = save_abnormal(&source, directory, "failure") {
            value["failure_artifact"] = Value::String(artifact);
        }
    }
    let path = directory.join("capture-validation.failure.json");
    if let Ok(bytes) = serde_json::to_vec_pretty(&value) {
        let _ = std::fs::write(&path, bytes);
    }
    format!("{error} (failure evidence: {})", path.display())
}

fn wait_initial(runtime: &mut Runtime, mode: ThreadMode, ctx: &Ctx) -> Result<(), String> {
    wait(runtime, mode, ctx, "initial_frame", |runtime, ctx| {
        let snapshot = ctx.source.snapshot();
        let view = &snapshot.view;
        let frame_ok = view.visible
            && view.frame.as_ref().is_some_and(|frame| {
                frame.info.surface == SurfaceKind::View
                    && frame.info.surface_generation == view.generation
                    && frame.info.width == 320
                    && frame.info.height == 240
                    && valid_frame(frame)
            });
        if !frame_ok {
            return Ok(false);
        }
        let status = status(runtime, ctx.browser)?;
        Ok(status == "Idle" || status == "Recovered")
    })
}

fn resize_stage(
    runtime: &mut Runtime,
    mode: ThreadMode,
    ctx: &Ctx,
    handle: &BrowserHandle,
    id: &str,
    viewport: Viewport,
    expected: (u32, u32),
) -> Result<Value, String> {
    let before = ctx.source.snapshot().view.generation;
    submit(handle, BrowserCommand::SetViewport(viewport), id)?;
    wait(runtime, mode, ctx, id, |runtime, ctx| {
        let snapshot = ctx.source.snapshot();
        let view = &snapshot.view;
        if view.generation <= before {
            return Ok(false);
        }
        let frame_ok = view.visible
            && view.frame.as_ref().is_some_and(|frame| {
                frame.info.surface == SurfaceKind::View
                    && frame.info.surface_generation == view.generation
                    && frame.info.width == expected.0
                    && frame.info.height == expected.1
                    && valid_frame(frame)
            });
        if !frame_ok {
            return Ok(false);
        }
        Ok(status(runtime, ctx.browser)? == "Recovered")
    })?;
    capture_stage(runtime, ctx, id, expected, viewport)
}

fn wait_probe_applied(
    runtime: &mut Runtime,
    mode: ThreadMode,
    ctx: &Ctx,
    label: &str,
    applied: u64,
    control: FrameProbeControl,
    assert: fn(&FrameProbeState) -> bool,
) -> Result<(), String> {
    runtime
        .probe_frame_control(ctx.browser, control.clone())
        .map_err(|error| format!("{label}: control rejected: {error:?}"))?;
    wait(runtime, mode, ctx, label, |runtime, ctx| {
        let state = probe_state(runtime, ctx.browser)?;
        Ok(state.applied > applied && assert(&state))
    })
}

fn execute(
    runtime: &mut Runtime,
    mode: ThreadMode,
    handle: &BrowserHandle,
    ctx: &Ctx,
    report: &mut Report,
    expected_fault: &AtomicBool,
) -> Result<(), String> {
    let browser = ctx.browser;

    // ---- initial 320x240@1 ----
    wait_initial(runtime, mode, ctx)?;
    let stage = capture_stage(
        runtime,
        ctx,
        "initial",
        (320, 240),
        Viewport {
            width: 320,
            height: 240,
            scale: 1.0,
        },
    )?;
    let initial_status = stage["recovery"]["status"].as_str().unwrap_or("");
    report.check(
        CHECK_IDS[0],
        stage_valid(&stage, 320, 240)
            && (initial_status == "Idle" || initial_status == "Recovered"),
        json!({
            "stage": "initial",
            "generation": stage["generation"],
            "sequence": stage["sequence"],
            "recovered_status": initial_status,
        }),
    );
    report.stage(stage);

    // ---- real resize/final stages; each waits for a new Source generation,
    // same-generation complete frame at the exact pixel target and Recovered.
    const STAGES: [(&str, (i32, i32, f32), (u32, u32)); 5] = [
        ("resize-352x264", (320, 240, 1.1), (352, 264)),
        ("resize-480x360", (320, 240, 1.5), (480, 360)),
        ("shrink-20x20", (20, 20, 1.0), (20, 20)),
        ("grow-600x420", (400, 280, 1.5), (600, 420)),
        ("final-320x240", (320, 240, 1.0), (320, 240)),
    ];
    for (index, (id, (width, height, scale), expected)) in STAGES.iter().enumerate() {
        let viewport = Viewport {
            width: *width,
            height: *height,
            scale: *scale,
        };
        let stage = resize_stage(runtime, mode, ctx, handle, *id, viewport, *expected)?;
        report.check(
            CHECK_IDS[index + 1],
            stage_recovered(&stage, expected.0, expected.1),
            json!({
                "stage": id,
                "generation": stage["generation"],
                "sequence": stage["sequence"],
                "width": stage["width"],
                "height": stage["height"],
                "recovered_status": stage["recovery"]["status"],
            }),
        );
        report.stage(stage);
    }

    // ---- controlled exhaustion: DropView is an explicit injection ----
    let before = recovery_diagnostics(runtime, browser)?;
    let old = snapshot_frame(&ctx.source)?;
    let state_before = probe_state(runtime, browser)?;
    wait_probe_applied(
        runtime,
        mode,
        ctx,
        "arm_drop_view",
        state_before.applied,
        FrameProbeControl::DropView(true),
        |state| state.drop_view && !state.incomplete_pending,
    )?;
    let fault = FaultWindow::new(expected_fault);
    submit(
        handle,
        BrowserCommand::SetViewport(Viewport {
            width: 360,
            height: 240,
            scale: 1.1,
        }),
        "exhaustion_resize",
    )?;
    wait(runtime, mode, ctx, "exhaustion", |runtime, ctx| {
        Ok(status(runtime, ctx.browser)? == "Exhausted" && !expected_fault.load(Ordering::Acquire))
    })?;
    let exhausted = recovery_diagnostics(runtime, browser)?;
    let offsets = &exhausted.request_offsets_us;
    let gaps: Vec<u64> = offsets
        .windows(2)
        .map(|pair| pair[1].saturating_sub(pair[0]))
        .collect();
    let native_offsets = &exhausted.native_offsets_us;
    let native_gaps: Vec<u64> = native_offsets
        .windows(2)
        .map(|pair| pair[1].saturating_sub(pair[0]))
        .collect();
    let ui_thread = runtime
        .diagnostics()
        .cef_ui_thread_id
        .or(runtime.diagnostics().context_thread_id);
    report.check(
        CHECK_IDS[6],
        exhausted.status.to_string() == "Exhausted" && exhausted.attempts as usize == MAX_ATTEMPTS,
        json!({
            "status": exhausted.status,
            "attempts": exhausted.attempts,
            "exhaustions": exhausted.exhaustions,
            "probe_dropped": exhausted.probe_dropped,
        }),
    );
    report.check(
        CHECK_IDS[7],
        offsets.len() == MAX_ATTEMPTS
            && gaps.iter().all(|gap| *gap >= MIN_SPACING_US)
            && exhausted
                .min_spacing_us
                .is_some_and(|value| value >= MIN_SPACING_US),
        json!({
            "request_offsets_us": offsets,
            "gaps_us": gaps,
            "min_spacing_us": exhausted.min_spacing_us,
        }),
    );
    report.check(
        CHECK_IDS[8],
        exhausted.native_invalidate_entries
            == before.native_invalidate_entries + MAX_ATTEMPTS as u64
            && native_offsets.len() == MAX_ATTEMPTS
            && native_gaps.iter().all(|gap| *gap >= MIN_SPACING_US)
            && ui_thread.is_some_and(|id| id != 0 && exhausted.native_thread == id),
        json!({
            "native_invalidate_entries_before": before.native_invalidate_entries,
            "native_invalidate_entries_after": exhausted.native_invalidate_entries,
            "native_offsets_us": native_offsets,
            "native_gaps_us": native_gaps,
            "native_thread": exhausted.native_thread,
            "cef_ui_thread_id": ui_thread,
        }),
    );
    report.check(
        CHECK_IDS[9],
        exhausted.requests_total == before.requests_total + MAX_ATTEMPTS as u64,
        json!({
            "requests_before": before.requests_total,
            "requests_after": exhausted.requests_total,
        }),
    );
    report.check(
        CHECK_IDS[10],
        exhausted.acks == before.acks,
        json!({"acks_before": before.acks, "acks_after": exhausted.acks}),
    );
    let snapshot = ctx.source.snapshot();
    let retained = snapshot.view.frame.as_ref().is_some_and(|frame| {
        frame.info.sequence == old.sequence && frame.info.surface_generation == old.generation
    });
    report.check(
        CHECK_IDS[11],
        retained,
        json!({
            "expected": old.json(),
            "retained": snapshot.view.frame.as_ref().map(|frame| json!({
                "sequence": frame.info.sequence,
                "generation": frame.info.surface_generation,
                "width": frame.info.width,
                "height": frame.info.height,
            })),
        }),
    );
    report.check(
        CHECK_IDS[12],
        snapshot.view.generation > old.generation
            && snapshot
                .view
                .frame
                .as_ref()
                .is_some_and(|frame| frame.info.surface_generation != snapshot.view.generation),
        json!({
            "current_view_generation": snapshot.view.generation,
            "retained_frame_generation": snapshot.view.frame.as_ref().map(|frame| frame.info.surface_generation),
            "old_generation": old.generation,
        }),
    );

    // ---- hidden: no further requests while Hidden, then reattach ----
    submit(handle, BrowserCommand::SetVisible(false), "hide")?;
    wait(runtime, mode, ctx, "hidden", |runtime, ctx| {
        let snapshot = ctx.source.snapshot();
        Ok(!snapshot.view.visible && status(runtime, ctx.browser)? == "Hidden")
    })?;
    let hidden = recovery_diagnostics(runtime, browser)?;
    let observe_until = Instant::now() + HIDDEN_OBSERVATION;
    while Instant::now() < observe_until {
        if Instant::now() >= ctx.deadline {
            return Err(timeout_error(runtime, ctx, "hidden_observation"));
        }
        tick(runtime, mode)?;
    }
    let after_hidden = recovery_diagnostics(runtime, browser)?;
    report.check(
        CHECK_IDS[13],
        after_hidden.requests_total == hidden.requests_total
            && after_hidden.native_invalidate_entries == hidden.native_invalidate_entries
            && after_hidden.status.to_string() == "Hidden",
        json!({
            "requests_at_hidden": hidden.requests_total,
            "requests_after_250ms": after_hidden.requests_total,
            "native_invalidates_at_hidden": hidden.native_invalidate_entries,
            "native_invalidates_after_250ms": after_hidden.native_invalidate_entries,
            "status": after_hidden.status,
            "observation_ms": HIDDEN_OBSERVATION.as_millis() as u64,
        }),
    );

    let hidden_generation = ctx.source.snapshot().view.generation;
    submit(handle, BrowserCommand::SetVisible(true), "show")?;
    let state_before_release = probe_state(runtime, browser)?;
    wait_probe_applied(
        runtime,
        mode,
        ctx,
        "release_drop_view",
        state_before_release.applied,
        FrameProbeControl::DropView(false),
        |state| !state.drop_view && !state.incomplete_pending,
    )?;
    wait(runtime, mode, ctx, "exhaustion-recovery", |runtime, ctx| {
        let snapshot = ctx.source.snapshot();
        let view = &snapshot.view;
        if view.generation <= hidden_generation || !view.visible {
            return Ok(false);
        }
        let frame_ok = view.frame.as_ref().is_some_and(|frame| {
            frame.info.surface == SurfaceKind::View
                && frame.info.surface_generation == view.generation
                && frame.info.width == 396
                && frame.info.height == 264
                && valid_frame(frame)
        });
        if !frame_ok {
            return Ok(false);
        }
        Ok(status(runtime, ctx.browser)? == "Recovered")
    })?;
    // Arm on the CEF UI before sampling the old frame. A valid duplicate of
    // the old generation can no longer replace it or consume the one-shot.
    let recovered_generation = ctx.source.snapshot().view.generation;
    let target_generation = recovered_generation
        .checked_add(1)
        .ok_or("incomplete target generation overflow")?;
    let state_before = probe_state(runtime, browser)?;
    wait_probe_applied(
        runtime,
        mode,
        ctx,
        "arm_reject_incomplete",
        state_before.applied,
        FrameProbeControl::RejectIncompleteAndHold {
            generation: target_generation,
            width: 440,
            height: 308,
        },
        |state| state.incomplete_pending && !state.drop_view,
    )?;
    let stage = capture_stage(
        runtime,
        ctx,
        "exhaustion-recovery",
        (396, 264),
        Viewport {
            width: 360,
            height: 240,
            scale: 1.1,
        },
    )?;
    report.check(
        CHECK_IDS[14],
        stage_recovered(&stage, 396, 264)
            && stage["generation"]
                .as_u64()
                .is_some_and(|value| value > hidden_generation),
        json!({
            "stage": "exhaustion-recovery",
            "generation": stage["generation"],
            "hidden_generation": hidden_generation,
            "sequence": stage["sequence"],
            "recovered_status": stage["recovery"]["status"],
        }),
    );
    let before = recovery_diagnostics(runtime, browser)?;
    let old = snapshot_frame(&ctx.source)?;
    if old.generation != recovered_generation
        || stage["generation"].as_u64() != Some(old.generation)
        || stage["sequence"].as_u64() != Some(old.sequence)
        || stage["sha256"].as_str() != Some(old.sha256.as_str())
    {
        return Err("held baseline changed after CEF UI arm".into());
    }
    report.stage(stage);
    drop(fault);

    // ---- injected incomplete-frame rejection, then recovery ----
    let fault = FaultWindow::new(expected_fault);
    submit(
        handle,
        BrowserCommand::SetViewport(Viewport {
            width: 400,
            height: 280,
            scale: 1.1,
        }),
        "incomplete_resize",
    )?;
    wait(
        runtime,
        mode,
        ctx,
        "incomplete_rejection",
        |runtime, ctx| {
            let snapshot = ctx.source.snapshot();
            if snapshot.view.generation > target_generation {
                return Err("incomplete probe target generation superseded".into());
            }
            if snapshot.view.generation != target_generation {
                return Ok(false);
            }
            let state = probe_state(runtime, ctx.browser)?;
            let diagnostics = recovery_diagnostics(runtime, ctx.browser)?;
            Ok(state.incomplete_rejected > state_before.incomplete_rejected
                && !state.incomplete_pending
                && state.drop_view
                && diagnostics.incomplete_rejected > before.incomplete_rejected)
        },
    )?;
    let rejected_state = probe_state(runtime, browser)?;
    let rejected = recovery_diagnostics(runtime, browser)?;
    let snapshot = ctx.source.snapshot();
    let retained = snapshot.view.frame.as_ref().is_some_and(|frame| {
        frame.info.sequence == old.sequence && frame.info.surface_generation == old.generation
    });
    report.check(
        CHECK_IDS[15],
        rejected_state.incomplete_rejected == state_before.incomplete_rejected + 1
            && rejected.incomplete_rejected == before.incomplete_rejected + 1
            && rejected_state.drop_view,
        json!({
            "probe_incomplete_rejected_before": state_before.incomplete_rejected,
            "probe_incomplete_rejected_after": rejected_state.incomplete_rejected,
            "diagnostics_incomplete_rejected_before": before.incomplete_rejected,
            "diagnostics_incomplete_rejected_after": rejected.incomplete_rejected,
            "hold": rejected_state.drop_view,
        }),
    );
    report.check(
        CHECK_IDS[16],
        retained,
        json!({
            "expected": old.json(),
            "retained": snapshot.view.frame.as_ref().map(|frame| json!({
                "sequence": frame.info.sequence,
                "generation": frame.info.surface_generation,
                "width": frame.info.width,
                "height": frame.info.height,
            })),
        }),
    );
    report.check(
        CHECK_IDS[17],
        rejected.status.to_string() == "Pending"
            && snapshot.view.generation == target_generation
            && rejected.generation == target_generation
            && (rejected.expected_width, rejected.expected_height) == (440, 308)
            && snapshot
                .view
                .frame
                .as_ref()
                .is_some_and(|frame| frame.info.surface_generation != snapshot.view.generation),
        json!({
            "status": rejected.status,
            "expected_width": rejected.expected_width,
            "expected_height": rejected.expected_height,
            "current_view_generation": snapshot.view.generation,
            "retained_frame_generation": snapshot.view.frame.as_ref().map(|frame| frame.info.surface_generation),
        }),
    );
    report.check(
        CHECK_IDS[18],
        rejected.acks == before.acks,
        json!({"acks_before": before.acks, "acks_after": rejected.acks}),
    );

    let state_before_release = probe_state(runtime, browser)?;
    wait_probe_applied(
        runtime,
        mode,
        ctx,
        "release_incomplete_hold",
        state_before_release.applied,
        FrameProbeControl::DropView(false),
        |state| !state.drop_view && !state.incomplete_pending,
    )?;
    wait(runtime, mode, ctx, "incomplete-recovery", |runtime, ctx| {
        let snapshot = ctx.source.snapshot();
        let view = &snapshot.view;
        if view.generation > target_generation {
            return Err("incomplete recovery target generation superseded".into());
        }
        if view.generation != target_generation || !view.visible {
            return Ok(false);
        }
        let frame_ok = view.frame.as_ref().is_some_and(|frame| {
            frame.info.surface == SurfaceKind::View
                && frame.info.surface_generation == view.generation
                && frame.info.width == 440
                && frame.info.height == 308
                && valid_frame(frame)
        });
        if !frame_ok {
            return Ok(false);
        }
        Ok(status(runtime, ctx.browser)? == "Recovered")
    })?;
    let stage = capture_stage(
        runtime,
        ctx,
        "incomplete-recovery",
        (440, 308),
        Viewport {
            width: 400,
            height: 280,
            scale: 1.1,
        },
    )?;
    report.check(
        CHECK_IDS[19],
        stage_recovered(&stage, 440, 308)
            && stage["generation"].as_u64() == Some(target_generation)
            && stage["recovery"]["generation"].as_u64() == Some(target_generation)
            && before
                .acks
                .checked_add(1)
                .is_some_and(|ack| stage["recovery"]["acks"].as_u64() == Some(ack)),
        json!({
            "stage": "incomplete-recovery",
            "generation": stage["generation"],
            "rejected_generation": old.generation,
            "target_generation": target_generation,
            "sequence": stage["sequence"],
            "recovered_status": stage["recovery"]["status"],
            "acks_before": before.acks,
            "acks_after": stage["recovery"]["acks"],
        }),
    );
    report.stage(stage);
    drop(fault);

    let samples_ok = report
        .stages
        .iter()
        .zip(EXPECTED_STAGE_IDS)
        .all(|(stage, id)| {
            stage["id"].as_str() == Some(id)
                && stage["samples"].as_array().is_some_and(|samples| {
                    !samples.is_empty()
                        && samples.iter().all(|sample| sample["passed"] == true)
                        && samples.iter().any(|sample| {
                            sample["quadrant"] == "BL"
                                && sample["actual"][3] == 128
                                && sample["actual"][0] == 100
                        })
                        && samples.iter().any(|sample| {
                            sample["quadrant"] == "BR" && sample["actual"] == json!([0, 0, 0, 0])
                        })
                })
        });
    report.check(
        CHECK_IDS[20],
        samples_ok,
        json!({
            "stages": report.stages.len(),
            "sample_point_count_per_stage": report
                .stages
                .iter()
                .filter_map(|stage| stage["samples"].as_array().map(Vec::len))
                .min()
                .unwrap_or(0),
            "alpha_checked": true,
            "half_alpha_bgra": BL,
            "transparent_bgra": BR,
        }),
    );

    Ok(())
}

/// Runs the fixed validation protocol after the parent's Frames scenario has
/// created its dedicated capture Browser. The parent still owns the enclosing
/// close/shutdown/observer/native report; this function never initializes or
/// shuts the Runtime down, never creates a window and never captures a desktop.
///
/// `expected_fault` grants one expected ResizeExhausted event only. The
/// observer atomically consumes it and checks the total is exactly one. Device
/// and duplicate errors are fatal; every real incomplete error needs separate
/// callback-witness accounting, even during injection.
pub(super) fn run(
    runtime: &mut Runtime,
    mode: ThreadMode,
    handle: &BrowserHandle,
    directory: &Path,
    expected_fault: &AtomicBool,
) -> Result<Value, String> {
    let browser = handle.snapshot().browser;
    let directory = directory.to_path_buf();
    let mut report = Report::new(mode);
    let executed = (|| -> Result<(), String> {
        let source = runtime
            .frame_source(browser)
            .map_err(|error| format!("frame source: {error:?}"))?;
        report.backend = observed_backend(runtime, browser);
        let ctx = Ctx {
            browser,
            source,
            directory: directory.clone(),
            deadline: Instant::now() + TOTAL_LIMIT,
        };
        execute(runtime, mode, handle, &ctx, &mut report, expected_fault)?;
        report.backend = Some(
            observed_backend(runtime, browser)
                .ok_or_else(|| "actual CEF capture backend was never observed".to_string())?,
        );
        Ok(())
    })();
    match executed {
        Ok(()) if report.all_passed() => Ok(report.value(true, None)),
        Ok(()) => {
            let error = format!(
                "fixed capture-validation checks failed: {}",
                report.failed_ids().join(", ")
            );
            Err(write_failure(runtime, browser, &directory, &report, &error))
        }
        Err(error) => Err(write_failure(runtime, browser, &directory, &report, &error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: [u8; 16] = [
        60, 60, 220, 255, 60, 200, 60, 255, 100, 40, 20, 128, 0, 0, 0, 0,
    ];

    #[test]
    fn fixture_url_is_percent_encoded_css_without_script() {
        let url = fixture_url();
        assert!(url.starts_with("data:text/html,"));
        assert!(!url.contains('<') && !url.contains('>') && !url.contains('"'));
        assert!(!url.contains("script") && !url.contains("animation"));
        assert!(url.contains("%23tl") && url.contains("rgba%2840%2C80%2C200%2C0.5%29"));
    }

    #[test]
    fn percent_encode_round_trips_unreserved_only() {
        assert_eq!(percent_encode("aZ09-_.~"), "aZ09-_.~");
        assert_eq!(percent_encode("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn full_scan_rejects_a_wrong_channel_and_the_seam_is_not_blurred() {
        let mut pixels = FIXTURE.to_vec();
        assert!(samples_match(&pixels, 2, 2));
        pixels[0] = 59;
        assert!(!samples_match(&pixels, 2, 2));
        let make_frame = |bad: Option<usize>| {
            let budget = rust_cef_core::frame_budget::FrameBudget::new(256);
            let mut buffer = budget.allocate(8 * 8 * 4).unwrap();
            for y in 0..8 {
                for x in 0..8 {
                    let offset = ((y * 8 + x) * 4) as usize;
                    buffer.as_mut_slice()[offset..offset + 4]
                        .copy_from_slice(&expected_color(x, y, 8, 8));
                }
            }
            if let Some(index) = bad {
                buffer.as_mut_slice()[index] ^= 1;
            }
            OwnedFrame {
                info: rust_cef_runtime::ports::FrameInfo {
                    browser: BrowserId::new(
                        rust_cef_runtime::contracts::RuntimeGeneration::new(1),
                        1,
                    ),
                    surface: SurfaceKind::View,
                    surface_generation: 1,
                    sequence: 1,
                    width: 8,
                    height: 8,
                    format: PixelFormat::PremultipliedBgra8,
                },
                pixels: buffer.freeze(),
            }
        };
        assert!(valid_frame(&make_frame(None)));
        assert!(!valid_frame(&make_frame(Some(0))), "border is not exempt");
        assert!(
            !valid_frame(&make_frame(Some((4 * 8 + 4) * 4 + 3))),
            "seam alpha is not exempt"
        );
    }

    #[test]
    fn legacy_fingerprint_bytes_shape_is_unchanged() {
        let bytes = [1u8; 100];
        let fingerprint = fingerprint_bytes(&bytes).expect("fingerprint");
        assert_eq!(fingerprint.len(), 12);
        assert_eq!(&fingerprint[0..3], &[1, 1, 1]);
    }
}
