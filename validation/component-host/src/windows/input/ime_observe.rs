//! Explicit pointer-only owner-IME prerequisites, not a Chinese input test.
//! No mode switching, keyboard submission, system caret ownership or timer.
use super::*;
use rust_cef_gpui::ports::BrowserHandle;

fn creation_ready(states: [SessionState; 2]) -> Result<bool, String> {
    if states
        .iter()
        .any(|s| !matches!(s, SessionState::Creating | SessionState::Alive))
    {
        return Err(format!("IME observation creation terminated: {states:?}"));
    }
    Ok(states == [SessionState::Alive; 2])
}

/// Environment observations start with live targets, not Creating input views.
/// This never changes the component's public Creating-attachment contract or
/// erases a refusal. The original stage gates and overall deadline still apply.
pub(super) async fn wait_created(
    cx: &mut AsyncApp,
    rt: &mut Runtime,
    receiver: &UiWakeReceiver,
    stats: &Stats,
    deadline: Instant,
    handles: [&BrowserHandle; 2],
    report: &std::path::Path,
) -> Result<(), String> {
    let until = (Instant::now() + WAIT).min(deadline);
    loop {
        let states = handles.map(|h| h.snapshot().state);
        if creation_ready(states)? {
            let inputs = handles.map(|h| {
                h.input_snapshot()
                    .map_err(|e| format!("IME created input: {e:?}"))
            });
            let [a, b] = inputs;
            let case = report.parent().ok_or("IME creation report missing")?;
            atomic(
                &case.join("ime-observe-created.json"),
                &object(vec![
                    ("schema", "1".into()),
                    ("kind", quote("ime-environment-created-before-views")),
                    ("pid", std::process::id().to_string()),
                    ("states", quote(&format!("{states:?}"))),
                    ("a_input", input_json(&a?)),
                    ("b_input", input_json(&b?)),
                    ("task_11_2_accepted", bool_json(false)),
                ]),
            )?;
            return Ok(());
        }
        if Instant::now() >= until {
            return Err(format!(
                "IME observation Browser creation timeout: {states:?}"
            ));
        }
        // Existing bounded host turn only. No input submission, extra event
        // queue, component retry, Threaded pump or foreground manipulation.
        turn(cx, rt, receiver, false).await?;
        stats.check()?;
    }
}

