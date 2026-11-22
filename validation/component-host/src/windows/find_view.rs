//! Explicit real original-View Find result/automatic retirement slice only.
//! Synthetic CEF menu input, never OS input or a foreground request.
use super::pointer_probe::{PointerProbe, WindowInfo};
use super::*;
use gpui::Focusable;
use rust_cef_gpui::{
    ContextMenuConfig, ContextMenuServices, FindServices, FindUiStatus, MenuUiUpdate,
    contracts::SessionState,
    find::{FindLimits, FindOutcome, FindPhase, FindResult},
    menu::{ContextMenuRequestId, ContextMenuResponse, MenuContextLimits, MenuLimits},
};
use std::num::NonZeroUsize;
#[link(name = "user32")]
unsafe extern "system" {
    fn SetForegroundWindow(hwnd: isize) -> i32;
    fn GetForegroundWindow() -> isize;
}
const FOUND: &str = "Find: 1 match, active 0";
const PENDING: &str = "Find: 1 match, active 0; next search pending";
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
fn new_file(p: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(p)
        .map_err(|e| e.to_string())?;
    f.write_all(text.as_bytes()).map_err(|e| e.to_string())
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
fn status(s: &FindUiStatus) -> String {
    object(vec![
        ("retired", bool_json(s.retired)),
        ("retire_submitted", bool_json(s.retire_submitted)),
        ("active", bool_json(s.active)),
        ("started", s.started.to_string()),
        ("finished", s.finished.to_string()),
        ("delivered", s.delivered.to_string()),
        ("faulted", bool_json(s.faulted)),
        (
            "first_error",
            s.first_error
                .as_ref()
                .map_or_else(|| "null".into(), |e| quote(&format!("{e:?}"))),
        ),
        (
            "operation",
            s.last
                .as_ref()
                .map_or_else(|| "null".into(), |q| q.operation.sequence.to_string()),
        ),
        (
            "outcome",
            s.last
                .as_ref()
                .map_or_else(|| "null".into(), |q| quote(&format!("{:?}", q.outcome))),
        ),
    ])
}
fn id(browser: BrowserId, sequence: u64) -> String {
    format!(
        "[{},{},{}]",
        browser.runtime().value(),
        browser.local(),
        sequence
    )
}
fn no_input(p: &PointerProbe) -> Result<(), String> {
    let q = p.snapshot()?;
    if q.down != 0 || q.up != 0 || !q.messages.is_empty() {
        return Err("unexpected owned OS input during Find View slice".into());
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
            .with_file_name(format!("find-view-stage-{stage}.json")),
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
            .with_extension(format!("find-view-{stage}.ready")),
        &std::process::id().to_string(),
    )?;
    let release = options
        .gate
        .with_extension(format!("find-view-{stage}.release"));
    let until = Instant::now() + Duration::from_secs(20);
    while !release.is_file() {
        if options.gate.is_file() {
            return Err(format!(
                "observer released without validating Find View stage {stage}"
            ));
        }
        if Instant::now() >= until {
            return Err("Find View observer timeout".into());
        }
        turn(cx, rt, receiver, false).await?;
    }
    if std::fs::read_to_string(release)
        .map_err(|e| e.to_string())?
        .trim()
        != std::process::id().to_string()
    {
        return Err("Find View observer PID mismatch".into());
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
            return Err("Find Runtime readiness timeout".into());
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
    .map_err(|e| format!("Find menus configure: {e:?}"))?;
    rt.configure_finds(FindLimits {
        capacity: nz(1),
        max_query_bytes: nz(64),
    })
    .map_err(|e| format!("Find configure: {e:?}"))?;
    let scale = update(cx, root, |_, window, _| window.scale_factor())?;
    if !scale.is_finite() || !(0.5..=5.0).contains(&scale) {
        return Err("invalid original FindView viewport scale".into());
    }
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
        .map_err(|e| format!("Find create: {e:?}"))?;
    let browser = handle.snapshot().browser;
    out.browser = Some(browser);
    let attachment = rt
        .attach_browser(&handle)
        .map_err(|e| format!("Find attachment: {e:?}"))?;
    out.duplicate_refused = rt.attach_browser(&handle).is_err();
    let entity = update(cx, root, |host, window, cx| {
        let entity = CefBrowser::new(attachment, window, cx)?;
        host.browser = Some(entity.clone());
        let focus = entity.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
        Ok::<_, rust_cef_gpui::ViewError>(entity)
    })?
    .map_err(|e| format!("Find Entity: {e:?}"))?;
    let weak = entity.downgrade();
    let mut entity = Some(entity);
    let mut menus = ContextMenuServices::new(nz(1), nz(1), model_limits, context_limits);
    let mut finds = FindServices::new(nz(1), nz(64));
    let mut probe = None;
    let mut phase = 0;
    let mut first_result: Option<FindResult> = None;
    let mut identity_record = None;
    let mut first_footer_renders = 0;
    let result = async {
        let target = update(cx, root, |_, window, cx| {
            menus.register(entity.as_ref().unwrap(), window, cx)
        })?
        .map_err(|e| format!("Find menu UI register: {e:?}"))?;
        let registration = Arc::new(
            rt.bind_context_menu(target.clone())
                .map_err(|e| format!("Find native bind: {e:?}"))?,
        );
        update(cx, root, |_, window, cx| {
            finds.register(
                entity.as_ref().unwrap(),
                window,
                &menus,
                registration.clone(),
                cx,
            )
        })?
        .map_err(|e| format!("Find original View register: {e:?}"))?;
        probe = Some(app_update(cx, |_| PointerProbe::install(TITLE))??);
        // Approved ordinary foreground request: menu readiness only holds while
        // the owner window is active, and this driver never activates by itself.
        if options.menu_foreground_approved {
            if APP_BORROW.with(std::cell::Cell::get) {
                return Err("find foreground inside GPUI borrow".into());
            }
            visibility::require_unlocked_session()?;
            let probe_ref = probe.as_ref().ok_or("find probe missing")?;
            if !probe_ref.is_foreground()? {
                let hwnd = probe_ref.info()?.hwnd as isize;
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
                    return Err("find approved foreground denied".into());
                }
            }
            if !probe_ref.is_foreground()? {
                return Err("find approved foreground not confirmed".into());
            }
            new_file(
                &options.report.with_file_name("find-view-foreground.json"),
                &object(vec![
                    ("schema", "1".into()),
                    ("kind", quote("find-approved-foreground")),
                    ("pid", std::process::id().to_string()),
                    ("hwnd", probe_ref.info()?.hwnd.to_string()),
                    ("foreground", bool_json(true)),
                    ("desktop_input_sent", bool_json(false)),
                ]),
            )?;
        }
        let until = Instant::now() + Duration::from_secs(25);
        loop {
            if Instant::now() >= until {
                return Err(menu_fixture_readiness_timeout(
                    cx,
                    root,
                    "FindView",
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
                .map_err(|e| format!("Find consume: {e:?}"))?;
            }
            stats.check()?;
            no_input(probe.as_ref().unwrap())?;
            let d = update(cx, root, |_, _, cx| {
                entity.as_ref().unwrap().read(cx).diagnostics()
            })?;
            if menus.status(browser).map_err(|e| e.to_string())?.ready
                && d.paint_calls > 0
                && d.painted_sequence > 0
                && d.created_images > 0
                && (d.frame_width, d.frame_height) == expected
                && d.last_error.is_none()
                && handle.snapshot().navigation.title == "menu-bridge-ready"
                && !handle.snapshot().navigation.is_loading
            {
                out.paint = Some(d);
                break;
            }
        }
        let mut menu_ids: Vec<ContextMenuRequestId> = Vec::with_capacity(3);
        for opening in 1..=3 {
            phase = opening;
            let until = Instant::now() + Duration::from_secs(20);
            if opening == 1 {
                rt.probe_menu_bridge_open(browser, false)
            } else {
                rt.probe_menu_find_open(browser)
            }
            .map_err(|e| format!("Find CEF context input: {e:?}"))?;
            let mut submitted = None;
            loop {
                if Instant::now() >= until {
                    return Err(format!(
                        "Find View phase {opening} timeout: {:?} / {:?}",
                        registration.status(),
                        finds.status(browser)
                    ));
                }
                if turn(cx, rt, receiver, false).await? {
                    out.wake_consumed += 1;
                    update(cx, root, |_, window, cx| {
                        entity
                            .as_ref()
                            .unwrap()
                            .update(cx, |view, cx| view.consume(window, cx))
                    })?
                    .map_err(|e| format!("Find running consume: {e:?}"))?;
                }
                stats.check()?;
                no_input(probe.as_ref().unwrap())?;
                let mut response = None;
                let report = update(cx, root, |_, window, cx| {
                    menus.dispatch(browser, window, cx, |event, _, _| {
                        if let MenuUiUpdate::Open {
                            presentation,
                            response: port,
                            ..
                        } = event
                        {
                            if opening > 1 && presentation.context.selection.len() != 21 {
                                return Err(EngineError::InvalidRequest);
                            }
                            response = Some((presentation.request, port));
                        }
                        Ok(())
                    })
                })?
                .map_err(|e| format!("Find menu dispatch: {e:?}"))?;
                if let Some(e) = report.presenter_error.or(report.cancellation_error) {
                    return Err(format!("Find menu presenter: {e:?}"));
                }
                if let Some((id, port)) = response {
                    if submitted.is_some()
                        || id.browser != browser
                        || menu_ids
                            .last()
                            .is_some_and(|prior| prior.sequence >= id.sequence)
                    {
                        return Err("duplicate, stale or foreign Find Select".into());
                    }
                    port.submit(
                        id,
                        ContextMenuResponse::Select {
                            command_id: if opening == 1 { 117 } else { 130 },
                        },
                    )
                    .map_err(|e| format!("Find Select: {e:?}"))?;
                    submitted = Some(id);
                }
                let updates = app_update(cx, |app| finds.dispatch(app))?;
                for update in updates {
                    if update.browser != browser {
                        return Err("Find result routed to foreign View".into());
                    }
                    let q = update.status;
                    if q.first_failure.is_some() {
                        return Err(format!("Find UI receipt failed: {q:?}"));
                    }
                    if q.outcome == Some(FindOutcome::Result) {
                        let r = q.last_result.as_ref().ok_or("Find final result absent")?;
                        if opening != 2
                            || first_result.is_some()
                            || submitted != Some(q.menu)
                            || q.operation.browser != browser
                            || r.operation != q.operation
                            || !r.final_update
                            || r.matches != 1
                            || r.active_ordinal != 0
                            || q.operation.sequence != 1
                            || q.query_bytes != 21
                        {
                            return Err("actual Find result mismatch".into());
                        }
                        first_result = Some(r.clone());
                        first_footer_renders = update_host_footer(cx, root, FOUND).await?;
                    }
                }
                let menu = registration.status();
                if let Some(e) = menu.first_failure.as_deref() {
                    return Err(format!("Find menu execution: {e}"));
                }
                let d = rt.find_diagnostics();
                if d.faulted || d.first_failure.is_some() {
                    return Err(format!("Find backend failure: {d:?}"));
                }
                let ui = finds
                    .status(browser)
                    .map_err(|e| format!("Find UI status: {e:?}"))?;
                let done = if opening == 1 {
                    handle.snapshot().navigation.title == "menu-bridge-selected"
                } else if opening == 2 {
                    first_result.is_some() && d.finished == 1 && d.pending == 0
                } else {
                    ui.active
                        && d.started == 2
                        && d.finished == 1
                        && d.pending == 1
                        && ui.last.as_ref().is_some_and(|q| {
                            submitted == Some(q.menu)
                                && q.operation.browser == browser
                                && q.operation.sequence == 2
                                && q.query_bytes == 21
                                && q.issued
                                && q.last_result.is_none()
                                && q.outcome.is_none()
                        })
                        && rt.diagnostics().tasks.pending == 0
                };
                if let Some(id) = submitted
                    && done
                    && menu.opened == opening as u64
                    && menu.published == opening as u64
                    && menu.actions_submitted == opening as u64
                    && menu.native_dismissed == opening as u64
                    && menu.post_failures == 0
                {
                    menu_ids.push(id);
                    break;
                }
            }
        }
        update_host_footer(cx, root, PENDING).await?;
        let ui = finds
            .status(browser)
            .map_err(|e| format!("Find pending UI: {e:?}"))?;
        if ui.first_error.is_some()
            || ui.faulted
            || ui.started != 2
            || ui.finished != 1
            || !ui.active
            || registration.status().native_dismissed != 3
        {
            return Err(format!("Find pending/menu terminal facts: {ui:?}"));
        }
        let first = first_result
            .as_ref()
            .ok_or("missing original final Find result")?;
        let repeat = ui
            .last
            .as_ref()
            .ok_or("missing original pending Find receipt")?;
        let repeat_id = repeat.operation;
        let repeat_menu = repeat.menu;
        if repeat_menu != menu_ids[2]
            || repeat_id.browser != browser
            || repeat_id.sequence != 2
            || !repeat.issued
            || repeat.last_result.is_some()
            || repeat.outcome.is_some()
        {
            return Err("Find original repeat changed before observer".into());
        }
        let identity = object(vec![
            (
                "menus",
                format!(
                    "[{}]",
                    menu_ids
                        .iter()
                        .map(|m| id(m.browser, m.sequence))
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            ),
            (
                "first",
                id(first.operation.browser, first.operation.sequence),
            ),
            ("repeat", id(repeat_id.browser, repeat_id.sequence)),
            ("first_cef_identifier", first.cef_identifier.to_string()),
            ("first_final", bool_json(first.final_update)),
            ("query_bytes", repeat.query_bytes.to_string()),
        ]);
        identity_record = Some(identity.clone());
        let original = probe.as_ref().unwrap().info()?;
        let thread = probe.as_ref().unwrap().owner_thread()?;
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
                ("original", fact(&original, thread)),
                ("find_ui", status(&ui)),
                ("first_result", "true".into()),
                ("matches", first.matches.to_string()),
                ("active_ordinal", first.active_ordinal.to_string()),
                ("view_scale", scale.to_string()),
                ("view_pixels", format!("[{},{}]", expected.0, expected.1)),
                ("identities", identity),
                ("first_footer_renders", first_footer_renders.to_string()),
                ("footer", quote(PENDING)),
                ("os_input_sent", "false".into()),
                ("foreground_requested", "false".into()),
            ]),
        )
        .await?;
        // Critical automatic interval: no Find dispatch/shutdown/cancel/retire call
        // from actual original Entity drop through genuine Browser/receipt closure.
        out.released_view = Some(
            update(cx, root, |host, window, cx| {
                let view = entity.as_ref().unwrap();
                view.update(cx, |view, cx| view.release_image(window, cx))?;
                host.browser = None;
                host.detached = true;
                host.fault_label = None;
                cx.notify();
                Ok::<_, rust_cef_gpui::ViewError>(view.read(cx).diagnostics())
            })?
            .map_err(|e| format!("Find original image retirement: {e:?}"))?,
        );
        drop(entity.take());
        rendered(cx, root).await?;
        let until = Instant::now() + Duration::from_secs(8);
        loop {
            let ui = finds
                .status(browser)
                .map_err(|e| format!("Find automatic retirement status: {e:?}"))?;
            if weak.upgrade().is_none()
                && ui.retired
                && ui.retire_submitted
                && !ui.active
                && ui.finished == 2
                && handle.snapshot().state == SessionState::Closed
                && rt.find_diagnostics().pending == 0
            {
                break;
            }
            if Instant::now() >= until {
                return Err(format!(
                    "automatic original Find View retirement not observed: {ui:?}"
                ));
            }
            turn(cx, rt, receiver, false).await?;
            stats.check()?;
        }
        let ui = finds
            .status(browser)
            .map_err(|e| format!("Find final status: {e:?}"))?;
        let q = ui.last.as_ref().ok_or("Find terminal receipt missing")?;
        if q.operation != repeat_id
            || q.menu != repeat_menu
            || q.last_result.is_some()
            || q.phase != FindPhase::Finished
            || q.outcome != Some(FindOutcome::Cancelled)
            || q.first_failure.is_some()
            || ui.first_error.is_some()
            || ui.faulted
        {
            return Err(format!("Find final operation mismatch: {ui:?}"));
        }
        let lease = target
            .lease
            .snapshot()
            .map_err(|e| format!("Find original lease: {e:?}"))?;
        if !lease.retired || !lease.is_drained() {
            return Err("Find original lease not retired".into());
        }
        out.view_release_observed_closing = true;
        out.detached_alive = update(cx, root, |host, _, _| {
            host.browser.is_none() && host.detached && host.renders > first_footer_renders
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
                ("original", fact(&probe.as_ref().unwrap().info()?, thread)),
                ("find_ui", status(&ui)),
                ("retired_menu", id(q.menu.browser, q.menu.sequence)),
                (
                    "retired_operation",
                    id(q.operation.browser, q.operation.sequence),
                ),
                ("weak_entity_released", bool_json(weak.upgrade().is_none())),
                ("lease_retired", bool_json(lease.retired)),
                (
                    "browser_closed",
                    bool_json(handle.snapshot().state == SessionState::Closed),
                ),
                ("automatic_view_release_cleanup", "true".into()),
                ("find_dispatch_used_after_owner_drop", "false".into()),
                ("host_alive", bool_json(out.detached_alive)),
            ]),
        )
        .await?;
        finds
            .unregister(browser)
            .map_err(|e| format!("Find unregister: {e:?}"))?;
        if finds.status(browser).is_ok() {
            return Err("Find route capacity not reclaimed".into());
        }
        phase = 4;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = app_update(cx, |app| finds.shutdown(app));
        drop(entity.take());
        let _ = rendered(cx, root).await;
    }
    if let Some(mut p) = probe {
        if let Err(e) = app_update(cx, |_| p.retire()).and_then(|r| r) {
            if result.is_ok() {
                return Err(e);
            }
        }
    }
    out.find_view_verified = result.is_ok();
    out.find_view_detail = Some(object(vec![
        ("success", bool_json(result.is_ok())),
        ("phase", phase.to_string()),
        ("first_result", bool_json(first_result.is_some())),
        (
            "identities",
            identity_record.unwrap_or_else(|| "null".into()),
        ),
        ("first_footer_renders", first_footer_renders.to_string()),
        ("automatic_view_release_cleanup", bool_json(result.is_ok())),
        ("find_dispatch_used_after_owner_drop", "false".into()),
        ("route_unregistered_after_drain", bool_json(result.is_ok())),
        ("real_view_owner", "true".into()),
        ("menu_widget_tested", "false".into()),
        ("full_find_matrix", "false".into()),
    ]));
    result
}
async fn update_host_footer(
    cx: &mut AsyncApp,
    root: WindowHandle<HostRoot>,
    text: &'static str,
) -> Result<u64, String> {
    let before = update(cx, root, |host, _, cx| {
        let before = host.renders;
        host.fault_label = Some(text);
        cx.notify();
        before
    })?;
    rendered(cx, root).await?;
    let (after, label) = update(cx, root, |host, _, _| (host.renders, host.fault_label))?;
    if after <= before || label != Some(text) {
        return Err("actual host Find footer did not render".into());
    }
    Ok(after)
}
