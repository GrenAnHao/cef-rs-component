//! Validation-host evidence only. It never accepts an error or calls CEF.
//! Snapshot fields are independent observations, not a native transaction ID.
use super::*;
use rust_cef_gpui::{
    ports::CaptureFailureKind,
    test_support::{FrameProbeState, NativeFrameRejection},
};
use std::fmt::Display;

const MAX_EVENTS: usize = 32;
#[derive(Clone, Default)]
pub(super) struct CaptureEvents {
    seen: u64,
    overflow: bool,
    events: Vec<(BrowserId, u64, EngineError)>,
}
impl CaptureEvents {
    pub(super) fn record(&mut self, browser: BrowserId, sequence: u64, error: &EngineError) {
        if self.seen == u64::MAX {
            self.overflow = true;
        } else {
            self.seen += 1;
        }
        if self.events.len() < MAX_EVENTS {
            self.events.push((browser, sequence, error.clone()));
        } else {
            self.overflow = true;
        }
    }
    /// Count recorded events of one capture kind for the given Browser. The
    /// ledger is written by the event sink itself, so this does not depend on
    /// any polling window the way a diagnostics counter does.
    pub(super) fn count_kind(&self, browser: BrowserId, kind: CaptureFailureKind) -> usize {
        self.events
            .iter()
            .filter(|(b, _, error)| {
                *b == browser
                    && matches!(
                        error,
                        EngineError::Capture {
                            kind: event_kind,
                            code: None
                        } if *event_kind == kind
                    )
            })
            .count()
    }

    pub(super) fn json(&self) -> String {
        let events = self
            .events
            .iter()
            .map(|(browser, sequence, error)| {
                let (kind, code) = match error {
                    EngineError::Capture { kind, code } => (
                        quote(&format!("{kind:?}")),
                        code.map_or("null".into(), |v| v.to_string()),
                    ),
                    EngineError::Backend { code } => (
                        quote("Backend"),
                        code.map_or("null".into(), |v| v.to_string()),
                    ),
                    _ => ("null".into(), "null".into()),
                };
                object(vec![
                    ("browser", bid(*browser)),
                    ("sequence", sequence.to_string()),
                    ("error", quote(&format!("{error:?}"))),
                    ("kind", kind),
                    ("code", code),
                ])
            })
            .collect::<Vec<_>>()
            .join(",");
        object(vec![
            ("seen", self.seen.to_string()),
            ("overflow", bool_json(self.overflow)),
            ("events", format!("[{events}]")),
        ])
    }
}
pub(super) fn is_letterbox_candidate(error: &EngineError) -> bool {
    matches!(
        error,
        EngineError::Capture {
            kind: CaptureFailureKind::IncompleteFrame | CaptureFailureKind::ResizeExhausted,
            code: None
        }
    )
}

/// Original good frame and original pending target. This is a validation-only
/// witness, not permission to consume an incomplete texture or change counters.
pub(super) struct LetterboxCut {
    pub browser: BrowserId,
    pub ui: u64,
    pub generation: u64,
    pub target: (u32, u32),
    pub frame: (u64, u64, u32, u32),
    pub sha256: String,
    pub acks: u64,
    pub io: (u64, u64, u64, u64),
}

impl LetterboxCut {
    pub(super) fn verify(
        &self,
        events: &CaptureEvents,
        native: &FrameProbeState,
        retired_failures: Option<u64>,
    ) -> Result<usize, String> {
        if events.overflow
            || native.native_rejections_overflow
            || events.seen != events.events.len() as u64
        {
            return Err("letterbox evidence overflow or missing event".into());
        }
        let mut previous = 0;
        let mut count = 0;
        let mut exhaustion = false;
        for (browser, sequence, error) in &events.events {
            if *browser != self.browser || *sequence <= previous {
                return Err("letterbox event Browser/sequence mismatch".into());
            }
            previous = *sequence;
            if is_letterbox_candidate(error) {
                let witness = native
                    .native_rejections
                    .get(count)
                    .ok_or_else(|| format!("unpaired CaptureFailed {error:?} event {sequence}"))?;
                if exhaustion || !self.matches(witness, (count + 1) as u64) {
                    return Err(format!(
                        "unqualified CaptureFailed {error:?} event {sequence}: {witness:?}"
                    ));
                }
                count += 1;
            } else if !exhaustion
                && matches!(
                    error,
                    EngineError::Capture {
                        kind: CaptureFailureKind::ResizeExhausted,
                        code: None
                    }
                )
            {
                exhaustion = true;
            } else {
                return Err(format!(
                    "unqualified CaptureFailed {error:?} event {sequence}"
                ));
            }
        }
        // In Threaded mode a callback may publish its row just after our event
        // snapshot. It must be paired exactly once when the Browser retires.
        if let Some(failures) = retired_failures {
            if !exhaustion || count != native.native_rejections.len() || failures != count as u64 {
                return Err("letterbox terminal event/row/failure counts differ".into());
            }
        }
        Ok(count)
    }

