//! The named-profile companion can share A's host or own a separate passive window.
//! B always paints again on its original binding after A retires; timing B is explicit.
use super::*;
use rust_cef_gpui::{
    contracts::ProfileHandle,
    ports::{BrowserHandle, FrameSource},
};

pub(super) struct Peer {
    handle: BrowserHandle,
    view: Entity<CefBrowser>,
    source: Arc<dyn FrameSource>,
    root: WindowHandle<HostRoot>,
    separate: bool,
    dims: (u32, u32),
}

const PEER_PIXEL: [u8; 4] = [180, 40, 110, 255];
const PEER_URL: &str = "data:text/html,%3Cbody%20style%3D%22margin%3A0%3Bbackground%3Argb(110%2C40%2C180)%22%3E%3C%2Fbody%3E";

fn open_window(cx: &mut AsyncApp) -> Result<WindowHandle<HostRoot>, String> {
    cx.open_window(
        WindowOptions {
            focus: false,
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                gpui::point(px(410.), px(40.)),
                size(px(360.), px(310.)),
            ))),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some(windows_pair::PEER_TITLE.into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        |w, cx| {
            w.on_window_should_close(cx, |_, _| false);
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
                extent: (320., 240.),
            })
        },
    )
    .map_err(|e| format!("timing peer window: {e}"))
}

impl Peer {
    pub(super) fn open(
        s: &mut Scene<'_>,
        options: &Options,
        profile: ProfileHandle,
    ) -> Result<Self, String> {
        let separate = options.timing_profile_windows;
        let root = if separate {
            let root = open_window(s.cx)?;
            s.out.additional_roots.push(root);
            root
        } else {
            s.root
        };
        let (scale, visible) = update(s.cx, root, |_, w, _| (w.scale_factor(), w.is_visible()))?;
        let dims = dimensions(scale)?;
        if !visible {
            return Err("companion owned window is hidden".into());
        }
        let handle =
            s.rt.create_browser(BrowserConfig {
                initial_url: if options.timing_window_find_pressure {
                    if !separate {
                        return Err("B Find requires original separate GPUI window".into());
                    }
                    window_find::url()?
                } else {
                    if separate { PEER_URL } else { URL }.into()
                },
                profile: Some(profile),
                render_preference: options.backend,
                viewport: Viewport {
                    width: 320,
                    height: 240,
                    scale,
                },
                options: Default::default(),
            })
            .map_err(|e| format!("companion Browser create: {e:?}"))?;
        let id = handle.snapshot().browser;
        s.out.additional_browsers.push(id);
        let source =
            s.rt.frame_source(id)
                .map_err(|e| format!("companion frame source: {e:?}"))?;
        let attachment =
            s.rt.attach_browser(&handle)
                .map_err(|e| format!("companion attachment: {e:?}"))?;
        if s.rt.attach_browser(&handle).is_ok() {
            return Err("duplicate companion attachment accepted".into());
        }
        let view = update(s.cx, root, |host, window, cx| {
            let view = CefBrowser::new(attachment, window, cx).map_err(|e| e.to_string())?;
            s.composition
                .register(&view, window, cx)
                .map_err(|e| format!("companion route: {e:?}"))?;
            if separate {
                host.browser = Some(view.clone());
            } else {
                host.multi = true;
                host.secondary = Some(view.clone());
            }
            cx.notify();
            Ok::<_, String>(view)
        })??;
        Ok(Self {
            handle,
            view,
            source,
            root,
            separate,
            dims,
        })
    }

    pub(super) fn root(&self) -> WindowHandle<HostRoot> {
        self.root
    }

    fn expected_pixel(&self) -> [u8; 4] {
        if self.separate { PEER_PIXEL } else { A_PIXEL }
    }

    pub(super) fn handle(&self) -> &BrowserHandle {
        &self.handle
    }

    pub(super) fn view(&self) -> &Entity<CefBrowser> {
        &self.view
    }

    pub(super) fn id(&self) -> BrowserId {
        self.handle.snapshot().browser
    }

    pub(super) fn is_ready(&self) -> bool {
        self.handle.snapshot().is_ready()
    }

