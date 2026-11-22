//! Measure one original VIEW at a time, without crossing its GPUI window binding.
use super::*;
use rust_cef_gpui::ports::FrameSource;

pub(super) struct Lane<'a> {
    pub root: WindowHandle<HostRoot>,
    pub view: &'a Entity<CefBrowser>,
    pub source: &'a dyn FrameSource,
    pub browser: BrowserId,
    pub dims: (u32, u32),
    pub pixel: [u8; 4],
    pub window: Option<u64>,
}

pub(super) struct Samples {
    browser: BrowserId,
    generation: u64,
    baseline_sequence: u64,
    baseline_images: u64,
    pub sequence: u64,
    pub paint: ViewDiagnostics,
    intervals: Vec<u64>,
    copies: Vec<u64>,
    pumps: Pumps,
}

impl Lane<'_> {
    pub(super) async fn measure(
        &self,
        s: &mut Scene<'_>,
        deadline: Instant,
        peer: bool,
    ) -> Result<Samples, String> {
        let Self {
            root,
            view,
            source,
            browser,
            dims,
            pixel,
            window,
        } = *self;
        let baseline = update(s.cx, root, |_, _, cx| view.read(cx).diagnostics())?;
        let generation = baseline.painted_generation;
        let baseline_sequence = baseline.painted_sequence;
        let armed = update(s.cx, root, |_, _, cx| {
            view.update(cx, |v, _| {
                v.probe_arm_view_paint_timing(generation, baseline_sequence)
            })
        })?;
        if !armed {
            return Err(format!(
                "VIEW paint timing could not be armed for {browser:?}"
            ));
        }
        let origin = Instant::now();
        s.rt.probe_arm_view_copy_timing(browser)
            .map_err(|e| format!("VIEW copy timing arm for {browser:?}: {e:?}"))?;
        let mut intervals = Vec::with_capacity(VIEW_COPY_SAMPLE_CAPACITY);
        let mut copies = Vec::with_capacity(VIEW_COPY_SAMPLE_CAPACITY);
        let mut pumps = Pumps::default();
        let mut previous = baseline_sequence;
        let mut bound_window = window;
        for step in 1..=VIEW_COPY_SAMPLE_CAPACITY {
            s.rt.probe_frame_control(browser, FrameProbeControl::InvalidateView)
                .map_err(|e| format!("{browser:?} timing Invalidate {step}: {e:?}"))?;
            let seq = loop {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "{browser:?} timing VIEW publication {step} timeout"
                    ));
                }
                s.service(true, Some(&mut pumps)).await?;
                let control =
                    s.rt.probe_frame_state(browser)
                        .ok_or("timing control state missing")?;
                if let Some(frame) = source.latest(SurfaceKind::View)
                    && control.applied == step as u64
                    && frame.info.sequence > previous
                {
                    if !frame_matches(&frame, browser, generation, dims, pixel) {
                        return Err(format!(
                            "{browser:?} timing VIEW identity/extent/pixels changed"
                        ));
                    }
                    // The exact image must reach the original View, not just the mailbox.
                    if update(s.cx, root, |_, _, cx| {
                        view.read(cx)
                            .probe_has_view_image(generation, frame.info.sequence)
                    })? {
                        break frame.info.sequence;
                    }
                }
            };
            let paint = loop {
                let samples = update(s.cx, root, |_, _, cx| {
                    view.read(cx).probe_view_paint_samples()
                })?
                .ok_or("VIEW paint timing lane lost")?;
                if samples.len() == step {
                    break samples[step - 1];
                }
                if samples.len() > step || Instant::now() >= deadline {
                    return Err(format!(
                        "{browser:?} timing paint {step} missing or skipped"
                    ));
                }
                // The owner turn lets us yield, but does not itself prove an image paint.
                rendered(s.cx, root).await?;
            };
            let recorded =
                s.rt.probe_view_copy_samples(browser)
                    .ok_or("VIEW copy timing lane lost")?;
            if recorded.len() != step {
                return Err(format!(
                    "{browser:?} timing copy count diverged at {step}: {}",
                    recorded.len()
                ));
            }
            let c = &recorded[step - 1];
            if c.browser != (browser.runtime().value(), browser.local())
                || c.generation != generation
                || c.sequence != seq
                || (c.width, c.height) != dims
                || paint.browser != browser
                || paint.generation != generation
                || paint.sequence != seq
                || (paint.width, paint.height) != dims
                || bound_window.is_some_and(|id| id != paint.window)
                || s.rt.diagnostics().cef_ui_thread_id != Some(c.ui_thread)
            {
                return Err(format!(
                    "{browser:?} original copy/paint attribution mismatch at {step}"
                ));
            }
            bound_window = Some(paint.window);
            let paint_ns = ns_from(origin, paint.paint_finished_at)?;
            let copy_ns = ns_from(origin, c.copy_finished_at)?;
            let elapsed = paint_ns
                .checked_sub(copy_ns)
                .ok_or("paint preceded original copy completion")?;
            intervals.push(elapsed);
            copies.push(c.copy_ns);
            let rows = if peer {
                &mut s.out.paint_timing_peer_rows
            } else {
                &mut s.out.paint_timing_rows
            };
            let mut row = vec![
                ("browser", identity(browser)),
                ("gpui_window", paint.window.to_string()),
                ("generation", generation.to_string()),
                ("sequence", seq.to_string()),
                ("width", dims.0.to_string()),
                ("height", dims.1.to_string()),
                ("capture_ui_thread", c.ui_thread.to_string()),
                ("copy_ns", c.copy_ns.to_string()),
                ("copy_completed_ns", copy_ns.to_string()),
                ("paint_completed_ns", paint_ns.to_string()),
                ("copy_to_paint_ns", elapsed.to_string()),
            ];
            if let Some((high, low)) = c.readback_adapter_luid {
                row.push(("readback_adapter_luid", format!("[{high},{low}]")));
            }
            rows.push(object(row));
            previous = seq;
        }
        let paint = update(s.cx, root, |_, _, cx| view.read(cx).diagnostics())?;
        let samples = Samples {
            browser,
            generation,
            baseline_sequence,
            baseline_images: baseline.created_images,
            sequence: previous,
            paint,
            intervals,
            copies,
            pumps,
        };
        if !samples.valid() {
            return Err(format!(
                "{browser:?} final VIEW paint or timing samples invalid"
            ));
        }
        Ok(samples)
    }
}

impl Samples {
    fn valid(&self) -> bool {
        self.paint.painted_sequence == self.sequence
            && self.paint.painted_generation == self.generation
            && self.paint.last_error.is_none()
            && self.paint.created_images > self.baseline_images
            && self.intervals.iter().any(|n| *n > 0)
            && self.copies.iter().any(|n| *n > 0)
    }

    pub(super) fn fields(self, rows: &[String]) -> Vec<(&'static str, String)> {
        vec![
            ("browser", identity(self.browser)),
            ("generation", self.generation.to_string()),
            ("baseline_sequence", self.baseline_sequence.to_string()),
            ("measurement_count", rows.len().to_string()),
            ("rows", format!("[{}]", rows.join(","))),
            ("copy_ns", distribution(self.copies)),
            ("copy_to_paint_ns", distribution(self.intervals)),
            ("pump_calls", self.pumps.count.to_string()),
            ("pump_samples_ns", format!("{:?}", self.pumps.samples)),
            ("pump_ns", distribution(self.pumps.samples)),
        ]
    }
}