    fn matches(&self, w: &NativeFrameRejection, ordinal: u64) -> bool {
        let (cw, ch) = w.coded;
        let (vx, vy, vw, vh) = w.visible;
        let (cx, cy, width, height) = w.content;
        let ends = (
            vx.checked_add(vw),
            vy.checked_add(vh),
            cx.checked_add(width),
            cy.checked_add(height),
        );
        let (Some(vr), Some(vb), Some(cr), Some(cb)) = ends else {
            return false;
        };
        vw > 0
            && vh > 0
            && width > 0
            && height > 0
            && vx >= 0
            && vy >= 0
            && cw > 0
            && ch > 0
            && vr <= cw
            && vb <= ch
            && cx >= vx
            && cy >= vy
            && cr <= vr
            && cb <= vb
            && w.content != w.visible
            && w.browser == format!("{:?}", self.browser)
            && w.surface == "View"
            && self.ui != 0
            && w.thread == self.ui
            && w.failure_ordinal == ordinal
            && w.target_generation == self.generation
            && w.target == self.target
            && self.frame.0 < self.generation
            && w.source_generation_before == self.generation
            && w.source_generation_after == self.generation
            && w.frame_before == Some(self.frame)
            && w.frame_after == Some(self.frame)
            && w.same_frame
            && w.frame_sha256.as_deref() == Some(self.sha256.as_str())
            && w.recovery_status_before == "Pending"
            && w.recovery_status_after == "Pending"
            && w.acks_before == self.acks
            && w.acks_after == self.acks
            && w.native_io_before == self.io
            && w.native_io_after == self.io
    }
}

/// Resize does not include the Exhaustion probe's special terminal event. At
/// an active transition a native row may be ahead of the paired event, but
/// once the CEF endpoint has retired every sequence and counter must agree.
pub(super) fn verify_resize_prefix<'a>(
    events: &CaptureEvents,
    native: &FrameProbeState,
    mut cut_for_ordinal: impl FnMut(usize) -> Option<&'a LetterboxCut>,
    retired_failures: Option<u64>,
) -> Result<usize, String> {
    let count = events.events.len();
    let rows = &native.native_rejections;
    if events.overflow
        || native.native_rejections_overflow
        || events.seen != count as u64
        || rows.len() < count
        || retired_failures.is_some_and(|n| n != events.seen || rows.len() != count)
    {
        return Err("verified Resize event/native accounting mismatch or overflow".into());
    }
    let mut previous = 0;
    for (i, ((browser, seq, error), witness)) in events.events.iter().zip(rows).enumerate() {
        let ordinal = i + 1;
        if !is_letterbox_candidate(error)
            || *seq <= previous
            || !cut_for_ordinal(ordinal)
                .is_some_and(|cut| cut.browser == *browser && cut.matches(witness, ordinal as u64))
        {
            return Err(format!(
                "unqualified Resize CaptureFailed {error:?} event {seq}: {witness:?}"
            ));
        }
        previous = *seq;
    }
    Ok(count)
}

#[cfg(test)]
mod letterbox_tests {
    use super::*;
    use rust_cef_gpui::contracts::RuntimeGeneration;

