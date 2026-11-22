//! Explicit validation-only Source -> real GPUI viewer window slice.
//! Synthetic CEF context-menu button messages, no OS input/foreground operation.
use super::pointer_probe::{PointerProbe, WindowInfo};
use super::*;
use gpui::{Focusable, point};
use rust_cef_gpui::{
    ContextMenuConfig, ContextMenuServices, MenuUiUpdate, SourceUiStatus, SourceViewOptions,
    SourceViewServices,
    contracts::SessionState,
    menu::{ContextMenuResponse, MenuContextLimits, MenuLimits},
    source::{SourceOutcome, SourcePhase, SourceViewLimits},
};
use std::num::NonZeroUsize;
#[link(name = "user32")]
unsafe extern "system" {
    fn SetForegroundWindow(hwnd: isize) -> i32;
    fn GetForegroundWindow() -> isize;
}
fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}
fn page() -> String {
    let html = "<body style='margin:0;font:24px Arial;background:rgb(26,76,126)'><span>menu bridge selection</span><script>document.title='menu-bridge-ready';document.addEventListener('selectionchange',()=>{document.title=window.getSelection().toString().trim()==='menu bridge selection'?'menu-bridge-selected':'menu-bridge-empty';});</script></body>";
    let mut uri = String::from("data:text/html,");
    for b in html.bytes() {
        if b.is_ascii_alphanumeric() {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}
fn new_file(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    file.write_all(text.as_bytes()).map_err(|e| e.to_string())
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
fn status(s: &SourceUiStatus) -> String {
    object(vec![
        ("retired", bool_json(s.retired)),
        ("operations", s.operations.to_string()),
        ("windows", s.live_windows.to_string()),
        ("created", s.created.to_string()),
        ("window_closed", s.window_closed.to_string()),
        ("entities_released", s.entities_released.to_string()),
        ("payloads_retired", s.payloads_retired.to_string()),
        ("acknowledged", s.acknowledged.to_string()),
        (
            "first_error",
            s.first_error
                .as_ref()
                .map_or_else(|| "null".into(), |e| quote(&format!("{e:?}"))),
        ),
        (
            "outcome",
            s.last_operation
                .as_ref()
                .map_or_else(|| "null".into(), |q| quote(&format!("{:?}", q.outcome))),
        ),
    ])
}
fn no_input(probe: &PointerProbe) -> Result<(), String> {
    let q = probe.snapshot()?;
    if q.down != 0 || q.up != 0 || !q.messages.is_empty() {
        return Err("unexpected owned OS input during source viewer slice".into());
    }
    Ok(())
}
async fn observer(
    cx: &mut AsyncApp,
    rt: &mut Runtime,
    receiver: &UiWakeReceiver,
    options: &Options,
    stage: u32,
    metadata: String,
) -> Result<(), String> {
    new_file(
        &options
            .report
            .with_file_name(format!("source-viewer-stage-{stage}.json")),
        &metadata,
    )?;
    if stage == 0 {
        new_file(
            &options.gate.with_extension("ready"),
            &std::process::id().to_string(),
        )?;
    }
    new_file(
        &options
            .gate
            .with_extension(format!("source-viewer-{stage}.ready")),
        &std::process::id().to_string(),
    )?;
    let release = options
        .gate
        .with_extension(format!("source-viewer-{stage}.release"));
    let until = Instant::now() + Duration::from_secs(20);
    while !release.is_file() {
        if options.gate.is_file() {
            return Err(format!(
                "observer released without validating source viewer stage {stage}"
            ));
        }
        if Instant::now() >= until {
            return Err(format!("source viewer observer stage {stage} timeout"));
        }
        turn(cx, rt, receiver, false).await?;
    }
    if std::fs::read_to_string(release)
        .map_err(|e| e.to_string())?
        .trim()
        != std::process::id().to_string()
    {
        return Err("source viewer observer PID mismatch".into());
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
    visibility::require_unlocked_session()?;
    let ready = Instant::now() + Duration::from_secs(25);
    while rt.state() != RuntimeState::Ready {
        if Instant::now() >= ready {
            return Err("source Runtime readiness timeout".into());
        }
        turn(cx, rt, receiver, false).await?;
        stats.check()?;
    }
    let model_limits = MenuLimits {
        max_entries: nz(32),
        max_label_bytes: nz(256),
        max_total_label_bytes: nz(2048),
    };
    let context_limits = MenuContextLimits {
        max_frame_id_bytes: nz(512),
        max_text_bytes: nz(4096),
        max_suggestions: nz(5),
        max_total_bytes: nz(8192),
    };
    rt.configure_context_menus(ContextMenuConfig {
        capacity: nz(1),
        model_limits,
        context_limits,
    })
    .map_err(|e| format!("source menu config: {e:?}"))?;
    rt.configure_source_views(SourceViewLimits {
        capacity: nz(1),
        max_source_bytes: nz(8192),
    })
    .map_err(|e| format!("source config: {e:?}"))?;
    let scale = update(cx, root, |_, window, _| window.scale_factor())?;
    if !scale.is_finite() || !(0.5..=5.0).contains(&scale) {
        return Err("invalid original SourceViewer viewport scale".into());
    }
    // Start at the owned window's actual scale; don't force a wrong-sized
    // accelerated first frame and then treat the capture fault as transient.
    let expected = ((320. * scale).ceil() as u32, (240. * scale).ceil() as u32);
    let handle = rt
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
        .map_err(|e| format!("source create: {e:?}"))?;
    let browser = handle.snapshot().browser;
    out.browser = Some(browser);
    let attachment = rt
        .attach_browser(&handle)
        .map_err(|e| format!("source attachment: {e:?}"))?;
    out.duplicate_refused = rt.attach_browser(&handle).is_err();
    let entity = update(cx, root, |host, window, cx| {
        let entity = CefBrowser::new(attachment, window, cx)?;
        host.browser = Some(entity.clone());
        let focus = entity.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
        Ok::<_, rust_cef_gpui::ViewError>(entity)
    })?
    .map_err(|e| format!("source view construction: {e:?}"))?;
    let weak = entity.downgrade();
    let mut entity = Some(entity);
    let mut menus = ContextMenuServices::new(nz(1), nz(1), model_limits, context_limits);
    let mut sources = SourceViewServices::new(
        nz(1),
        SourceViewOptions {
            max_operations: nz(1),
            max_source_bytes: nz(8192),
            bounds: Bounds::new(point(px(680.), px(40.)), size(px(420.), px(340.))),
            focus: false,
        },
    );
    let mut original_probe = None;
    let mut viewer_probe = None;
    let mut phase = 0u32;
    let result = async {
        let target = update(cx, root, |_, window, cx| {
            menus.register(entity.as_ref().unwrap(), window, cx)
        })?
        .map_err(|e| format!("source menu UI registration: {e:?}"))?;
        let registration = Arc::new(
            rt.bind_context_menu(target.clone())
                .map_err(|e| format!("source native registration: {e:?}"))?,
        );
        update(cx, root, |_, window, cx| {
            sources.register(
                entity.as_ref().unwrap(),
                window,
                &menus,
                registration.clone(),
                cx,
            )
        })?
        .map_err(|e| format!("source viewer registration: {e:?}"))?;
        original_probe = Some(app_update(cx, |_| PointerProbe::install(TITLE))??);
        // Approved ordinary foreground request: menu readiness only holds while
        // the owner window is active, and this driver never activates by itself.
        // Only our own HWND is asked for; no ATTACH, no ALT, no foreign window.
        if options.menu_foreground_approved {
            if APP_BORROW.with(std::cell::Cell::get) {
                return Err("source foreground inside GPUI borrow".into());
            }
            visibility::require_unlocked_session()?;
            let probe = original_probe.as_ref().ok_or("source probe missing")?;
            if !probe.is_foreground()? {
                let facts = probe.info()?;
                let hwnd = facts.hwnd as isize;
                let mut confirmed = false;
                for attempt in 0..3 {
                    if attempt > 0 {
                        std::thread::sleep(Duration::from_millis(600));
                    }
                    if unsafe { SetForegroundWindow(hwnd) } != 0
                        && unsafe { GetForegroundWindow() } == hwnd
                    {
                        confirmed = true;
                        break;
                    }
                }
                if !confirmed {
                    return Err("source approved foreground denied".into());
                }
            }
            if !probe.is_foreground()? {
                return Err("source approved foreground not confirmed".into());
            }
            // Auditable receipt for the single approved activation this driver
            // performs; create_new so a rerun cannot overwrite the first record.
            new_file(
                &options
                    .report
                    .with_file_name("source-viewer-foreground.json"),
                &object(vec![
                    ("schema", "1".into()),
                    ("kind", quote("source-approved-foreground")),
                    ("pid", std::process::id().to_string()),
                    ("hwnd", probe.info()?.hwnd.to_string()),
                    ("foreground", bool_json(true)),
                    ("desktop_input_sent", bool_json(false)),
                ]),
            )?;
        }
        let view_ready = Instant::now() + Duration::from_secs(25);
        loop {
            if Instant::now() >= view_ready {
                return Err(menu_fixture_readiness_timeout(
                    cx,
                    root,
                    "SourceViewer",
                    menus.status(browser).map_err(|e| e.to_string())?.ready,
                )?);
            }
            if turn(cx, rt, receiver, false).await? {
                out.wake_consumed += 1;
                update(cx, root, |_, window, cx| {
                    entity
                        .as_ref()
                        .unwrap()
                        .update(cx, |view, cx| view.consume(window, cx))
                })?
                .map_err(|e| format!("source view consume: {e:?}"))?;
            }
            stats.check()?;
            no_input(original_probe.as_ref().unwrap())?;
            let d = update(cx, root, |_, _, cx| {
                entity.as_ref().unwrap().read(cx).diagnostics()
            })?;
            if menus.status(browser).map_err(|e| e.to_string())?.ready
                && d.paint_calls > 0
                && d.painted_sequence > 0
                && (d.frame_width, d.frame_height) == expected
                && d.last_error.is_none()
                && handle.snapshot().navigation.title == "menu-bridge-ready"
                && !handle.snapshot().navigation.is_loading
            {
                out.paint = Some(d);
                break;
            }
        }
        rendered(cx, root).await?;
        phase = 1;
        rt.probe_menu_source_open(browser)
            .map_err(|e| format!("source native context fixture: {e:?}"))?;
        let until = Instant::now() + Duration::from_secs(20);
        let mut submitted = None;
        let document = loop {
            if Instant::now() >= until {
                return Err(format!(
                    "source delivery/display timeout; menu={:?}; source={:?}; ui={:?}",
                    registration.status(),
                    rt.source_view_diagnostics(),
                    sources.status(browser)
                ));
            }
            if turn(cx, rt, receiver, false).await? {
                out.wake_consumed += 1;
            }
            stats.check()?;
            no_input(original_probe.as_ref().unwrap())?;
            let mut response = None;
            let report = update(cx, root, |_, window, cx| {
                menus.dispatch(browser, window, cx, |update, _, _| {
                    if let MenuUiUpdate::Open {
                        presentation,
                        response: port,
                        ..
                    } = update
                    {
                        response = Some((presentation.request, port));
                    }
                    Ok(())
                })
            })?
            .map_err(|e| e.to_string())?;
            if report.presenter_error.is_some() || report.cancellation_error.is_some() {
                return Err(format!("source menu UI dispatch: {report:?}"));
            }
            if let Some((id, port)) = response {
                if submitted.is_some() || id.browser != browser {
                    return Err("duplicate or foreign source Select delivery".into());
                }
                port.submit(id, ContextMenuResponse::Select { command_id: 132 })
                    .map_err(|e| format!("source Select: {e:?}"))?;
                submitted = Some(id);
            }
            // Top-level App context, not an Entity/window update. Selection submission
            // above likewise occurred only AFTER the original update borrow returned.
            app_update(cx, |app| sources.dispatch(app))?;
            let menu = registration.status();
            if let Some(e) = menu.first_failure.as_deref() {
                return Err(format!("source menu failure: {e}"));
            }
            let native = rt.source_view_diagnostics();
            if native.faulted || native.first_failure.is_some() {
                return Err(format!("source backend failure: {native:?}"));
            }
            let docs = app_update(cx, |app| sources.validation_documents(browser, app))?
                .map_err(|e| format!("source document observation: {e:?}"))?;
            if docs.len() == 1
                && docs[0].4 > 0
                && native.native_live == 0
                && native.host_live == 1
                && native.active.len() == 1
                && native.active[0].phase == SourcePhase::Presented
                && native.active[0].callback_received
                && native.active[0].operation == docs[0].0
                && submitted == Some(native.active[0].menu)
                && menu.opened == 1
                && menu.published == 1
                && menu.actions_submitted == 1
                && menu.native_dismissed == 1
                && menu.post_failures == 0
            {
                break docs[0];
            }
        };
        if document.1 != 392 || document.2 != 14904330143978622504 || !document.3 {
            // Record what was actually observed: the bare message made this
            // failure indistinguishable from any other content drift.
            return Err(format!(
                "original-frame source plaintext bytes/hash/witness mismatch: bytes={} hash={} witness={} (expected 392 / 14904330143978622504 / true)",
                document.1, document.2, document.3
            ));
        }
        viewer_probe = Some(app_update(cx, |_| PointerProbe::install("Page source"))??);
        let original = original_probe.as_ref().unwrap().info()?;
        let viewer = viewer_probe.as_ref().unwrap().info()?;
        let a_thread = original_probe.as_ref().unwrap().owner_thread()?;
        let b_thread = viewer_probe.as_ref().unwrap().owner_thread()?;
        if original.hwnd == viewer.hwnd || original.pid != viewer.pid || a_thread != b_thread {
            return Err("source window original-owner association mismatch".into());
        }
        let menu_id = submitted.ok_or("original source menu identity missing")?;
        let prefix = format!("{},{}", browser.runtime().value(), browser.local());
        let menu = format!("[{prefix},{}]", menu_id.sequence);
        let operation = format!("[{prefix},{}]", document.0.sequence);
        let before = sources
            .status(browser)
            .map_err(|e| format!("source UI status: {e:?}"))?;
        if before.first_error.is_some()
            || before.created != 1
            || before.operations != 1
            || before.live_windows != 1
            || before.acknowledged != 0
        {
            return Err(format!("source window pre-release accounting: {before:?}"));
        }
        observer(
            cx,
            rt,
            receiver,
            options,
            0,
            object(vec![
                ("schema", "1".into()),
                ("pid", std::process::id().to_string()),
                ("stage", "0".into()),
                ("original", fact(&original, a_thread)),
                ("viewer", fact(&viewer, b_thread)),
                ("source_bytes", document.1.to_string()),
                ("source_hash", document.2.to_string()),
                ("source_witness", bool_json(document.3)),
                ("renders", document.4.to_string()),
                ("operation", operation.clone()),
                ("source_menu", menu.clone()),
                ("view_pixels", format!("[{},{}]", expected.0, expected.1)),
                ("view_scale", scale.to_string()),
                ("source_ui", status(&before)),
                ("native_hold_released", "true".into()),
                ("host_hold_live", "true".into()),
                ("menu_widget_tested", "false".into()),
                ("os_input_sent", "false".into()),
                ("foreground_requested", "false".into()),
            ]),
        )
        .await?;
        no_input(original_probe.as_ref().unwrap())?;
        no_input(viewer_probe.as_ref().unwrap())?;
        phase = 2;
        // Atlas retirement is explicit. No sources.dispatch/shutdown/control call
        // runs between the actual Entity drop and the automatic close proof below.
        out.released_view = Some(
            update(cx, root, |host, window, cx| {
                let view = entity.as_ref().unwrap();
                view.update(cx, |view, cx| view.release_image(window, cx))?;
                host.browser = None;
                host.detached = true;
                cx.notify();
                Ok::<_, rust_cef_gpui::ViewError>(view.read(cx).diagnostics())
            })?
            .map_err(|e| format!("source original View image release: {e:?}"))?,
        );
        drop(entity.take());
        // Last local Entity handle dropped is not actual release: a previous
        // GPUI frame may retain it. Observe session state only after Weak=None.
        rendered(cx, root).await?;
        let until = Instant::now() + Duration::from_secs(8);
        loop {
            let q = sources
                .status(browser)
                .map_err(|e| format!("source automatic close status: {e:?}"))?;
            if weak.upgrade().is_none()
                && q.retired
                && q.operations == 0
                && q.live_windows == 0
                && q.acknowledged == 1
            {
                break;
            }
            if Instant::now() >= until {
                return Err(format!(
                    "automatic original-View release source closure not observed: {q:?}"
                ));
            }
            turn(cx, rt, receiver, false).await?;
            stats.check()?;
        }
        out.view_release_observed_closing = matches!(
            handle.snapshot().state,
            SessionState::Closing | SessionState::Closed
        );
        let after = sources
            .status(browser)
            .map_err(|e| format!("source final UI status: {e:?}"))?;
        if after.first_error.is_some()
            || after.window_closed != 1
            || after.entities_released != 1
            || after.payloads_retired != 1
            || after.last_operation.as_ref().is_none_or(|q| {
                q.outcome != Some(SourceOutcome::Cancelled)
                    || q.operation != document.0
                    || q.menu != menu_id
                    || q.first_failure.is_some()
                    || q.first_close_failure.is_some()
            })
        {
            return Err(format!("source retirement facts mismatch: {after:?}"));
        }
        let lease = target
            .lease
            .snapshot()
            .map_err(|e| format!("source original lease: {e:?}"))?;
        if !lease.retired || !lease.is_drained() {
            return Err("source original lease survived Entity retirement".into());
        }
        let mut retired_viewer = viewer_probe.take().unwrap();
        app_update(cx, |_| retired_viewer.retire())??;
        out.detached_alive = update(cx, root, |host, _, _| {
            host.browser.is_none() && host.detached && host.renders > 1
        })?;
        observer(
            cx,
            rt,
            receiver,
            options,
            1,
            object(vec![
                ("schema", "1".into()),
                ("pid", std::process::id().to_string()),
                ("stage", "1".into()),
                (
                    "original",
                    fact(&original_probe.as_ref().unwrap().info()?, a_thread),
                ),
                ("former_viewer", fact(&viewer, b_thread)),
                ("source_menu", menu.clone()),
                ("operation", operation.clone()),
                ("source_ui", status(&after)),
                ("weak_entity_released", bool_json(weak.upgrade().is_none())),
                ("lease_retired", bool_json(lease.retired)),
                ("automatic_view_release_cleanup", "true".into()),
                ("source_dispatch_used_after_owner_drop", "false".into()),
                ("host_alive", bool_json(out.detached_alive)),
            ]),
        )
        .await?;
        let freed = sources
            .unregister(browser)
            .map_err(|e| format!("source drained unregister: {e:?}"))?;
        if freed.operations != 0 || freed.live_windows != 0 {
            return Err("source route reclaimed before drainage".into());
        }
        menus.unregister(browser).map_err(|e| e.to_string())?;
        out.source_viewer_verified = true;
        out.source_viewer_detail = Some(object(vec![
            ("success", "true".into()),
            ("phase", "3".into()),
            ("source_bytes", document.1.to_string()),
            ("source_hash", document.2.to_string()),
            ("source_ui", status(&after)),
            ("source_menu", menu),
            ("operation", operation),
            ("real_source_getter", "true".into()),
            ("real_gpui_viewer", "true".into()),
            ("automatic_view_release_cleanup", "true".into()),
            ("route_unregistered_after_drain", "true".into()),
            ("menu_widget_tested", "false".into()),
            ("full_view_source_matrix", "false".into()),
        ]));
        Ok::<_, String>(())
    }
    .await;
    // One bounded cleanup path. It never replaces the primary failure or converts
    // a failed observer into a success; no OS input/activation/retry occurs here.
    if entity.is_some() {
        let _ = update(cx, root, |host, window, cx| {
            if let Some(view) = entity.as_ref() {
                let _ = view.update(cx, |view, cx| view.release_image(window, cx));
            }
            host.browser = None;
            host.detached = true;
            cx.notify();
        });
        drop(entity.take());
        let _ = rendered(cx, root).await;
    }
    if !out.source_viewer_verified {
        let _ = app_update(cx, |app| sources.shutdown(app));
        let until = Instant::now() + Duration::from_secs(8);
        loop {
            let _ = app_update(cx, |app| sources.dispatch(app));
            if sources
                .status(browser)
                .is_ok_and(|s| s.operations == 0 && s.live_windows == 0)
            {
                break;
            }
            if Instant::now() >= until {
                break;
            }
            if turn(cx, rt, receiver, false).await.is_err() {
                break;
            }
        }
        out.source_viewer_detail = Some(object(vec![
            ("success", "false".into()),
            ("phase", phase.to_string()),
            (
                "source_ui",
                sources
                    .status(browser)
                    .map_or_else(|_| "null".into(), |s| status(&s)),
            ),
            (
                "backend",
                quote(&format!("{:?}", rt.source_view_diagnostics())),
            ),
        ]));
    }
    let mut cleanup_error = None;
    for probe in [&mut viewer_probe, &mut original_probe] {
        if let Some(probe) = probe.as_mut() {
            if let Err(e) = app_update(cx, |_| probe.retire()).and_then(|r| r) {
                cleanup_error.get_or_insert(e);
            }
        }
    }
    if result.is_ok() {
        if let Some(e) = cleanup_error {
            return Err(e);
        }
    }
    result
}
