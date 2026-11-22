//! Passive, validation-only capture completion to actual GPUI VIEW paint submission.
//! No desktop capture, desktop foreground request, Win32 input injection, or Present claim.
//! The opt-in menu pressure variant uses only CEF-local fixture mouse events.
mod peer;
mod samples;
mod window_find;
pub(super) mod window_flush;
mod window_menu;
pub(super) mod window_pressure;
pub(super) mod window_print;
pub(super) mod window_source;
pub(super) mod windows_pair;
use rust_cef_gpui::{
    UiComposition,
    contracts::SessionState,
    test_support::{
        FrameProbeControl, MANUAL_PUMP_SAMPLE_CAPACITY, TimingDistribution,
        VIEW_COPY_SAMPLE_CAPACITY,
    },
};
use std::num::NonZeroUsize;

use super::*;

fn ns_from(anchor: Instant, event: Instant) -> Result<u64, String> {
    event
        .checked_duration_since(anchor)
        .and_then(|d| u64::try_from(d.as_nanos()).ok())
        .ok_or_else(|| "nonmonotonic or overflowing timing endpoints".into())
}

#[derive(Default)]
struct Pumps {
    count: u64,
    samples: Vec<u64>,
}

struct Scene<'a> {
    cx: &'a mut AsyncApp,
    rt: &'a mut Runtime,
    receiver: &'a UiWakeReceiver,
    stats: &'a Stats,
    root: WindowHandle<HostRoot>,
    out: &'a mut Outcome,
    composition: UiComposition,
}
impl Scene<'_> {
    async fn service(
        &mut self,
        require_wake: bool,
        pumps: Option<&mut Pumps>,
    ) -> Result<(), String> {
        let integrated = self.rt.mode() == ThreadMode::Integrated;
        let woke = if let Some(pumps) = pumps {
            // Same owner-only pump and wake paths as turn(); no timer dispatch.
            let interval = if integrated {
                Duration::from_millis(4)
            } else {
                Duration::from_secs(10)
            };
            let wake = matches!(wait(self.cx, self.receiver, interval).await?, Tick::Wake);
            if integrated {
                if APP_BORROW.with(Cell::get) {
                    return Err("sampled pump inside GPUI borrow".into());
                }
                let before = Instant::now();
                self.rt
                    .pump()
                    .map_err(|e| format!("sampled manual pump: {e:?}"))?;
                let ns = ns_from(before, Instant::now())?;
                pumps.count += 1;
                if pumps.samples.len() < MANUAL_PUMP_SAMPLE_CAPACITY {
                    pumps.samples.push(ns);
                }
            } else if !wake {
                return Err("Threaded paint timing did not receive a real wake".into());
            }
            wake
        } else {
            turn(self.cx, self.rt, self.receiver, require_wake).await?
        };
        if woke {
            self.out.wake_consumed += 1;
            self.dispatch()?;
        }
        self.stats.check()
    }

    fn dispatch(&mut self) -> Result<(), String> {
        let d = app_update(self.cx, |app| self.composition.dispatch(app))?;
        if d.removed != 0 || !d.failures.is_empty() {
            return Err(format!("timing dispatch: {d:?}"));
        }
        Ok(())
    }

    fn view_diag(&mut self, entity: &Entity<CefBrowser>) -> Result<ViewDiagnostics, String> {
        update(self.cx, self.root, |_, _, cx| entity.read(cx).diagnostics())
    }
}

fn identity(id: BrowserId) -> String {
    format!("[{},{}]", id.runtime().value(), id.local())
}

fn distribution(samples: impl IntoIterator<Item = u64>) -> String {
    TimingDistribution::of(samples).map_or_else(
        || "null".into(),
        |s| {
            object(vec![
                ("samples", s.samples.to_string()),
                ("p50_ns", s.p50_ns.to_string()),
                ("p95_ns", s.p95_ns.to_string()),
                ("max_ns", s.max_ns.to_string()),
            ])
        },
    )
}

fn dimensions(scale: f32) -> Result<(u32, u32), String> {
    if !scale.is_finite() || !(0.5..=5.0).contains(&scale) {
        return Err(format!("invalid timing scale {scale}"));
    }
    Ok(((320.0 * scale).ceil() as u32, (240.0 * scale).ceil() as u32))
}

const A_PIXEL: [u8; 4] = [126, 76, 26, 255];