    #[test]
    fn original_letterbox_witness_requires_exact_prefix_and_terminal_retirement_counts() {
        let browser = BrowserId::new(RuntimeGeneration::new(1), 1);
        let cut = LetterboxCut {
            browser,
            ui: 33344,
            generation: 2,
            target: (750, 450),
            frame: (1, 1, 480, 360),
            sha256: "7462d03f687658473e1faeb822378bdedc92db6c8461b0e4100218c9351b1c76".into(),
            acks: 0,
            io: (1, 1, 1, 1),
        };
        let evidence = NativeFrameRejection {
            browser: format!("{browser:?}"),
            surface: "View",
            thread: cut.ui,
            failure_ordinal: 1,
            frame_sha256: Some(cut.sha256.clone()),
            coded: (480, 360),
            visible: (0, 0, 480, 360),
            content: (0, 36, 480, 288),
            source_generation_before: 2,
            source_generation_after: 2,
            frame_before: Some(cut.frame),
            frame_after: Some(cut.frame),
            same_frame: true,
            recovery_status_before: "Pending",
            recovery_status_after: "Pending",
            target_generation: 2,
            target: cut.target,
            acks_before: 0,
            acks_after: 0,
            native_io_before: cut.io,
            native_io_after: cut.io,
        };
        let mut events = CaptureEvents::default();
        let rejected = EngineError::Capture {
            kind: CaptureFailureKind::IncompleteFrame,
            code: None,
        };
        let exhausted = EngineError::Capture {
            kind: CaptureFailureKind::ResizeExhausted,
            code: None,
        };
        let mut native = FrameProbeState::default();
        native.native_rejections.push(evidence.clone());
        assert_eq!(cut.verify(&events, &native, None), Ok(0)); // Native row can precede delivery.
        assert_eq!(
            verify_resize_prefix(&events, &native, |_| Some(&cut), None),
            Ok(0)
        );
        assert!(verify_resize_prefix(&events, &native, |_| Some(&cut), Some(1)).is_err());
        events.record(browser, 11, &rejected);
        let resize_events = events.clone();
        assert_eq!(
            verify_resize_prefix(&events, &native, |_| Some(&cut), Some(1)),
            Ok(1)
        );
        assert!(verify_resize_prefix(&events, &native, |_| None, None).is_err());
        assert_eq!(cut.verify(&events, &native, None), Ok(1));
        assert!(
            cut.verify(&events, &native, Some(1)).is_err(),
            "no actual exhaustion event"
        );
        events.record(browser, 12, &exhausted);
        assert_eq!(cut.verify(&events, &native, Some(1)), Ok(1));
        assert!(verify_resize_prefix(&events, &native, |_| Some(&cut), None).is_err());
        assert!(cut.verify(&events, &native, Some(0)).is_err());
        let changes: [fn(&mut NativeFrameRejection); 10] = [
            |w| w.content.3 = 0,
            |w| w.content.2 = i32::MAX,
            |w| w.coded.0 = 479,
            |w| w.acks_after = 1,
            |w| w.target = (480, 360),
            |w| w.same_frame = false,
            |w| w.frame_sha256 = None,
            |w| w.native_io_after.0 = 2,
            |w| w.failure_ordinal = 2,
            |w| w.recovery_status_after = "Exhausted",
        ];
        for change in changes {
            native.native_rejections[0] = evidence.clone();
            change(&mut native.native_rejections[0]);
            assert!(cut.verify(&events, &native, None).is_err());
            assert!(
                verify_resize_prefix(&resize_events, &native, |_| Some(&cut), Some(1)).is_err()
            );
        }
        native.native_rejections[0] = evidence;
        events.record(browser, 13, &rejected);
        assert!(cut.verify(&events, &native, None).is_err());
    }
}