    pub(super) fn timing_lane(&self, window: u64) -> samples::Lane<'_> {
        samples::Lane {
            root: self.root,
            view: &self.view,
            source: self.source.as_ref(),
            browser: self.id(),
            dims: self.dims,
            pixel: PEER_PIXEL,
            window: Some(window),
        }
    }

    pub(super) async fn published_paint(
        &self,
        s: &mut Scene<'_>,
        after: Option<(u64, u64, u64)>,
        deadline: Instant,
    ) -> Result<ViewDiagnostics, String> {
        loop {
            if Instant::now() >= deadline {
                return Err("companion actual VIEW paint timeout".into());
            }
            s.service(false, None).await?;
            rendered(s.cx, self.root).await?;
            let d = update(s.cx, self.root, |_, _, cx| self.view.read(cx).diagnostics())?;
            let snapshot = self.handle.snapshot();
            if let Some(frame) = self.source.latest(SurfaceKind::View)
                && snapshot.is_ready()
                && !snapshot.navigation.is_loading
                && d.last_error.is_none()
                && d.paint_calls > 0
                && d.painted_sequence == frame.info.sequence
                && d.painted_sequence > 0
                && d.painted_generation == frame.info.surface_generation
                && frame_matches(
                    &frame,
                    self.id(),
                    d.painted_generation,
                    self.dims,
                    self.expected_pixel(),
                )
                && after.is_none_or(|(generation, sequence, applied)| {
                    d.painted_generation == generation
                        && d.painted_sequence > sequence
                        && s.rt
                            .probe_frame_state(self.id())
                            .is_some_and(|c| c.applied == applied)
                })
            {
                return Ok(d);
            }
        }
    }

    pub(super) async fn retire_after_original(
        self,
        s: &mut Scene<'_>,
        first: BrowserId,
        initial: ViewDiagnostics,
        before_close: ViewDiagnostics,
        backend: &str,
        deadline: Instant,
        find: Option<&mut window_find::Held>,
        timing_both: bool,
    ) -> Result<String, String> {
        let browser = self.id();
        if initial.painted_generation != before_close.painted_generation
            || initial.painted_sequence > before_close.painted_sequence
            || !self.handle.snapshot().is_ready()
            || !s
                .rt
                .frame_source(first)
                .map_err(|e| format!("original source: {e:?}"))?
                .snapshot()
                .closed
        {
            return Err("companion could not be qualified after original close".into());
        }
        // Reload/Find may have advanced B since the earlier before-close sample.
        // Sample both the original View and Source after A's retirement before
        // accepting a newer B paint. Invalidate's applied count is not a frame ACK.
        let repaint_cut = self.published_paint(s, None, deadline).await?;
        let frame = self
            .source
            .latest(SurfaceKind::View)
            .ok_or("missing companion VIEW before post-close refresh")?;
        let cut_sequence = frame.info.sequence;
        if repaint_cut.painted_generation != before_close.painted_generation
            || cut_sequence < repaint_cut.painted_sequence
            || !frame_matches(
                &frame,
                browser,
                repaint_cut.painted_generation,
                self.dims,
                self.expected_pixel(),
            )
        {
            return Err(
                "companion post-close repaint baseline changed identity or generation".into(),
            );
        }
        drop(frame);
        s.rt.probe_frame_control(browser, FrameProbeControl::InvalidateView)
            .map_err(|e| format!("companion InvalidateView: {e:?}"))?;
        let expected_applied = if timing_both {
            VIEW_COPY_SAMPLE_CAPACITY as u64 + 1
        } else {
            1
        };
        let paint = self
            .published_paint(
                s,
                Some((
                    repaint_cut.painted_generation,
                    cut_sequence,
                    expected_applied,
                )),
                deadline,
            )
            .await?;
        if let Some(find) = find {
            find.sample(s.rt, true)?;
        }
        if let Some(print) = s.out.window_print.as_mut() {
            print.observe_b_repaint(s.cx, s.rt, deadline).await?;
        }
        let capture =
            s.rt.frame_capture_diagnostics(browser)
                .map_err(|e| format!("companion capture: {e:?}"))?;
        if capture.requested_backend != Some(backend)
            || capture.observed_backend != Some(backend)
            || capture.failures != 0
        {
            return Err("companion did not capture with its original Required backend".into());
        }
        let weak = self.view.downgrade();
        let released = update(s.cx, self.root, |host, window, cx| {
            let owner = if self.separate {
                host.browser.take()
            } else {
                host.secondary.take()
            }
            .ok_or("companion View missing")?;
            let released = owner.update(cx, |view, cx| {
                view.release_image(window, cx).map_err(|e| e.to_string())?;
                Ok::<_, String>(view.diagnostics())
            })?;
            if self.separate {
                host.detached = true;
            } else {
                host.secondary_detached = true;
            }
            cx.notify();
            Ok::<_, String>(released)
        })??;
        if released.created_images == 0
            || released.created_images != released.released_images
            || released.last_error.is_some()
            || !s.composition.unregister(browser)
        {
            return Err("companion image/route retirement incomplete".into());
        }
        drop(self.view);
        let prior = update(s.cx, self.root, |host, _, _| host.renders)?;
        while self.handle.snapshot().state != SessionState::Closed {
            if Instant::now() >= deadline {
                return Err("companion Browser close timeout".into());
            }
            s.service(false, None).await?;
        }
        rendered(s.cx, self.root).await?;
        if app_update(s.cx, |app| {
            weak.upgrade().is_some() || !app.windows().contains(&self.root.into())
        })? || !update(s.cx, self.root, |host, _, _| {
            host.browser.is_none()
                && host.secondary.is_none()
                && if self.separate {
                    host.detached
                } else {
                    host.secondary_detached
                }
                && host.renders > prior
        })? || !self.source.snapshot().closed
        {
            return Err("detached companion or original host lifetime mismatch".into());
        }
        Ok(object(vec![
            ("browser", identity(browser)),
            ("paint_at_sampling_start", metrics_json(&initial)),
            ("paint_before_original_close", metrics_json(&before_close)),
            (
                "paint_before_post_close_refresh",
                metrics_json(&repaint_cut),
            ),
            (
                "source_sequence_before_post_close_refresh",
                cut_sequence.to_string(),
            ),
            ("paint_after_original_close", metrics_json(&paint)),
            ("released_view", metrics_json(&released)),
            ("backend", quote(backend)),
            ("closed_after_original", "true".into()),
            ("repaint_control_applied", expected_applied.to_string()),
            ("capture_maps", capture.maps.to_string()),
            ("capture_unmaps", capture.unmaps.to_string()),
            ("software_captures", capture.software_captures.to_string()),
            ("d3d11_captures", capture.d3d11_captures.to_string()),
        ]))
    }
}