fn frame_matches(
    frame: &OwnedFrame,
    browser: BrowserId,
    generation: u64,
    dimensions: (u32, u32),
    pixel: [u8; 4],
) -> bool {
    let (w, h) = dimensions;
    let center = 4 * ((h as usize / 2) * w as usize + w as usize / 2);
    frame.info.browser == browser
        && frame.info.surface == SurfaceKind::View
        && frame.info.surface_generation == generation
        && (frame.info.width, frame.info.height) == dimensions
        && frame.pixels.len() == w as usize * h as usize * 4
        && frame.pixels.get(center..center + 4) == Some(&pixel)
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
    let mut s = Scene {
        cx,
        rt,
        receiver,
        stats,
        root,
        out,
        composition: UiComposition::new(
            NonZeroUsize::new(if options.timing_profile_peer { 2 } else { 1 }).unwrap(),
        ),
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while s.rt.state() != RuntimeState::Ready {
        if Instant::now() >= deadline {
            return Err("timing Runtime readiness timeout".into());
        }
        s.service(false, None).await?;
    }
    let (scale, visible) = update(s.cx, root, |_, w, _| (w.scale_factor(), w.is_visible()))?;
    let dims = dimensions(scale)?;
    if !visible {
        return Err("timing fixture's owned window is hidden".into());
    }
    let profiles = if options.timing_profile_peer {
        let bundle = options
            .helper
            .parent()
            .ok_or("timing helper bundle missing")?;
        let cache_root = timing_cache_root(bundle, options.mode, options.backend);
        Some(
            dispatch::ProfilePair::open(s.cx, s.rt, s.receiver, s.stats, &cache_root, deadline)
                .await?,
        )
    } else {
        None
    };
    let handle =
        s.rt.create_browser(BrowserConfig {
            initial_url: if options.timing_window_source_pressure {
                rust_cef_gpui::test_support::PRESSURE_SOURCE_FIXTURE_URL
            } else if options.timing_window_pressure {
                rust_cef_gpui::test_support::PRESSURE_FIXTURE_URL
            } else {
                URL
            }
            .into(),
            profile: profiles.as_ref().map(|pair| pair.default),
            render_preference: options.backend,
            viewport: Viewport {
                width: 320,
                height: 240,
                scale,
            },
            options: Default::default(),
        })
        .map_err(|e| format!("timing Browser create: {e:?}"))?;
    let browser = handle.snapshot().browser;
    s.out.browser = Some(browser);
    let source =
        s.rt.frame_source(browser)
            .map_err(|e| format!("timing source: {e:?}"))?;
    let attachment =
        s.rt.attach_browser(&handle)
            .map_err(|e| format!("timing attach: {e:?}"))?;
    s.out.duplicate_refused = s.rt.attach_browser(&handle).is_err();
    if !s.out.duplicate_refused {
        return Err("duplicate timing attachment accepted".into());
    }
    let entity = update(s.cx, root, |host, w, cx| {
        let v = CefBrowser::new(attachment, w, cx).map_err(|e| e.to_string())?;
        s.composition
            .register(&v, w, cx)
            .map_err(|e| format!("timing register: {e:?}"))?;
        host.browser = Some(v.clone());
        cx.notify();
        Ok::<_, String>(v)
    })??;
    let companion = profiles
        .as_ref()
        .map(|pair| peer::Peer::open(&mut s, options, pair.named))
        .transpose()?;
    loop {
        if Instant::now() >= deadline {
            return Err("timing first VIEW timeout".into());
        }
        s.service(false, None).await?;
        rendered(s.cx, root).await?;
        let d = s.view_diag(&entity)?;
        if let Some(frame) = source.latest(SurfaceKind::View)
            && d.painted_sequence == frame.info.sequence
            && d.painted_sequence > 0
            && d.painted_generation == frame.info.surface_generation
            && frame_matches(&frame, browser, d.painted_generation, dims, A_PIXEL)
            && handle.snapshot().is_ready()
            && !handle.snapshot().navigation.is_loading
        {
            break;
        }
    }
    let companion_start = if let Some(view) = companion.as_ref() {
        Some(view.published_paint(&mut s, None, deadline).await?)
    } else {
        None
    };
    let owned_pair = if options.timing_profile_windows {
        let peer_root = companion
            .as_ref()
            .ok_or("timing window peer missing")?
            .root();
        Some(windows_pair::Pair::observe(s.cx, root, peer_root)?)
    } else {
        None
    };
    let expected_pid = std::process::id().to_string();
    let ready = options.gate.with_extension("ready");
    let temp = ready.with_extension("ready.pending");
    std::fs::write(&temp, &expected_pid).map_err(|e| e.to_string())?;
    std::fs::rename(temp, ready).map_err(|e| e.to_string())?;
    while !options.gate.is_file() {
        if Instant::now() >= deadline {
            return Err("timing observer did not release".into());
        }
        s.service(false, None).await?;
    }
    let renderer_pair =
        if options.timing_measure_both && options.backend == RenderPreference::D3d11Required {
            Some((
                owned_pair.as_ref().ok_or("renderer adapter pair missing")?,
                companion
                    .as_ref()
                    .ok_or("renderer adapter peer missing")?
                    .root(),
            ))
        } else {
            None
        };
    let renderer_before =
        renderer_pair.map(|(pair, peer)| pair.renderer_snapshot(s.cx, root, peer));
    let a = samples::Lane {
        root,
        view: &entity,
        source: source.as_ref(),
        browser,
        dims,
        pixel: A_PIXEL,
        window: owned_pair.as_ref().map(|pair| pair.gpui_a),
    }
    .measure(&mut s, deadline, false)
    .await?;
    let live =
        s.rt.frame_capture_diagnostics(browser)
            .map_err(|e| format!("timing capture diagnostics: {e:?}"))?;
    let requested = if options.backend == RenderPreference::D3d11Required {
        "D3d11"
    } else {
        "Software"
    };
    if live.requested_backend != Some(requested)
        || live.observed_backend != Some(requested)
        || live.failures != 0
    {
        return Err("timing used wrong backend or failed capture".into());
    }
    s.out.paint = Some(a.paint.clone());
    let peer_timing = if options.timing_measure_both {
        let pair = owned_pair.as_ref().ok_or("paired timing window missing")?;
        let peer = companion.as_ref().ok_or("paired timing B missing")?;
        pair.both_still_owned()?;
        let b = peer
            .timing_lane(pair.peer_gpui_window())
            .measure(&mut s, deadline, true)
            .await?;
        pair.both_still_owned()?;
        if !handle.snapshot().is_ready() || !peer.is_ready() {
            return Err("both original Browsers must remain ready through B sampling".into());
        }
        Some(object(b.fields(&s.out.paint_timing_peer_rows)))
    } else {
        None
    };
    let renderer_adapter = renderer_before
        .zip(renderer_pair)
        .map(|(before, (pair, peer))| {
            object(vec![
                ("before", before),
                ("after", pair.renderer_snapshot(s.cx, root, peer)),
            ])
        });
    let companion_end = if let Some(view) = companion.as_ref() {
        Some(view.published_paint(&mut s, None, deadline).await?)
    } else {
        None
    };
    let mut find = None;
    let pressure = if options.timing_window_pressure {
        let pair = owned_pair
            .as_ref()
            .ok_or("pressure requires original native window pair")?;
        let peer = companion
            .as_ref()
            .ok_or("pressure requires original B Browser")?;
        let b = peer.id();
        let menu_window = options.timing_window_menu_pressure.then_some(pair.gpui_a);
        if menu_window.is_some() {
            window_menu::Pending::configure(s.rt, options.timing_window_find_pressure)?;
        }
        let original_flush = if options.timing_window_menu_flush_pressure {
            let profile = profiles.as_ref().ok_or("missing original B profile")?.named;
            s.out.window_flush = Some(window_flush::Held::arm(s.rt, b, profile)?);
            loop {
                if let Some(cut) = s.out.window_flush.as_mut().unwrap().qualify_ready(s.rt)? {
                    break Some(cut);
                }
                if Instant::now() >= deadline {
                    return Err("original B flush callback timeout".into());
                }
                s.service(false, None).await?;
            }
        } else {
            None
        };
        if options.timing_extra_flush_fault {
            use rust_cef_gpui::runtime::CookieFlushStatus;
            // Deliberate independent default-Memory flush. Unlike the
            // original named B ticket its completion is not held.
            let extra =
                s.rt.flush_profile_cookies(s.rt.default_profile_handle())
                    .map_err(|e| format!("test extra default cookie flush: {e:?}"))?;
            loop {
                match extra.poll() {
                    CookieFlushStatus::Complete => break,
                    CookieFlushStatus::Pending if Instant::now() < deadline => {
                        s.service(false, None).await?
                    }
                    other => return Err(format!("test extra flush did not complete: {other:?}")),
                }
            }
            s.out
                .window_flush
                .as_mut()
                .ok_or("missing original B hold")?
                .observe_extra_completed(s.rt)?;
            return Err("test-induced failure after independent extra flush completed".into());
        }
        if options.timing_window_event_pressure {
            window_pressure::seed_from_live_reload(&mut s, browser, peer.handle(), deadline)
                .await?;
        }
        if options.timing_window_find_pressure {
            find =
                Some(window_find::Held::start(&mut s, peer.handle(), pair.gpui_b, deadline).await?);
        }
        let find_id = find.as_ref().map(|op| op.pending(s.rt)).transpose()?;
        let print_owners = options.timing_window_print_pressure.then_some([
            window_print::Owner {
                handle: &handle,
                view: &entity,
                root,
                window: pair.gpui_a,
            },
            window_print::Owner {
                handle: peer.handle(),
                view: peer.view(),
                root: peer.root(),
                window: pair.gpui_b,
            },
        ]);
        let w = window_pressure::close_first(
            &mut s,
            &handle,
            b,
            menu_window,
            original_flush,
            find_id,
            options.timing_window_event_pressure,
            print_owners,
            deadline,
        )
        .await?;
        if let Some(op) = s.out.window_print.as_mut() {
            op.first_closed(s.cx, s.rt, &w.command)?;
        }
        if let Some(op) = find.as_mut() {
            op.sample(s.rt, false)?;
        }
        if let Some(source) = s.out.window_source.as_mut() {
            source.ack_close()?;
            source.observed_closed(s.rt, false)?;
        }
        if let Some(flush) = s.out.window_flush.as_mut() {
            flush.observe_after_close(s.rt, false)?;
        }
        pair.both_still_owned()?;
        Some(w)
    } else {
        None
    };
    let weak = entity.downgrade();
    let (released, terminal_image_counts) = update(s.cx, root, |host, w, cx| {
        let original = host.browser.take().ok_or("host timing Entity missing")?;
        let result = original.update(cx, |v, cx| {
            if pressure.is_some() {
                let before = v.diagnostics();
                let changed = v.consume(w, cx).map_err(|e| e.to_string())?;
                let after = v.diagnostics();
                if changed
                    || before.binding_retired
                    || !after.binding_retired
                    || before.consumed_updates >= after.consumed_updates
                    || before.created_images <= before.released_images
                    || after.created_images != before.created_images
                    || after.released_images != after.created_images
                {
                    return Err(
                        "original view did not consume Closed and release its live image".into(),
                    );
                }
                let counts = [
                    before.created_images,
                    before.released_images,
                    after.released_images,
                ];
                Ok::<_, String>((after, Some(counts)))
            } else {
                v.release_image(w, cx).map_err(|e| e.to_string())?;
                Ok((v.diagnostics(), None))
            }
        });
        host.detached = true;
        cx.notify();
        result
    })??;
    if released.created_images != released.released_images || released.last_error.is_some() {
        return Err("timing image release was not balanced".into());
    }
    s.out.released_view = Some(released);
    if !s.composition.unregister(browser) {
        return Err("original timing route was not registered at retirement".into());
    }
    if pressure.is_some() {
        // Waiting for Closed may consume a shared wake while A needs its
        // one explicit terminal consume. Reconcile the remaining B route now.
        if s.composition.len() != 1 {
            return Err("expected only B after pressured A consume".into());
        }
        s.dispatch()?;
    }
    drop(entity);
    let renders = update(s.cx, root, |host, _, _| host.renders)?;
    rendered(s.cx, root).await?;
    if app_update(s.cx, |app| {
        weak.upgrade().is_some() || !app.windows().contains(&root.into())
    })? {
        return Err("timing View or original window did not retire as expected".into());
    }
    s.out.detached_alive = update(s.cx, root, |host, _, _| {
        host.detached && host.renders > renders
    })?;
    loop {
        let state = handle.snapshot().state;
        if state == SessionState::Closed
            || (!options.timing_profile_peer && state == SessionState::Closing)
        {
            break;
        }
        if Instant::now() >= deadline {
            return Err("timing View release did not close Browser".into());
        }
        s.service(false, None).await?;
    }
    s.out.pressure_closed_before_view_release = terminal_image_counts.is_some();
    s.out.view_release_observed_closing = pressure.is_none();
    // A's real panel is retired on its original still-owned GPUI window,
    // after the Browser and View are closed, before destroying that HWND.
    if let Some(op) = s.out.window_print.as_mut() {
        op.reap_first(s.cx, s.rt).await?;
    }
    let mut native_destroy_yields = 0u32;
    if let Some(pair) = &owned_pair {
        update(s.cx, root, |_, window, _| window.remove_window())?;
        let b_root = companion
            .as_ref()
            .ok_or("timing window peer missing")?
            .root();
        let windows_ok = app_update(s.cx, |app| {
            !app.windows().contains(&root.into()) && app.windows().contains(&b_root.into())
        })?;
        if !windows_ok || s.composition.len() != 1 {
            return Err(
                "original window or peer route was not independently retired/preserved".into(),
            );
        }
        // GPUI removes the route first; fixed Windows GPUI queues DestroyWindow
        // after dialog-owner idle. Wait on B without touching A or restarting CEF.
        while !pair.a_hwnd_gone() {
            if Instant::now() >= deadline {
                return Err("original A native window did not retire before the deadline".into());
            }
            native_destroy_yields += 1;
            rendered(s.cx, b_root).await?;
        }
        pair.report_after_a_removed(native_destroy_yields)?;
    }
    if let Some(companion) = companion {
        let peer = companion.id();
        let view = companion
            .retire_after_original(
                &mut s,
                browser,
                companion_start.ok_or("missing companion baseline paint")?,
                companion_end.ok_or("missing companion end-of-sampling paint")?,
                requested,
                deadline,
                find.as_mut(),
                options.timing_measure_both,
            )
            .await?;
        let profiles = profiles
            .as_ref()
            .ok_or("missing timed profile pair")?
            .report(s.rt, browser, peer)?;
        let mut peer_fields = vec![("profiles", profiles), ("view", view)];
        if let Some(timing) = peer_timing {
            peer_fields.push(("two_live_windows_b_timed_after_a", timing));
        }
        if let Some(matched) = renderer_adapter {
            peer_fields.push(("renderer_adapter", matched));
        }
        if let Some(pair) = owned_pair {
            peer_fields.push((
                "window_pair",
                pair.report_after_a_removed(native_destroy_yields)?,
            ));
        }
        if let Some(window_pressure::FirstClose {
            command,
            title,
            menu,
            peer_flush,
        }) = pressure
        {
            while !s.stats.window_pressure.both_closed()? {
                if Instant::now() >= deadline {
                    return Err("missing actual B terminal event".into());
                }
                s.service(false, None).await?;
            }
            if let Some(op) = find.take() {
                peer_fields.push(("window_find", op.retire(s.rt)?));
            }
            if let Some(op) = s.out.window_print.as_mut() {
                peer_fields.push(("window_print", op.reap_second(s.cx, s.rt).await?));
            }
            if let Some(source) = s.out.window_source.as_mut() {
                source.observed_closed(s.rt, true)?;
            }
            if let Some(flush) = s.out.window_flush.as_mut() {
                flush.observe_after_close(s.rt, true)?;
            }
            let images = terminal_image_counts.ok_or("missing A terminal image observation")?;
            peer_fields.push((
                "window_pressure",
                s.stats
                    .window_pressure
                    .receipt(command, title, images, menu, peer_flush)?,
            ));
        }
        s.out.paint_timing_peer = Some(object(peer_fields));
    }
    let mut measured = vec![
        (
            "scope",
            quote("copy-completion-to-gpui-paint-image-success"),
        ),
        ("observer_released", "true".into()),
        ("desktop_pixels_observed", "false".into()),
        ("swapchain_present_observed", "false".into()),
        ("backend", quote(requested)),
        // Same reason as scale_factor: keep 1.0 a float, not the integer 1.
        ("scale", format!("{scale:?}")),
        ("software_captures", live.software_captures.to_string()),
        ("d3d11_captures", live.d3d11_captures.to_string()),
        ("maps", live.maps.to_string()),
        ("unmaps", live.unmaps.to_string()),
    ];
    measured.extend(a.fields(&s.out.paint_timing_rows));
    s.out.paint_timing_detail = Some(object(measured));
    Ok(())
}