fn bid(id: BrowserId) -> String {
    format!("[{},{}]", id.runtime().value(), id.local())
}
fn pair<T: Display>((a, b): (T, T)) -> String {
    format!("[{a},{b}]")
}
fn quad<T: Display>((a, b, c, d): (T, T, T, T)) -> String {
    format!("[{a},{b},{c},{d}]")
}
fn frame_identity(value: Option<(u64, u64, u32, u32)>) -> String {
    value.map_or("null".into(), |(g, s, w, h)| format!("[{g},{s},{w},{h}]"))
}
pub(super) fn probe_json(p: &FrameProbeState) -> String {
    // Keep all callback-owned fields, even for a failure/overflow. Merely
    // serializing a witness never authorizes it or changes CaptureFailed.
    let rows = p
        .native_rejections
        .iter()
        .map(|r| {
            object(vec![
                ("browser", quote(&r.browser)),
                ("surface", quote(r.surface)),
                ("thread", r.thread.to_string()),
                ("failure_ordinal", r.failure_ordinal.to_string()),
                (
                    "frame_sha256",
                    r.frame_sha256.as_deref().map_or("null".into(), quote),
                ),
                ("coded", pair(r.coded)),
                ("visible", quad(r.visible)),
                ("content", quad(r.content)),
                (
                    "source_generation_before",
                    r.source_generation_before.to_string(),
                ),
                (
                    "source_generation_after",
                    r.source_generation_after.to_string(),
                ),
                ("frame_before", frame_identity(r.frame_before)),
                ("frame_after", frame_identity(r.frame_after)),
                ("same_frame", bool_json(r.same_frame)),
                ("recovery_status_before", quote(r.recovery_status_before)),
                ("recovery_status_after", quote(r.recovery_status_after)),
                ("target_generation", r.target_generation.to_string()),
                ("target", pair(r.target)),
                ("acks_before", r.acks_before.to_string()),
                ("acks_after", r.acks_after.to_string()),
                ("native_io_before", quad(r.native_io_before)),
                ("native_io_after", quad(r.native_io_after)),
            ])
        })
        .collect::<Vec<_>>()
        .join(",");
    object(vec![
        ("drop_view", bool_json(p.drop_view)),
        ("applied", p.applied.to_string()),
        ("dropped", p.dropped.to_string()),
        ("incomplete_pending", bool_json(p.incomplete_pending)),
        ("incomplete_rejected", p.incomplete_rejected.to_string()),
        (
            "native_rejections_overflow",
            bool_json(p.native_rejections_overflow),
        ),
        ("native_rejections", format!("[{rows}]")),
    ])
}
pub(super) fn atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("pending");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    if let Err(primary) = std::io::Write::write_all(&mut file, bytes) {
        drop(file);
        let cleanup = std::fs::remove_file(&tmp).err();
        return Err(format!(
            "evidence write failed: {primary}; temporary cleanup: {cleanup:?}"
        ));
    }
    drop(file);
    // Same-directory hard-link publication is atomic and no-replace on the
    // supported local Windows filesystem. No rename/overwrite fallback.
    if let Err(primary) = std::fs::hard_link(&tmp, path) {
        let cleanup = std::fs::remove_file(&tmp).err();
        return Err(format!(
            "no-replace evidence publication: {primary}; temporary cleanup: {cleanup:?}"
        ));
    }
    if let Err(secondary) = std::fs::remove_file(&tmp) {
        // The complete destination is already published. Do not lie that raw
        // was not written; only the redundant temporary link needs cleanup.
        eprintln!(
            "Exhaustion published evidence {}; secondary temporary cleanup failure: {secondary}",
            path.display()
        );
    }
    Ok(())
}

pub(super) fn retain_primary(
    primary: Result<(), String>,
    retirement: Result<(), String>,
) -> Result<(), String> {
    match (primary, retirement) {
        (Err(primary), Err(secondary)) => {
            eprintln!(
                "Exhaustion secondary probe retirement failure: {secondary}; original: {primary}"
            );
            Err(primary)
        }
        (Ok(()), Err(secondary)) => Err(format!("probe retirement: {secondary}")),
        (primary, Ok(())) => primary,
    }
}
pub(super) fn recovery_json(r: &rust_cef_gpui::runtime::FrameRecoveryDiagnostics) -> String {
    let list = |v: &[u64]| {
        format!(
            "[{}]",
            v.iter().map(u64::to_string).collect::<Vec<_>>().join(",")
        )
    };
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
        ("native_offsets_us", list(&r.native_offsets_us)),
        ("request_offsets_us", list(&r.request_offsets_us)),
        ("acks", r.acks.to_string()),
        ("exhaustions", r.exhaustions.to_string()),
        ("queued", bool_json(r.queued)),
        ("post_failures", r.post_failures.to_string()),
        (
            "min_spacing_us",
            r.min_spacing_us.map_or("null".into(), |v| v.to_string()),
        ),
        ("probe_dropped", r.probe_dropped.to_string()),
        ("incomplete_rejected", r.incomplete_rejected.to_string()),
    ])
}