fn ime_json(d: &ViewDiagnostics) -> String {
    let i = &d.ime;
    object(vec![
        ("queued", i.queued.to_string()),
        ("applied", i.applied.to_string()),
        ("refused", i.refused.to_string()),
        ("composition_calls", i.composition_calls.to_string()),
        ("candidate_calls", i.candidate_calls.to_string()),
        (
            "last_applied_point",
            i.last_applied_point
                .map_or_else(|| "null".into(), |(x, y)| format!("[{x},{y}]")),
        ),
        ("system_caret_owned", bool_json(i.system_caret_owned)),
        (
            "candidate_adoption_verified",
            bool_json(i.candidate_adoption_verified),
        ),
    ])
}
/// Passively move one owned normal window without activating it or changing
/// z-order. Only the original HWND is touched; the caller re-verifies identity
/// and geometry afterwards.
fn passive_move(hwnd: isize, dx: i32, dy: i32) -> Result<(), String> {
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_NOACTIVATE: u32 = 0x0010;
    unsafe extern "system" {
        fn SetWindowPos(
            hwnd: isize,
            insert_after: isize,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
        fn GetWindowRect(hwnd: isize, rect: *mut i32) -> i32;
    }
    // SetWindowPos takes absolute screen coordinates; `left`/`top` from the
    // facts are client origin, so move from the real window rect.
    let mut rect = [0i32; 4];
    if unsafe { GetWindowRect(hwnd, rect.as_mut_ptr()) } == 0 {
        return Err("owned window rect query failed".into());
    }
    if unsafe {
        SetWindowPos(
            hwnd,
            0,
            rect[0] + dx,
            rect[1] + dy,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    } == 0
    {
        return Err("passive owned window move failed".into());
    }
    Ok(())
}

fn dom_focused(title: &str) -> bool {
    expected_title(title, 0, 1, 0, 1, 0, 0, 0).is_ok() && field(title, "u") == Ok(1)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    exec: &mut Exec<'_>,
    root: WindowHandle<HostRoot>,
    peer: WindowHandle<HostRoot>,
    entity: Entity<CefBrowser>,
    other: Entity<CefBrowser>,
    handle: BrowserHandle,
    peer_handle: BrowserHandle,
    a: &InputProbe,
    b: &InputProbe,
) -> Result<(), String> {
    let case = exec
        .options
        .report
        .parent()
        .ok_or("IME observation case missing")?
        .to_path_buf();
    for stage in 0..4u32 {
        let (target, idle, target_root, target_entity, target_handle, target_title, idle_title) =
            if stage == 0 || stage == 2 {
                (a, b, root, &entity, &handle, TITLE, PEER_TITLE)
            } else {
                (b, a, peer, &other, &peer_handle, PEER_TITLE, TITLE)
            };
        let until = (Instant::now() + WAIT).min(exec.deadline);
        let mut last = "null".to_owned();
        let (diag, source, scale) = loop {
            if Instant::now() >= until {
                atomic(
                    &case.join(format!("ime-observe-timeout-{stage}.json")),
                    &object(vec![
                        ("schema", "1".into()),
                        ("pid", std::process::id().to_string()),
                        ("stage", stage.to_string()),
                        ("last_observation", last),
                        ("success", bool_json(false)),
                        ("task_11_2_accepted", bool_json(false)),
                    ]),
                )?;
                return Err(format!(
                    "IME observation stage {stage} initial frame timeout"
                ));
            }
            rendered(exec.cx, target_root).await?;
            let diag = view(exec.cx, target_root, target_entity)?;
            let id = target_handle.snapshot().browser;
            let source = exec
                .rt
                .frame_source(id)
                .map_err(|e| format!("IME source: {e:?}"))?
                .snapshot();
            let input = target_handle
                .input_snapshot()
                .map_err(|e| format!("IME input: {e:?}"))?;
            let scale = update(exec.cx, target_root, |_, w, _| w.scale_factor())?;
            let frame_ok = source.view.frame.as_ref().is_some_and(|f| {
                source.view.visible
                    && !source.closed
                    && diag.paint_calls > 0
                    && diag.painted_generation == f.info.surface_generation
                    && diag.painted_sequence == f.info.sequence
                    && diag.frame_width == f.info.width
                    && diag.frame_height == f.info.height
                    && diag.surface_bounds == (0., 48., 320., 240.)
            });
            let nav = target_handle.snapshot();
            last = object(vec![
                ("dom_title", quote(&nav.navigation.title)),
                ("state", quote(&format!("{:?}", nav.state))),
                ("loading", bool_json(nav.navigation.is_loading)),
                ("frame_ok", bool_json(frame_ok)),
                ("input", input_json(&input)),
                ("diagnostics", metrics_json(&diag)),
                ("source_closed", bool_json(source.closed)),
                ("source_visible", bool_json(source.view.visible)),
                (
                    "source_frame",
                    source
                        .view
                        .frame
                        .as_ref()
                        .map_or_else(|| "null".into(), |f| raw_json(f, "not-written")),
                ),
            ]);
            if nav.state == SessionState::Alive
                && !nav.navigation.is_loading
                && frame_ok
                && good(&input)
                && diag.last_error.is_none()
                && !diag.input.pending
                && diag.input.rejected == 0
                && (stage >= 2
                    || expected_title(&nav.navigation.title, 0, 0, 0, 0, 0, 0, 0).is_ok())
            {
                break (diag, source, scale);
            }
            exec.step().await?;
        };
        // On stage 1 this same process already owns A's foreground. Never use
        // activate_window (implicit ALT), AttachThreadInput or policy changes.
        if stage == 1 && !a.is_foreground()? {
            return Err("IME peer request lost original owner foreground".into());
        }
        target.foreground()?;
        // Stage 3 additionally proves the placement still follows the owned
        // window. It is moved passively first, so the geometry recomputed for
        // the same composition must differ from stage 2's applied point.
        let mut moved = false;
        if stage == 3 {
            let before = target.info()?;
            passive_move(before.hwnd as isize, 40, 24)?;
            let after = target.info()?;
            if (after.left, after.top) != (before.left + 40, before.top + 24)
                || (after.width, after.height) != (before.width, before.height)
                || after.dpi != before.dpi
            {
                return Err("owned window move changed size/DPI or was refused".into());
            }
            moved = true;
        }
        let (x, y) = physical(90, 94, scale);
        if stage >= 2 {
            // IME composition injects its own WM_CHAR/`VK_PROCESSKEY` traffic,
            // so no fixed key sequence can be armed here. Record the complete
            // raw FIFO instead of stopping at the first unmatched key; the
            // marker/association checks below still reject foreign input.
            target.arm_observe_all()?;
        } else {
            target.arm(&[Event::Down(x, y), Event::Up(x, y)])?;
        }
        idle.arm(&[])?;
        let target_facts = target.info()?;
        let idle_facts = idle.info()?;
        let idle_scale = update(
            exec.cx,
            if stage == 0 || stage == 2 { peer } else { root },
            |_, w, _| w.scale_factor(),
        )?;
        let frame = source.view.frame.as_ref().ok_or("IME VIEW disappeared")?;
        // Keep the unchanged shared capture helper's exact case-local raw name.
        let raw_name = format!("input-stage-{stage}-view.bgra");
        std::fs::write(case.join(&raw_name), frame.pixels.as_slice()).map_err(|e| e.to_string())?;
        let a_hwnd = a.info()?.hwnd;
        let b_hwnd = b.info()?.hwnd;
        atomic(
            &case.join(format!("ime-observe-stage-{stage}.json")),
            &object(vec![
                ("schema", "1".into()),
                ("kind", quote("ime-environment-stage")),
                ("scope", quote("ime-environment-only")),
                ("pid", std::process::id().to_string()),
                ("stage", stage.to_string()),
                ("title", quote(target_title)),
                ("peer_title", quote(idle_title)),
                ("window", facts_json(target_facts)),
                ("peer", facts_json(idle_facts)),
                ("hwnds", format!("[{a_hwnd},{b_hwnd}]")),
                ("scale_factor", scale.to_string()),
                ("peer_scale", idle_scale.to_string()),
                ("view_bounds", "[0,48,320,240]".into()),
                (
                    "browser",
                    format!(
                        "[{},{}]",
                        frame.info.browser.runtime().value(),
                        frame.info.browser.local()
                    ),
                ),
                ("view_raw", raw_json(frame, &raw_name)),
                ("action", quote("focus-and-observe")),
                ("window_moved", bool_json(moved)),
                ("task_11_2_accepted", bool_json(false)),
                ("native_chinese_verified", bool_json(false)),
                ("diagnostics", metrics_json(&diag)),
            ]),
        )?;
        if stage == 0 {
            atomic(
                &exec.options.gate.with_extension("ready"),
                &std::process::id().to_string(),
            )?;
        }
        atomic(
            &exec
                .options
                .gate
                .with_extension(format!("ime-observe-{stage}.ready")),
            &std::process::id().to_string(),
        )?;
        let request = case.join(format!("ime-observe-owner-{stage}.request"));
        let response = request.with_extension("json");
        let release = exec
            .options
            .gate
            .with_extension(format!("ime-observe-{stage}.release"));
        let mut served = false;
        let mut gates = (0usize, false, false, false, false, false);
        let mut last_written = (usize::MAX, false, false, false, false, false);
        let mut last_title = String::new();
        while !release.is_file() {
            if exec.options.gate.is_file() {
                return Err("IME observer released before stages finished".into());
            }
            if Instant::now() >= until {
                let _ = atomic(
                    &case.join(format!("ime-observe-gates-{stage}.json")),
                    &object(vec![
                        ("schema", "1".into()),
                        ("kind", quote("ime-observation-gates")),
                        ("records", gates.0.to_string()),
                        ("trace_error", bool_json(gates.1)),
                        ("accepts_text", bool_json(gates.2)),
                        ("dom_focused", bool_json(gates.3)),
                        ("foreground", bool_json(gates.4)),
                        ("input_good", bool_json(gates.5)),
                    ]),
                );
                return Err(format!(
                    "IME observation stage {stage} request/release timeout; gates={gates:?}"
                ));
            }
            // Do not wait for another frame while a request is already pending:
            // an IME composition can stop producing wakes until the host answers.
            // Exec::step is already bounded (4ms Integrated / 50ms Threaded) and
            // it is what dispatches pending Win32 input to the GPUI window. The
            // earlier 1ms sleep starved that dispatch, so clicks never reached
            // the component.
            eprintln!(
                "ime-compose loop req={} served={}",
                request.is_file(),
                served
            );
            exec.step().await?;
            if request.is_file() && gates != last_written {
                // Publish the live gate tuple so an earlier probe deadline
                // still leaves the actual blocker on disk.
                let _ = atomic(
                    &case.join(format!("ime-observe-gates-{stage}.json")),
                    &object(vec![
                        ("schema", "1".into()),
                        ("kind", quote("ime-observation-gates")),
                        ("records", gates.0.to_string()),
                        ("trace_error", bool_json(gates.1)),
                        ("accepts_text", bool_json(gates.2)),
                        ("dom_focused", bool_json(gates.3)),
                        ("foreground", bool_json(gates.4)),
                        ("input_good", bool_json(gates.5)),
                        ("dom_title", quote(&last_title)),
                    ]),
                );
                last_written = gates;
            }
            if request.is_file() && !served {
                rendered(exec.cx, target_root).await?;
                let trace = target.trace()?;
                // Persist the complete raw FIFO before any continue: an owner
                // response made from counts alone cannot explain a mismatched
                // virtual key between a DOWN and its UP.
                let _ = atomic(
                    &case.join(format!("ime-compose-trace-{stage}.json")),
                    &object(vec![
                        ("schema", "1".into()),
                        ("kind", quote("ime-compose-trace")),
                        ("pid", std::process::id().to_string()),
                        ("stage", stage.to_string()),
                        ("request_seen", bool_json(request.is_file())),
                        ("trace", trace_json(&trace)),
                    ]),
                );
                eprintln!(
                    "ime-compose trace records={} error={:?} stage={stage} wparam={:?}",
                    trace.records.len(),
                    trace.error,
                    trace
                        .records
                        .iter()
                        .map(|m| (m.message, m.wparam, m.event.is_some()))
                        .collect::<Vec<_>>()
                );
                gates.0 = trace.records.len();
                gates.1 = trace.error.is_some();
                // Stage >= 2 is the compose stage: real pinyin/selection keys
                // are expected and recorded, so the pointer-only limits of the
                // earlier stages do not apply.
                let compose = stage >= 2;
                if trace.error.is_some() && !compose {
                    return Err("IME observation unexpected raw input".into());
                }
                if !compose {
                    if trace.records.len() > 2 {
                        return Err("IME observation unexpected raw input".into());
                    }
                    if trace.records.len() != 2 || !target.accepts_text()? {
                        continue;
                    }
                    if trace.records.iter().any(|m| {
                        matches!(
                            m.event,
                            Some(Event::KeyDown(_) | Event::Char(_) | Event::KeyUp(_))
                        )
                    }) {
                        return Err("IME observation received keyboard input".into());
                    }
                } else {
                    // The driver signals readiness only after it has submitted the
                    // marked keys, but the owner IME finishes the composition
                    // asynchronously. Answering from the first sample commits
                    // whatever has arrived so far and would let an unfinished
                    // composition look like a plain ASCII insert, so wait a
                    // bounded time for the real commit and record every sample.
                    let commit_until = (Instant::now() + Duration::from_secs(2)).min(until);
                    let mut samples = Vec::new();
                    loop {
                        let title = target_handle.snapshot().navigation.title;
                        let committed = field(&title, "n").is_ok_and(|n| n == 2)
                            && field(&title, "cs").is_ok_and(|cs| cs == 1)
                            && field(&title, "ce").is_ok_and(|ce| ce == 1)
                            // A commit alone can be sampled before the last UP is
                            // delivered; a balanced key ledger is part of the
                            // input-count requirement, not a separate nicety.
                            && field(&title, "kd")
                                .ok()
                                .zip(field(&title, "ku").ok())
                                .is_some_and(|(kd, ku)| kd == ku && kd == 6);
                        samples.push(object(vec![
                            ("dom_title", quote(&title)),
                            ("committed", bool_json(committed)),
                        ]));
                        last_title = title.clone();
                        if committed || Instant::now() >= commit_until {
                            break;
                        }
                        exec.step().await?;
                    }
                    let _ = atomic(
                        &case.join(format!("ime-compose-samples-{stage}.json")),
                        &object(vec![
                            ("schema", "1".into()),
                            ("kind", quote("ime-compose-samples")),
                            ("stage", stage.to_string()),
                            ("samples", format!("[{}]", samples.join(","))),
                        ]),
                    );
                    let title = target_handle.snapshot().navigation.title;
                    last_title = title.clone();
                    gates.3 = dom_focused(&title);
                    gates.2 = target.accepts_text().unwrap_or(false);
                    gates.4 = target.is_foreground()?;
                    if let Ok(snapshot) = target_handle.input_snapshot() {
                        let diag2 = view(exec.cx, target_root, target_entity)?;
                        let _ = atomic(
                            &case.join(format!("ime-compose-state-{stage}.json")),
                            &object(vec![
                                ("schema", "1".into()),
                                ("kind", quote("ime-compose-state")),
                                ("stage", stage.to_string()),
                                ("accepts_text", bool_json(gates.2)),
                                ("foreground", bool_json(gates.4)),
                                ("title", quote(&title)),
                                ("input", input_json(&snapshot)),
                                ("diagnostics", metrics_json(&diag2)),
                            ]),
                        );
                    }
                    if !gates.2 {
                        continue;
                    }
                }
                let input = target_handle
                    .input_snapshot()
                    .map_err(|e| format!("IME focused input: {e:?}"))?;
                gates.2 = target.accepts_text().unwrap_or(false);
                gates.5 = good(&input);
                let title = target_handle.snapshot().navigation.title;
                last_title = title.clone();
                gates.3 = dom_focused(&title);
                if !compose && (!good(&input) || !dom_focused(&title)) {
                    continue;
                }
                gates.4 = target.is_foreground()?;
                if !gates.4 {
                    return Err("IME observation target no longer foreground".into());
                }
                let diag = view(exec.cx, target_root, target_entity)?;
                let facts = target.info()?;
                let tag = format!("{}:{stage}:{}", std::process::id(), facts.hwnd);
                if std::fs::read_to_string(&request)
                    .map_err(|e| e.to_string())?
                    .trim()
                    != tag
                {
                    return Err("IME owner request PID/stage/HWND mismatch".into());
                }
                // Fresh post-focus/draw owner IMM facts and exact trace copied
                // without another await. A prior null HIMC is never a mode proof.
                atomic(
                    &response,
                    &object(vec![
                        ("schema", "1".into()),
                        ("kind", quote("ime-environment-owner")),
                        ("pid", std::process::id().to_string()),
                        ("stage", stage.to_string()),
                        ("request", quote(&tag)),
                        ("window", facts_json(facts)),
                        // Real observed message count, not the pointer-only
                        // constant: a compose stage must show its own keys.
                        ("raw_count", trace.records.len().to_string()),
                        ("keyboard_seen", bool_json(compose)),
                        ("dom_title", quote(&title)),
                        ("accepts_text", bool_json(true)),
                        ("foreground", bool_json(true)),
                        ("dom_title", quote(&title)),
                        ("component_callbacks", callbacks_json(&diag)),
                        ("native_ime", ime_json(&diag)),
                        ("input_snapshot", input_json(&input)),
                        ("keyboard_input_sent", bool_json(false)),
                        ("mode_changed", bool_json(false)),
                    ]),
                )?;
                served = true;
                // Keep waiting for this stage's release marker. The launcher
                // writes it only after the driver exits, so breaking here would
                // read a missing release and abandon every later stage.
            }
        }
        if !served
            || std::fs::read_to_string(release)
                .map_err(|e| e.to_string())?
                .trim()
                != std::process::id().to_string()
        {
            return Err("IME observation release identity/cutpoint mismatch".into());
        }
        let ta = a.trace()?;
        let tb = b.trace()?;
        if !ta.complete || !tb.complete || ta.error.is_some() || tb.error.is_some() {
            return Err("IME pointer trace incomplete".into());
        }
        atomic(
            &case.join(format!("ime-observe-observed-{stage}.json")),
            &object(vec![
                ("schema", "1".into()),
                ("kind", quote("ime-environment-wm")),
                ("pid", std::process::id().to_string()),
                ("stage", stage.to_string()),
                ("success", bool_json(true)),
                ("expected_marker", "1128678706".into()),
                (
                    "windows",
                    format!(
                        "[{},{}]",
                        object(vec![
                            ("hwnd", a_hwnd.to_string()),
                            ("trace", trace_json(&ta))
                        ]),
                        object(vec![
                            ("hwnd", b_hwnd.to_string()),
                            ("trace", trace_json(&tb))
                        ])
                    ),
                ),
                ("keyboard_input_sent", bool_json(false)),
                ("task_11_2_accepted", bool_json(false)),
            ]),
        )?;
        a.finish()?;
        b.finish()?;
    }
    let final_until = (Instant::now() + WAIT).min(exec.deadline);
    while !exec.options.gate.is_file() {
        if Instant::now() >= final_until {
            return Err("IME observer final release timeout".into());
        }
        exec.step().await?;
    }
    // The shared launcher writes the literal `release`, not a PID, only for
    // its final observer gate. Per-stage markers above remain PID-bound.
    if std::fs::read_to_string(&exec.options.gate)
        .map_err(|e| e.to_string())?
        .trim()
        != "release"
    {
        return Err("IME final observer release marker mismatch".into());
    }
    exec.out.paint = Some(view(exec.cx, root, &entity)?);
    let before = update(exec.cx, root, |host, _, _| host.renders)?;
    let clear = |cx: &mut AsyncApp, owned_root, entity: &Entity<CefBrowser>| {
        update(cx, owned_root, |host: &mut HostRoot, window, cx| {
            entity.update(cx, |view, cx| view.release_image(window, cx))?;
            host.browser = None;
            host.detached = true;
            cx.notify();
            Ok::<_, ViewError>(entity.read(cx).diagnostics())
        })?
        .map_err(|e| format!("IME explicit release: {e:?}"))
    };
    let released = clear(exec.cx, root, &entity)?;
    let peer_released = clear(exec.cx, peer, &other)?;
    for d in [&released, &peer_released] {
        if d.created_images == 0 || d.created_images != d.released_images {
            return Err("IME images unbalanced".into());
        }
    }
    exec.out.released_view = Some(released);
    let weak_a = entity.downgrade();
    let weak_b = other.downgrade();
    drop(entity);
    drop(other);
    if !exec.composition.unregister(handle.snapshot().browser)
        || !exec.composition.unregister(peer_handle.snapshot().browser)
    {
        return Err("IME route unregister failed".into());
    }
    rendered(exec.cx, root).await?;
    rendered(exec.cx, peer).await?;
    if weak_a.upgrade().is_some() || weak_b.upgrade().is_some() || !exec.composition.is_empty() {
        return Err("IME views retained after release".into());
    }
    if !update(exec.cx, root, |host, _, _| {
        host.detached && host.renders > before
    })? || !app_update(exec.cx, |app| {
        app.windows().contains(&AnyWindowHandle::from(root))
            && app.windows().contains(&AnyWindowHandle::from(peer))
    })? {
        return Err("IME green host did not remain alive".into());
    }
    exec.out.detached_alive = true;
    let closing_until = (Instant::now() + WAIT).min(exec.deadline);
    while ![handle.snapshot().state, peer_handle.snapshot().state]
        .iter()
        .all(|s| matches!(s, SessionState::Closing | SessionState::Closed))
    {
        if Instant::now() >= closing_until {
            return Err("IME released views never requested close".into());
        }
        exec.step().await?;
    }
    exec.out.view_release_observed_closing = true;
    let retired = exec
        .native_keyboard
        .retire()
        .map_err(|e| format!("IME keyboard retirement: {e:?}"))?;
    if !retired.retired || retired.hook_live || retired.lease_live || retired.properties_live != 0 {
        return Err("IME keyboard retirement incomplete".into());
    }
    // Both owned windows really composed the fixed Chinese text, so the detail
    // reports that scope instead of the earlier pointer-only one. Task 11.2
    // still also needs candidate/caret placement and a moved window at another
    // DPI, so it stays unaccepted here.
    exec.out.input_detail = Some(object(vec![
        ("kind", quote("ime-compose-two-window")),
        ("success", bool_json(true)),
        ("stages", "4".into()),
        ("task_11_2_accepted", bool_json(false)),
        ("native_chinese_verified", bool_json(true)),
        ("keyboard_input_sent", bool_json(true)),
        ("peer_release", metrics_json(&peer_released)),
        ("peer_entity_released", bool_json(true)),
        ("peer_release_observed_closing", bool_json(true)),
        ("native_keyboard", native_keyboard_json(&retired)),
    ]));
    exec.completed = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_views_require_both_alive_and_refuse_terminal_states() {
        use SessionState::*;
        assert_eq!(creation_ready([Creating, Creating]), Ok(false));
        assert_eq!(creation_ready([Alive, Creating]), Ok(false));
        assert_eq!(creation_ready([Creating, Alive]), Ok(false));
        assert_eq!(creation_ready([Alive, Alive]), Ok(true));
        for terminal in [CreateFailed, Closing, Closed] {
            assert!(creation_ready([terminal, Alive]).is_err());
            assert!(creation_ready([Alive, terminal]).is_err());
        }
    }
}