pub(super) fn capture_once(
    cx: &mut AsyncApp,
    rt: &Runtime,
    stats: &Stats,
    root: WindowHandle<HostRoot>,
    options: &Options,
    out: &mut Outcome,
    phase: &str,
    baseline: Option<&rust_cef_gpui::runtime::FrameRecoveryDiagnostics>,
    original: &str,
) {
    if out.failure_evidence_attempted {
        return;
    }
    out.failure_evidence_attempted = true;
    if let Err(secondary) =
        save_failure(cx, rt, stats, root, options, out, phase, baseline, original)
    {
        // Diagnostic IO cannot clear, replace or accept the original failure.
        eprintln!("Exhaustion failure evidence unavailable: {secondary}; original: {original}");
    }
}
fn save_failure(
    cx: &mut AsyncApp,
    rt: &Runtime,
    stats: &Stats,
    root: WindowHandle<HostRoot>,
    options: &Options,
    out: &Outcome,
    phase: &str,
    baseline: Option<&rust_cef_gpui::runtime::FrameRecoveryDiagnostics>,
    original: &str,
) -> Result<(), String> {
    let case = options
        .report
        .parent()
        .ok_or("failure evidence case missing")?;
    // End all registry/event/GPUI borrows before file IO. Never run another
    // step/pump/dispatch here or clear the original sticky error.
    let events = stats.capture_events.lock().unwrap().clone();
    let fault = stats.fault.lock().unwrap().json();
    let id = out.browser;
    let (mut source, mut source_error, mut probe, mut capture, mut recovery) = (
        "null".into(),
        "null".into(),
        "null".into(),
        "null".into(),
        "null".into(),
    );
    let mut raw: Option<Arc<OwnedFrame>> = None;
    if let Some(id) = id {
        probe = rt
            .probe_frame_state(id)
            .as_ref()
            .map_or("null".into(), probe_json);
        capture = match rt.frame_capture_diagnostics(id) {
            Ok(c) => object(vec![
                ("owner_thread", c.owner_thread.to_string()),
                ("retirement_thread", c.retirement_thread.to_string()),
                (
                    "requested_backend",
                    c.requested_backend.map_or("null".into(), quote),
                ),
                (
                    "observed_backend",
                    c.observed_backend.map_or("null".into(), quote),
                ),
                ("software_captures", c.software_captures.to_string()),
                ("d3d11_captures", c.d3d11_captures.to_string()),
                ("failures", c.failures.to_string()),
                (
                    "last_failure",
                    c.last_failure.as_deref().map_or("null".into(), quote),
                ),
                (
                    "last_failure_code",
                    c.last_failure_code.map_or("null".into(), |v| v.to_string()),
                ),
                ("opens", c.opens.to_string()),
                ("copies", c.copies.to_string()),
                ("maps", c.maps.to_string()),
                ("unmaps", c.unmaps.to_string()),
                ("staging_bytes", c.staging_bytes.to_string()),
                ("staging_peak", c.staging_peak.to_string()),
                ("live_imports", c.live_imports.to_string()),
                ("native_resources_live", bool_json(c.native_resources_live)),
                ("retired", bool_json(c.retired)),
                ("abandoned", bool_json(c.abandoned)),
            ]),
            Err(e) => object(vec![("query_error", quote(&format!("{e:?}")))]),
        };
        recovery = match rt.frame_recovery_diagnostics(id) {
            Ok(r) => recovery_json(&r),
            Err(e) => object(vec![("query_error", quote(&format!("{e:?}")))]),
        };
        match rt.frame_source(id) {
            Ok(s) => {
                let s = s.snapshot();
                let viewport = s.viewport.map_or("null".into(), |v| {
                    object(vec![
                        ("width", v.width.to_string()),
                        ("height", v.height.to_string()),
                        ("scale", v.scale.to_string()),
                    ])
                });
                let frame = s.view.frame.as_ref().map_or("null".into(), |f| {
                    object(vec![
                        ("file", quote("exhaustion-failed-view.bgra")),
                        ("browser", bid(f.info.browser)),
                        ("surface", quote(&format!("{:?}", f.info.surface))),
                        ("generation", f.info.surface_generation.to_string()),
                        ("sequence", f.info.sequence.to_string()),
                        ("width", f.info.width.to_string()),
                        ("height", f.info.height.to_string()),
                        ("bytes", f.pixels.len().to_string()),
                    ])
                });
                raw = s.view.frame.clone();
                source = object(vec![
                    ("closed", bool_json(s.closed)),
                    ("view_visible", bool_json(s.view.visible)),
                    ("view_generation", s.view.generation.to_string()),
                    ("popup_visible", bool_json(s.popup.visible)),
                    ("popup_generation", s.popup.generation.to_string()),
                    ("popup_frame_present", bool_json(s.popup.frame.is_some())),
                    ("viewport", viewport),
                    ("frame", frame),
                ]);
            }
            Err(e) => source_error = quote(&format!("{e:?}")),
        }
    }
    let (paint, paint_error) = match update(cx, root, |host, _, app| {
        host.browser.as_ref().map(|e| e.read(app).diagnostics())
    }) {
        Ok(d) => (
            d.as_ref().map_or("null".into(), metrics_json),
            "null".into(),
        ),
        Err(e) => ("null".into(), quote(&e)),
    };
    let runtime = rt.diagnostics();
    let raw_error = raw.as_ref().and_then(|frame| {
        atomic(
            &case.join("exhaustion-failed-view.bgra"),
            frame.pixels.as_slice(),
        )
        .err()
    });
    let raw_written = raw.is_some() && raw_error.is_none();
    let text = object(vec![
        ("schema", "1".into()),
        ("kind", quote("exhaustion-failure-evidence")),
        ("scope", quote("recovery-exhaustion-slice")),
        ("pid", std::process::id().to_string()),
        ("browser", id.map_or("null".into(), bid)),
        ("phase", quote(phase)),
        (
            "baseline_recovery",
            baseline.map_or("null".into(), recovery_json),
        ),
        ("original_error", quote(original)),
        ("success", bool_json(false)),
        ("task_11_3_accepted", bool_json(false)),
        ("error_gate_changed", bool_json(false)),
        ("input_sent", bool_json(false)),
        ("foreground_forced", bool_json(false)),
        ("initial_foreground", out.initial_foreground.json(options)),
        ("capture_events", events.json()),
        ("expected_fault", fault),
        ("probe", probe),
        ("capture", capture),
        ("recovery", recovery),
        ("source", source),
        ("source_query_error", source_error),
        ("paint", paint),
        ("paint_query_error", paint_error),
        ("raw_written", bool_json(raw_written)),
        (
            "raw_write_error",
            raw_error.as_deref().map_or("null".into(), quote),
        ),
        (
            "unavailable_reason",
            if id.is_none() {
                quote("no Browser identity allocated")
            } else {
                "null".into()
            },
        ),
        ("main_thread", runtime.main_thread_id.to_string()),
        (
            "cef_ui_thread",
            runtime
                .cef_ui_thread_id
                .map_or("null".into(), |v| v.to_string()),
        ),
        (
            "limits",
            quote(
                "Failure-only independent snapshots, not atomic native callback/navigation IDs. Native rejection ordinal is not a CaptureFailed event sequence. No error allowance, counter reset, diagnostic pump/dispatch/retry or forced foreground. Optional initial_foreground records the separately approved single ordinary owned-window request; the observer never activates. Source raw is not a desktop image; absence/query errors/overflow remain explicit; original failure always remains failed.",
            ),
        ),
    ]);
    atomic(
        &case.join("exhaustion-failure-evidence.json"),
        text.as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_gpui::contracts::RuntimeGeneration;
    #[test]
    fn actual_capture_sink_stays_fatal_and_snapshot_bounds_keep_original_identity() {
        let id = BrowserId::new(RuntimeGeneration::new(7), 3);
        let stats = Arc::new(Stats::default());
        let events = Events(stats.clone());
        let incomplete = EngineError::Capture {
            kind: rust_cef_gpui::ports::CaptureFailureKind::IncompleteFrame,
            code: None,
        };
        for sequence in 1..=33 {
            assert_eq!(
                events.publish(BrowserEventEnvelope {
                    browser: id,
                    sequence,
                    event: BrowserEvent::CaptureFailed(incomplete.clone())
                }),
                PublishResult::Published
            );
        }
        assert_eq!(
            stats.check(),
            Err("native event: Capture { kind: IncompleteFrame, code: None }".into())
        );
        assert_eq!(stats.fault.lock().unwrap().count(), 0);
        let copy = stats.capture_events.lock().unwrap().clone();
        assert_eq!(copy.seen, 33);
        assert!(copy.overflow);
        assert_eq!(copy.events.len(), 32);
        assert_eq!(copy.events[0], (id, 1, incomplete.clone()));
        assert_eq!(copy.events[31], (id, 32, incomplete));
        assert!(copy.json().contains("\"browser\":[7,3]"));
        let good = Arc::new(Stats::default());
        good.fault.lock().unwrap().arm(id).unwrap();
        let sink = Events(good.clone());
        sink.publish(BrowserEventEnvelope {
            browser: id,
            sequence: 1,
            event: BrowserEvent::CaptureFailed(EngineError::Capture {
                kind: rust_cef_gpui::ports::CaptureFailureKind::ResizeExhausted,
                code: None,
            }),
        });
        assert!(good.check().is_ok());
        assert_eq!(good.fault.lock().unwrap().count(), 1);
        sink.publish(BrowserEventEnvelope {
            browser: id,
            sequence: 2,
            event: BrowserEvent::CaptureFailed(EngineError::Closed),
        });
        assert_eq!(good.check(), Err("native event: Closed".into()));
        assert_eq!(good.capture_events.lock().unwrap().events.len(), 2);
    }
    #[test]
    fn diagnostic_wire_keeps_null_signed_values_and_overflow_explicit() {
        assert_eq!(
            retain_primary(Err("primary".into()), Err("secondary".into())),
            Err("primary".into())
        );
        assert_eq!(
            retain_primary(Ok(()), Err("secondary".into())),
            Err("probe retirement: secondary".into())
        );
        assert_eq!(
            retain_primary(Err("primary".into()), Ok(())),
            Err("primary".into())
        );
        let mut rec = rust_cef_gpui::runtime::FrameRecoveryDiagnostics::default();
        rec.min_spacing_us = Some(100_123);
        rec.probe_dropped = 8;
        rec.incomplete_rejected = 2;
        let full = recovery_json(&rec);
        assert!(full.contains("\"min_spacing_us\":100123"));
        assert!(full.contains("\"probe_dropped\":8"));
        assert!(full.contains("\"incomplete_rejected\":2"));
        let mut log = CaptureEvents::default();
        log.record(
            BrowserId::new(RuntimeGeneration::new(1), 1),
            9,
            &EngineError::Backend {
                code: Some(-2147467259),
            },
        );
        assert!(log.json().contains("\"code\":-2147467259"));
        assert_eq!(frame_identity(None), "null");
        assert_eq!(frame_identity(Some((2, 3, 480, 360))), "[2,3,480,360]");
        assert_eq!(quad((-1, 2, 3, 4)), "[-1,2,3,4]");
        let mut probe = FrameProbeState::default();
        probe.drop_view = true;
        probe.applied = 2;
        probe.native_rejections_overflow = true;
        let text = probe_json(&probe);
        assert!(text.contains("\"drop_view\":true"));
        assert!(text.contains("\"applied\":2"));
        assert!(text.contains("\"native_rejections_overflow\":true"));
        assert!(text.contains("\"native_rejections\":[]"));
    }
    #[test]
    fn foreground_opt_in_cannot_change_unfocused_or_other_scenarios() {
        assert_eq!(
            initial_foreground_flag(None, HostScenario::Unfocused),
            Ok(false)
        );
        assert_eq!(
            initial_foreground_flag(Some("true"), HostScenario::Exhaustion),
            Ok(true)
        );
        for scenario in [
            HostScenario::Embed,
            HostScenario::Input,
            HostScenario::Resize,
            HostScenario::Unfocused,
            HostScenario::ImeObserve,
            HostScenario::Popup,
            HostScenario::Dispatch,
        ] {
            assert!(initial_foreground_flag(Some("true"), scenario).is_err());
        }
        assert!(initial_foreground_flag(Some("yes"), HostScenario::Exhaustion).is_err());
    }
    #[test]
    fn evidence_publication_never_replaces_existing_destination_or_temporary() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("gpui-cef-evidence-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let dest = dir.join("snapshot.json");
        atomic(&dest, b"original").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"original");
        assert!(!dest.with_extension("pending").exists());
        // Exercises the no-replace hard-link, not just a racy exists precheck.
        assert!(atomic(&dest, b"replacement").is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"original");
        assert!(!dest.with_extension("pending").exists());
        let next = dir.join("next.json");
        let temporary = next.with_extension("pending");
        std::fs::write(&temporary, b"existing temporary").unwrap();
        assert!(atomic(&next, b"new data").is_err());
        assert_eq!(std::fs::read(&temporary).unwrap(), b"existing temporary");
        assert!(!next.exists());
        std::fs::remove_dir_all(&dir).unwrap(); // Only this test's newly-created scratch tree.
    }
}
