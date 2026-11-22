//! First real default-menu SelectAll slice. No synthetic CEF context/model or
//! direct response submit; the external owned Win32 driver opens and selects.
//! ESC/submenu/spelling/Print/DevTools/full matrix remain separate acceptance.
use super::input_probe::{Event, InputProbe, WindowFacts};
use super::*;
use gpui::{Focusable, point};
use rust_cef_gpui::{
    ContextMenuConfig, ContextMenuServices, DefaultMenuPresenter, FindServices, MenuBounds,
    MenuPresenterStatus, MenuUiUpdate, SourceViewOptions, SourceViewServices,
    contracts::SessionState,
    menu::{ContextMenuResponse, MenuContextLimits, MenuLimits},
};
use std::{cell::RefCell, num::NonZeroUsize, rc::Rc};
fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}
fn new_file(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    file.write_all(text.as_bytes()).map_err(|e| e.to_string())
}
// Crash breadcrumbs are exact completed cuts, not guessed native progress.
// Each file is create_new and contains owned metadata only. No retry/pump.
pub(super) fn step(options: &Options, name: &str, facts: String) -> Result<(), String> {
    new_file(
        &options
            .report
            .with_file_name(format!("menu-default-step-{name}.json")),
        &object(vec![
            ("pid", std::process::id().to_string()),
            ("step", quote(name)),
            ("facts", facts),
        ]),
    )
}
fn spelling_page() -> String {
    // A real misspelled word in an editable with spellcheck on, so CEF itself
    // decides whether to offer suggestion commands 200..=204.
    let html = "<body style='margin:0;background:rgb(26,76,126)'><div id='e' contenteditable='true' spellcheck='true' lang='en-US' style='height:100px;width:280px;font:24px Arial'>helo</div><script>document.title='menu-spelling-ready';const e=document.getElementById('e');e.focus();const r=document.createRange();r.setStart(e.firstChild,2);r.setEnd(e.firstChild,2);const s=getSelection();s.removeAllRanges();s.addRange(r);new MutationObserver(()=>{if(e.textContent!=='helo'){document.title='menu-spelling-replaced'}}).observe(e,{childList:true,characterData:true,subtree:true})</script></body>";
    data_url(html)
}
fn page(dismiss: bool) -> String {
    // The editor content shrinks to empty only if a real Delete command runs, so
    // the title distinguishes "selected" from "deleted" without trusting a log.
    let html = "<body style='margin:0;background:rgb(26,76,126)'><div id='e' contenteditable='true' spellcheck='false' style='height:100px;width:280px;font:24px Arial'>menu text</div><script>let n=0;const e=document.getElementById('e');document.title='menu-default-ready';document.addEventListener('selectionchange',()=>{if(getSelection().toString()==='menu text'){n++;document.title='menu-default-selected:'+n}});new MutationObserver(()=>{if(e.textContent===''){document.title='menu-default-deleted'}}).observe(e,{childList:true,characterData:true,subtree:true})</script></body>";
    let mut html = html.to_owned();
    if dismiss {
        html.push_str("<script>document.addEventListener('input',()=>document.title='menu-dismiss-unexpected-input')</script>");
    }
    data_url(&html)
}
/// Kind, id, enabled and checked state of every entry as strict JSON, so the
/// matrix can be asserted from the receipt instead of inferred from ids.
fn menu_structure(model: &rust_cef_gpui::menu::MenuModel) -> String {
    use rust_cef_gpui::menu::{MenuChild, MenuItem};
    fn ck(v: Option<bool>) -> &'static str {
        match v {
            Some(true) => "true",
            Some(false) => "false",
            None => "null",
        }
    }
    fn cmd(c: &rust_cef_gpui::menu::MenuCommand) -> String {
        format!(
            "{{\"c\":{},\"en\":{},\"ck\":{}}}",
            c.command_id,
            c.enabled,
            ck(c.checked)
        )
    }
    fn child(c: &MenuChild) -> String {
        match c {
            MenuChild::Command(c) => cmd(c),
            MenuChild::Separator { visible } => format!("{{\"sep\":{visible}}}"),
        }
    }
    let rows: Vec<String> = model
        .entries
        .iter()
        .map(|e| match e {
            MenuItem::Command(c) => cmd(c),
            MenuItem::Separator { visible } => format!("{{\"sep\":{visible}}}"),
            MenuItem::Submenu(sub) => format!(
                "{{\"sub\":{{\"en\":{},\"kids\":[{}]}}}}",
                sub.enabled,
                sub.entries.iter().map(child).collect::<Vec<_>>().join(",")
            ),
        })
        .collect();
    format!("[{}]", rows.join(","))
}

fn has_separator(model: &rust_cef_gpui::menu::MenuModel) -> bool {
    model
        .entries
        .iter()
        .any(|e| matches!(e, rust_cef_gpui::menu::MenuItem::Separator { .. }))
}

fn has_checked(model: &rust_cef_gpui::menu::MenuModel) -> bool {
    model.entries.iter().any(|e| match e {
        rust_cef_gpui::menu::MenuItem::Command(c) => c.checked.is_some(),
        _ => false,
    })
}

fn has_submenu(model: &rust_cef_gpui::menu::MenuModel) -> bool {
    model
        .entries
        .iter()
        .any(|e| matches!(e, rust_cef_gpui::menu::MenuItem::Submenu(_)))
}

fn data_url(html: &str) -> String {
    let mut url = String::from("data:text/html,");
    for b in html.bytes() {
        if b.is_ascii_alphanumeric() {
            url.push(b as char);
        } else {
            url.push_str(&format!("%{b:02X}"));
        }
    }
    url
}
// Original self process metadata, not an OS authentication token. The external
// driver must compare creation time BEFORE reading an image from a reopened PID.
fn process_identity() -> Result<String, String> {
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn GetProcessTimes(
            process: *mut std::ffi::c_void,
            created: *mut FileTime,
            exited: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }
    let (mut created, mut exited, mut kernel, mut user) = (
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
    );
    if unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    } == 0
    {
        return Err("original menu process creation unavailable".into());
    }
    let ticks = ((created.high as u64) << 32) | created.low as u64;
    if ticks == 0 {
        return Err("original menu process creation zero".into());
    }
    let image = std::env::current_exe().map_err(|e| format!("original menu image: {e}"))?;
    let image = image.to_str().ok_or("original menu image not UTF8")?;
    Ok(object(vec![
        ("created_filetime", ticks.to_string()),
        ("image", quote(image)),
    ]))
}
fn window_json(f: WindowFacts) -> String {
    object(vec![
        ("hwnd", f.hwnd.to_string()),
        ("pid", f.pid.to_string()),
        ("thread", f.thread.to_string()),
        ("dpi", f.dpi.to_string()),
        ("left", f.left.to_string()),
        ("top", f.top.to_string()),
        ("width", f.width.to_string()),
        ("height", f.height.to_string()),
    ])
}
fn native_json(n: &rust_cef_gpui::ContextMenuStatus) -> String {
    object(vec![
        ("opened", n.opened.to_string()),
        ("published", n.published.to_string()),
        ("cancelled", n.cancelled.to_string()),
        ("actions", n.actions_submitted.to_string()),
        ("dismissed", n.native_dismissed.to_string()),
        ("post_failures", n.post_failures.to_string()),
        ("retired", bool_json(n.retired)),
        (
            "first_failure",
            n.first_failure
                .as_ref()
                .map_or_else(|| "null".into(), |e| quote(e)),
        ),
    ])
}
fn foreground_json(
    approved: bool,
    attempts: u32,
    before: Option<bool>,
    after: Option<bool>,
) -> String {
    object(vec![
        ("approved", bool_json(approved)),
        ("attempts", attempts.to_string()),
        ("before", before.map_or_else(|| "null".into(), bool_json)),
        ("after", after.map_or_else(|| "null".into(), bool_json)),
        (
            "policy",
            quote(if approved {
                "one-ordinary-owned-request"
            } else {
                "passive"
            }),
        ),
    ])
}
fn rect(b: MenuBounds) -> String {
    format!("[{},{},{},{}]", b.left, b.top, b.width, b.height)
}
fn visual_json(q: &MenuPresenterStatus) -> String {
    object(vec![
        ("retired", bool_json(q.retired)),
        (
            "open_sequence",
            q.open_request
                .map_or_else(|| "null".into(), |id| id.sequence.to_string()),
        ),
        ("pending", q.deferred_replies.to_string()),
        (
            "first_error",
            q.first_error
                .as_ref()
                .map_or_else(|| "null".into(), |e| quote(&format!("{e:?}"))),
        ),
        (
            "last_reply",
            q.last_reply.as_ref().map_or_else(
                || "null".into(),
                |r| {
                    object(vec![
                        ("sequence", r.request.sequence.to_string()),
                        ("requested", quote(&format!("{:?}", r.requested))),
                        ("submitted", quote(&format!("{:?}", r.submitted))),
                        ("accepted", bool_json(r.result.is_ok())),
                    ])
                },
            ),
        ),
        (
            "layout",
            q.last_layout.as_ref().map_or_else(
                || "null".into(),
                |l| {
                    object(vec![
                        ("sequence", l.request.sequence.to_string()),
                        ("menu", l.menu.map_or_else(|| "null".into(), rect)),
                        (
                            "commands",
                            format!(
                                "[{}]",
                                l.commands
                                    .iter()
                                    .map(|c| object(vec![
                                        ("id", c.command_id.to_string()),
                                        ("enabled", bool_json(c.enabled)),
                                        ("bounds", c.bounds.map_or_else(|| "null".into(), rect))
                                    ]))
                                    .collect::<Vec<_>>()
                                    .join(",")
                            ),
                        ),
                    ])
                },
            ),
        ),
    ])
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
    step(options, "entered", object(vec![]))?;
    visibility::require_unlocked_session()?;
    let original_process = process_identity()?;
    step(options, "process-identity", original_process.clone())?;
    let until = Instant::now() + Duration::from_secs(25);
    while rt.state() != RuntimeState::Ready {
        if Instant::now() >= until {
            return Err("menu Runtime readiness timeout".into());
        }
        turn(cx, rt, receiver, false).await?;
        stats.check()?;
    }
    step(options, "runtime-ready", object(vec![]))?;
    let limits = MenuLimits {
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
        model_limits: limits,
        context_limits,
    })
    .map_err(|e| format!("menu config {e:?}"))?;
    // Opt-in capacity so the capture offers CefSharp's DevTools pair; without
    // it the same menu slice stays exactly as before.
    let structure_probe = options.menu_structure_probe || options.menu_submenu_probe;
    let submenu_probe = options.menu_submenu_probe;
    let devtools_probe = options.menu_devtools_probe;
    let source_probe = options.menu_source_probe;
    // Both auxiliary slices use the ordinary four-stage shape with teardown at 2.
    let spelling_probe = options.menu_spelling_probe;
    let aux_probe = devtools_probe || source_probe;
    let mut source_started: u64 = 0;
    let mut edit_verified = false;
    let mut spelling_offered: Vec<i32> = Vec::new();
    let mut spelling_clicked: Option<i32> = None;
    if devtools_probe {
        rt.configure_devtools(nz(1))
            .map_err(|e| format!("menu devtools config {e:?}"))?;
    }
    let handle = rt
        .create_browser(BrowserConfig {
            initial_url: if options.menu_spelling_probe {
                spelling_page()
            } else {
                page(options.menu_dismiss_probe)
            },
            profile: None,
            render_preference: options.backend,
            viewport: Viewport {
                width: 320,
                height: 240,
                scale: 1.,
            },
            options: Default::default(),
        })
        .map_err(|e| format!("menu create {e:?}"))?;
    let browser = handle.snapshot().browser;
    out.browser = Some(browser);
    step(
        options,
        "browser-admitted",
        object(vec![("local", browser.local().to_string())]),
    )?;
    let attachment = rt
        .attach_browser(&handle)
        .map_err(|e| format!("menu attach {e:?}"))?;
    out.duplicate_refused = rt.attach_browser(&handle).is_err();
    let entity = update(cx, root, |host, window, cx| {
        let v = CefBrowser::new(attachment, window, cx)?;
        host.browser = Some(v.clone());
        window.focus(&v.read(cx).focus_handle(cx), cx);
        cx.notify();
        Ok::<_, rust_cef_gpui::ViewError>(v)
    })?
    .map_err(|e| format!("menu View {e:?}"))?;
    let weak = entity.downgrade();
    let mut entity = Some(entity);
    let mut menus = ContextMenuServices::new(nz(1), nz(1), limits, context_limits);
    let target = update(cx, root, |_, window, cx| {
        menus.register(entity.as_ref().unwrap(), window, cx)
    })?
    .map_err(|e| e.to_string())?;
    let registration = rt
        .bind_context_menu(target.clone())
        .map_err(|e| format!("menu bind {e:?}"))?;
    // Opt-in Find service: with it bound the provider offers the fixed CEF
    // "Find" command on the real menu, which otherwise cannot appear at all.
    // Opt-in structural items: a checkable entry and a one-level submenu, which
    // CEF's windowless edit model never offers by itself.
    if structure_probe {
        rt.configure_menu_structure(true)
            .map_err(|e| format!("menu structure config {e:?}"))?;
    }
    let mut finds = if options.menu_find_probe {
        rt.configure_finds(rust_cef_gpui::find::FindLimits {
            capacity: nz(1),
            max_query_bytes: nz(64),
        })
        .map_err(|e| format!("menu find config {e:?}"))?;
        Some(FindServices::new(nz(1), nz(64)))
    } else {
        None
    };
    if options.menu_spelling_probe {
        // Validation-only: allow the original CEF suggestion commands to reach
        // this host. Dictionary writes stay disabled on purpose.
        registration
            .configure_spelling(rust_cef_gpui::SpellingConfig {
                max_word_bytes: nz(64),
                allow_dictionary_write: false,
            })
            .map_err(|e| format!("menu spelling config {e:?}"))?;
    }
    let registration = std::sync::Arc::new(registration);
    if options.menu_source_probe {
        // Capacity must be configured before the service can register; the
        // source-viewer slice does the same thing.
        rt.configure_source_views(rust_cef_gpui::source::SourceViewLimits {
            capacity: nz(1),
            max_source_bytes: nz(8192),
        })
        .map_err(|e| format!("menu source config {e:?}"))?;
    }
    // Opt-in source viewer: binding it is what lets the provider offer the fixed
    // CEF "View source" command on the real menu.
    let mut sources = if options.menu_source_probe {
        Some(SourceViewServices::new(
            nz(1),
            SourceViewOptions {
                max_operations: nz(1),
                max_source_bytes: nz(8192),
                bounds: Bounds::new(point(px(680.), px(40.)), size(px(420.), px(340.))),
                focus: false,
            },
        ))
    } else {
        None
    };
    if let Some(sources) = sources.as_mut() {
        update(cx, root, |_, window, cx| {
            sources.register(
                entity.as_ref().unwrap(),
                window,
                &menus,
                registration.clone(),
                cx,
            )
        })?
        .map_err(|e| {
            eprintln!("menu source register failed: {e:?}");
            format!("menu source register {e:?}")
        })?;
    }
    if let Some(finds) = finds.as_mut() {
        update(cx, root, |_, window, cx| {
            finds.register(
                entity.as_ref().unwrap(),
                window,
                &menus,
                registration.clone(),
                cx,
            )
        })?
        .map_err(|e| format!("menu find register {e:?}"))?;
    }
    let visual = update(cx, root, |host, window, cx| {
        let visual = Rc::new(RefCell::new(DefaultMenuPresenter::new(
            entity.as_ref().unwrap(),
            window,
            limits,
            context_limits,
            cx,
        )?));
        host.menu = Some(visual.clone());
        cx.notify();
        Ok::<_, rust_cef_gpui::MenuPresenterFailure>(visual)
    })?
    .map_err(|e| format!("menu presenter {e:?}"))?;
    let mut probe_slot = None;
    let mut phase = 0u32;
    let mut capture: Option<String> = None;
    let mut select_point = None;
    let mut foreground_attempts = 0;
    let mut foreground_before = None;
    let mut foreground_after = None;
    // Sampled inside the stage loop, read by the report after the loop settles.
    let mut devtools_associated_after_menu: Option<bool> = None;
    // Command ids the real menu actually offered on the first capture.
    let mut offered_commands: Vec<i32> = Vec::new();
    // Structural facts taken from the first real open: the separator and the
    // enabled state come from CEF's own model, not from our own construction.
    let mut structure_seen = (false, false, false, 0usize);
    let mut structure_selected = false;
    let mut armed_child = false;
    let mut find_started: u64 = 0;
    let result = async {
        // Installation/observation failures belong to the SAME cleanup scope
        // as input stages; the already mounted visual owner must shut down.
        step(options, "visual-mounted", object(vec![]))?;
        let probe = probe_slot.insert(app_update(cx, |_| InputProbe::install(TITLE, 0))??);
        probe.observe_browser(handle.clone())?;
        step(options, "probe-observing", window_json(probe.info()?))?;
        if options.menu_foreground_approved {
            if APP_BORROW.with(Cell::get) {
                return Err("menu foreground inside GPUI borrow".into());
            }
            visibility::require_unlocked_session()?;
            let original = probe.info()?;
            foreground_before = Some(probe.is_foreground()?);
            if foreground_before == Some(false) {
                foreground_attempts = 1;
                step(options, "before-foreground", window_json(original))?;
                let requested = probe.foreground_visible();
                foreground_after = Some(probe.is_foreground()?);
                step(
                    options,
                    "after-foreground",
                    object(vec![
                        ("request_returned_ok", bool_json(requested.is_ok())),
                        (
                            "foreground",
                            foreground_after.map_or_else(|| "null".into(), bool_json),
                        ),
                    ]),
                )?;
                requested?;
            } else {
                foreground_after = foreground_before;
            }
            let after = probe.info()?;
            if original.hwnd != after.hwnd
                || original.pid != after.pid
                || original.thread != after.thread
                || (
                    original.left,
                    original.top,
                    original.width,
                    original.height,
                    original.dpi,
                ) != (after.left, after.top, after.width, after.height, after.dpi)
                || foreground_after != Some(true)
            {
                return Err(
                    "menu foreground changed original identity/geometry or was denied".into(),
                );
            }
            visibility::require_unlocked_session()?;
        }
        let until = Instant::now() + Duration::from_secs(25);
        let mut last_readiness = None;
        loop {
            if Instant::now() >= until {
                if let Some(facts) = last_readiness {
                    if let Err(e) = step(options, "view-readiness", facts) {
                        return Err(format!(
                            "menu original VIEW readiness timeout; diagnostic write: {e}"
                        ));
                    }
                }
                return Err("menu original VIEW readiness timeout".into());
            }
            if turn(cx, rt, receiver, false).await? {
                out.wake_consumed += 1;
                update(cx, root, |_, w, cx| {
                    entity
                        .as_ref()
                        .unwrap()
                        .update(cx, |v, cx| v.consume(w, cx))
                })?
                .map_err(|e| format!("menu consume {e:?}"))?;
            }
            stats.check()?;
            let (d, active) = update(cx, root, |_, window, cx| {
                (
                    entity.as_ref().unwrap().read(cx).diagnostics(),
                    window.is_window_active(),
                )
            })?;
            let menu_ready = menus.status(browser).map_err(|e| e.to_string())?.ready;
            let snapshot = handle.snapshot();
            let navigation = &snapshot.navigation;
            let title_matches = if options.menu_spelling_probe {
                navigation.title == "menu-spelling-ready"
            } else {
                navigation.title == "menu-default-ready"
            };
            last_readiness = Some(object(vec![
                ("window_active", bool_json(active)),
                ("menu_ready", bool_json(menu_ready)),
                ("paint_calls", d.paint_calls.to_string()),
                ("painted_sequence", d.painted_sequence.to_string()),
                ("view_error_present", bool_json(d.last_error.is_some())),
                ("fixture_title_matches", bool_json(title_matches)),
                ("is_loading", bool_json(navigation.is_loading)),
            ]));
            let ready = menu_ready
                && d.painted_sequence > 0
                && d.paint_calls > 0
                && d.last_error.is_none()
                && title_matches
                && !navigation.is_loading;
            // Preserve the actual last sample even on timeout; never fill it
            // from expected values or make a failed phase pass because it exists.
            out.paint = Some(d);
            if ready {
                step(options, "view-readiness", last_readiness.take().unwrap())?;
                break;
            }
        }
        let dismiss = options.menu_dismiss_probe;
        let mut escape_prefix_written=false;
        // The devtools variant keeps the ordinary four-stage shape on purpose:
        // stage 2 tears the View down and stage 3 is the terminal sample, so the
        // launcher's gate is only released after the teardown has finished.
        // The Find flow needs a selection before Find(selection) can start, so
        // it runs open/select-all/open/click-Find/teardown/terminal.
        let find_flow = options.menu_find_probe;
        // Edit flow: same two-click shape as Find, but the second click is the
        // fixed CEF Delete command and the witness is the DOM title it causes.
        let edit_flow = options.menu_edit_probe;
        let two_click = find_flow || edit_flow;
        let teardown_stage = if dismiss {
            3
        } else if two_click {
            4
        } else {
            2
        };
        let stage_count = if dismiss {
            5u32
        } else if two_click {
            6u32
        } else {
            4u32
        };
        for stage in 0..stage_count {
            phase = stage;
            visibility::require_unlocked_session()?;
            let facts = probe.info()?;
            let scale = facts.dpi as f32 / 96.;
            if stage == 0 || (dismiss && stage == 2) || (two_click && stage == 2) {
                // The spelling page puts its misspelled word at the top-left, so the
                // right click has to land on the word itself, not below it.
                let (rx, ry) = if options.menu_spelling_probe {
                    (20. * scale, 20. * scale)
                } else {
                    (24. * scale, 68. * scale)
                };
                probe.arm(&[
                    Event::RightDown(rx.round() as i32, ry.round() as i32),
                    Event::RightUp(rx.round() as i32, ry.round() as i32),
                ])?;
            }
            if !dismiss && !(submenu_probe && stage == 1) && (stage == 1 || (two_click && stage == 3)) {
                // stage 1 clicks the validated point; arm those two events.
                let (x0, y0) = match select_point {
                    Some(p) => p,
                    None => ((24. * scale).round() as i32, (116. * scale).round() as i32),
                };
                probe.arm(&[Event::Down(x0, y0), Event::Up(x0, y0)])?;
            }
            if dismiss && stage == 1 {
                // Real Escape produces one marked DOWN/UP and its translated
                // control-character WM_CHAR; no text or layout/IME mutation.
                probe.arm(&[Event::KeyDown(0x1b),Event::Char(0x1b),Event::KeyUp(0x1b)])?;
            }
            if dismiss && stage == 3 {
                let x=(300.*scale).round() as i32;let y=(400.*scale).round() as i32;
                select_point=Some((x,y));
                probe.arm(&[Event::Down(x,y),Event::Up(x,y)])?;
            }
            if (!dismiss && stage >= teardown_stage) || (dismiss && stage == 4) {
                probe.arm(&[])?;
            }
            let q = visual.borrow().status();
            let metadata = object(vec![
                ("schema", "1".into()),
                ("pid", std::process::id().to_string()),
                ("stage", stage.to_string()),
                ("dismiss_probe",bool_json(dismiss)),
                ("original", window_json(facts)),
                ("process_identity",original_process.clone()),
                ("scale", scale.to_string()),
                ("visual", visual_json(&q)),
                (
                    "initial_foreground",
                    foreground_json(
                        options.menu_foreground_approved,
                        foreground_attempts,
                        foreground_before,
                        foreground_after,
                    ),
                ),
                ("native", native_json(&registration.status())),
                (
                    "original_capture",
                    capture.clone().unwrap_or_else(|| "null".into()),
                ),
                (
                    "select_point",
                    select_point.map_or_else(|| "null".into(), |(x, y)| format!("[{x},{y}]")),
                ),
                ("title", quote(&handle.snapshot().navigation.title)),
                ("weak_released", bool_json(weak.upgrade().is_none())),
                (
                    "browser_closed",
                    bool_json(handle.snapshot().state == SessionState::Closed),
                ),
                ("host_alive", bool_json(out.detached_alive)),
            ]);
            new_file(
                &options
                    .report
                    .with_file_name(format!("menu-default-stage-{stage}.json")),
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
                    .with_extension(format!("menu-default-{stage}.ready")),
                &std::process::id().to_string(),
            )?;
            let release = options
                .gate
                .with_extension(format!("menu-default-{stage}.release"));
            let until = Instant::now() + Duration::from_secs(20);
            let mut activation_trace: Vec<String> = Vec::new();
            let mut last_active: Option<bool> = None;
            loop {
                let active = update(cx, root, |_, window, _| window.is_window_active())?;
                if last_active != Some(active) {
                    last_active = Some(active);
                    let n = registration.status();
                    activation_trace.push(format!(
                        "active={active} opened={} published={} dismissed={} actions={}",
                        n.opened, n.published, n.native_dismissed, n.actions_submitted
                    ));
                }
                if options.gate.is_file() {
                    let native = registration.status();
                    let title = handle.snapshot().navigation.title;
                    let pending = visual.borrow().status().open_request.is_some();
                    return Err(format!(
                        "observer released before MenuDefault stage {stage}:                          opened={} published={} cancelled={} actions={} dismissed={} unpending={} wake_refused={} trace={:?}                          first_failure={:?} open_request={pending} title={title:?} activation={:?}",
                        native.opened,
                        native.published,
                        native.cancelled,
                        native.actions_submitted,
                        native.native_dismissed,
                        native.cancel_unpending,
                        native.cancel_wake_refused,
                        native.trace,
                        native.first_failure,
                        activation_trace,
                    ));
                }
                if Instant::now() >= until {
                    let native = registration.status();
                    let visual_status = visual.borrow().status();
                    let trace = probe.trace();
                    let evidence = object(vec![
                        ("pid", std::process::id().to_string()),
                        ("stage", stage.to_string()),
                        ("native", native_json(&native)),
                        ("visual", visual_json(&visual_status)),
                        (
                            "original_capture",
                            capture.clone().unwrap_or_else(|| "null".into()),
                        ),
                        (
                            "trace",
                            trace.as_ref().map_or_else(
                                |e| object(vec![("observation_error", quote(e))]),
                                input::trace_json,
                            ),
                        ),
                    ]);
                    let write_error = new_file(
                        &options
                            .report
                            .with_file_name(format!("menu-default-timeout-{stage}.json")),
                        &evidence,
                    )
                    .err();
                    return Err(format!(
                        "menu stage {stage} timeout {native:?} / {visual_status:?}; diagnostic write: {write_error:?}"
                    ));
                }
                if turn(cx, rt, receiver, false).await? {
                    out.wake_consumed += 1;
                    if let Some(v) = entity.as_ref() {
                        update(cx, root, |_, w, cx| v.update(cx, |v, cx| v.consume(w, cx)))?
                            .map_err(|e| format!("menu running consume {e:?}"))?;
                    }
                }
                stats.check()?;
                if entity.is_some() {
                    let report = update(cx, root, |_, w, cx| {
                        menus.dispatch(browser, w, cx, |event, w, cx| {
                            let result = (|| {
                                if let MenuUiUpdate::Open { presentation, .. } = &event
                                    && spelling_probe
                                {
                                    spelling_offered = presentation
                                        .model
                                        .entries
                                        .iter()
                                        .filter_map(|e| match e {
                                            rust_cef_gpui::menu::MenuItem::Command(c) => {
                                                Some(c.command_id)
                                            }
                                            _ => None,
                                        })
                                        .collect();
                                }
                                if let MenuUiUpdate::Open { presentation, .. } = &event
                                    && capture.is_none()
                                {
                                    // Capture only the first open; later opens in the
                                    // same slice are legitimate (e.g. Find after
                                    // SelectAll) and must not be refused here.
                                    capture = Some(object(vec![
                                        ("sequence", presentation.request.sequence.to_string()),
                                        (
                                            "anchor",
                                            format!(
                                                "[{},{}]",
                                                presentation.model.x, presentation.model.y
                                            ),
                                        ),
                                        (
                                            "selection_bytes",
                                            presentation.context.selection.len().to_string(),
                                        ),
                                        (
                                            "frame_id_bytes",
                                            presentation.context.frame_id.len().to_string(),
                                        ),
                                        ("rows", presentation.model.entries.len().to_string()),
                                        (
                                            "structure",
                                            menu_structure(&presentation.model).to_string(),
                                        ),
                                        (
                                            "separator_present",
                                            has_separator(&presentation.model).to_string(),
                                        ),
                                        (
                                            "checked_present",
                                            has_checked(&presentation.model).to_string(),
                                        ),
                                        (
                                            "submenu_present",
                                            has_submenu(&presentation.model).to_string(),
                                        ),
                                        // Assertion, not decoration: a slice that
                                        // never sees a real separator row fails.
                                        (
                                            "structure_asserted",
                                            {
                                                structure_seen = (
                                                    has_separator(&presentation.model),
                                                    has_checked(&presentation.model),
                                                    has_submenu(&presentation.model),
                                                    presentation.model.entries.len(),
                                                );
                                                has_separator(&presentation.model).to_string()
                                            },
                                        ),
                                    ]));
                                }
                                let r = visual.borrow_mut().update(&event, w, cx);
                                if let Err(e) = &r {
                                    eprintln!("menu presenter failure: {e:?}");
                                }
                                r
                            })();
                            // The RefMut above has ended; even a rejected input
                            // event's last opaque endpoint is released later.
                            cx.defer(move |_| drop(event));
                            result
                        })
                    })?
                    .map_err(|e| e.to_string())?;
                    if report.presenter_error.is_some() || report.cancellation_error.is_some() {
                        return Err(format!("menu dispatch {report:?}"));
                    }
                }
                let q = visual.borrow().status();
                if q.first_error.is_some() {
                    return Err(format!("visual failure {q:?}"));
                }
                if registration.status().first_failure.is_some() {
                    return Err(format!("native menu failure {:?}", registration.status()));
                }
                if dismiss && stage==1 && !escape_prefix_written {
                    let trace=probe.trace()?;
                    // Diagnostic: keep exactly what the window recorded when the
                    // strict prefix condition did not hold, so a missing ack is
                    // explained by evidence instead of a guess.
                    if !(trace.error.is_none() && trace.records.len()==2
                        && trace.records[0].event==Some(Event::KeyDown(0x1b))
                        && trace.records[1].event==Some(Event::Char(0x1b))
                        && trace.records.iter().all(|r|r.accepted && !r.during_manual_pump)) {
                        let path=options.report.with_file_name("menu-dismiss-key-prefix-rejected.json");
                        if !path.is_file() {
                            new_file(&path,&object(vec![
                                ("pid",std::process::id().to_string()),("stage","1".into()),
                                ("records",trace.records.len().to_string()),
                                ("error",quote(&trace.error.clone().unwrap_or_default())),
                                ("trace",input::trace_json(&trace))]));
                        }
                    }
                    if trace.error.is_none() && trace.records.len()==2
                        && trace.records[0].event==Some(Event::KeyDown(0x1b))
                        && trace.records[1].event==Some(Event::Char(0x1b))
                        && trace.records.iter().all(|r|r.accepted && !r.during_manual_pump) {
                        new_file(&options.report.with_file_name("menu-dismiss-key-prefix.json"),
                            &object(vec![("pid",std::process::id().to_string()),("stage","1".into()),
                                ("window",window_json(probe.info()?)),("trace",input::trace_json(&trace))]))?;
                        escape_prefix_written=true;
                    }
                }
                // Publish the arm point as soon as the open menu actually has a
                // layout. Doing it only after the release can run before the
                // layout exists, which leaves a later click without a point.
                if !dismiss
                    && q.open_request.is_some()
                    && (stage == 0 || (two_click && stage == 2) || aux_probe || submenu_probe)
                {
                    let want = if submenu_probe {
                        // Validate against the parent row; the child row is clicked
                        // only after the submenu popup exists.
                        28500
                    } else if structure_probe {
                        // Host-managed checkable item: CEF's own model has none.
                        28500
                    } else if devtools_probe {
                        28440
                    } else if source_probe {
                        132
                    } else if spelling_probe {
                        // First suggestion CEF actually offers (200..=204).
                        spelling_offered
                            .iter()
                            .copied()
                            .find(|id| (200..=204).contains(id))
                            .unwrap_or(117)
                    } else if find_flow && stage == 2 {
                        130
                    } else if edit_flow && stage == 2 {
                        // 112 Cut: CEF's OSR model does not offer 116 Delete at
                        // all, but Cut is present and empties the editable.
                        112
                    } else {
                        117
                    };
                    let layout_facts = visual.borrow().status().last_layout.as_ref().map(|l| {
                        l.commands
                            .iter()
                            .map(|c| format!("{}:{}:{}", c.command_id, c.enabled, c.bounds.is_some()))
                            .collect::<Vec<_>>()
                            .join(",")
                    });
                    let found = visual
                        .borrow()
                        .status()
                        .last_layout
                        .as_ref()
                        .and_then(|l| {
                            l.commands
                                .iter()
                                .find(|c| c.command_id == want && c.bounds.is_some())
                        })
                        .and_then(|c| c.bounds);
                    if found.is_none() {
                        let path = options
                            .report
                            .with_file_name(format!("menu-default-publish-miss-{stage}.json"));
                        if !path.is_file() {
                            new_file(
                                &path,
                                &object(vec![
                                    ("pid", std::process::id().to_string()),
                                    ("stage", stage.to_string()),
                                    ("want", want.to_string()),
                                    ("rows", quote(&layout_facts.unwrap_or_default())),
                                ]),
                            )?;
                        }
                    }
                    if structure_probe {
                        // One-level submenu driving. The parent row carries no
                        // command id, so its position is derived from the measured
                        // checked row plus one row height; the child row is
                        // published only once the submenu popup has laid out.
                        let anchor = visual.borrow().status().last_layout.as_ref().and_then(|l| {
                            l.commands
                                .iter()
                                .find(|c| c.command_id == 28500 && c.bounds.is_some())
                                .and_then(|c| c.bounds)
                        });
                        let child = visual.borrow().status().last_layout.as_ref().and_then(|l| {
                            l.commands
                                .iter()
                                .find(|c| c.command_id == 28501 && c.bounds.is_some())
                                .and_then(|c| c.bounds)
                        });
                        if let Some(b) = anchor {
                            let parent = (
                                ((b.left + b.width / 2.) * scale).round() as i32,
                                ((b.top + b.height + b.height / 2.) * scale).round() as i32,
                            );
                            let path = options
                                .report
                                .with_file_name(format!("menu-default-submenu-point-{stage}.json"));
                            if !path.is_file() {
                                new_file(
                                    &path,
                                    &object(vec![
                                        ("pid", std::process::id().to_string()),
                                        ("stage", stage.to_string()),
                                        ("x", parent.0.to_string()),
                                        ("y", parent.1.to_string()),
                                        (
                                            "derived_from",
                                            quote(&format!(
                                                "{},{},{},{}",
                                                b.left, b.top, b.width, b.height
                                            )),
                                        ),
                                    ]),
                                )?;
                            }
                        }
                        if let Some(cb) = child {
                            // The probe must be armed with where the click will
                            // actually land, and that is the child row - known only
                            // now, after the submenu popup laid out.
                            if submenu_probe
                                && stage == 1
                                && !armed_child
                                && let Some(cpt) = Some((
                                    ((cb.left + cb.width / 2.) * scale).round() as i32,
                                    ((cb.top + cb.height / 2.) * scale).round() as i32,
                                ))
                            {
                                probe.arm(&[Event::Down(cpt.0, cpt.1), Event::Up(cpt.0, cpt.1)])?;
                                armed_child = true;
                            }
                            let cpt = (
                                ((cb.left + cb.width / 2.) * scale).round() as i32,
                                ((cb.top + cb.height / 2.) * scale).round() as i32,
                            );
                            let path = options
                                .report
                                .with_file_name(format!("menu-default-submenu-child-{stage}.json"));
                            if !path.is_file() {
                                new_file(
                                    &path,
                                    &object(vec![
                                        ("pid", std::process::id().to_string()),
                                        ("stage", stage.to_string()),
                                        ("x", cpt.0.to_string()),
                                        ("y", cpt.1.to_string()),
                                        (
                                            "row",
                                            quote(&format!(
                                                "{},{},{},{}",
                                                cb.left, cb.top, cb.width, cb.height
                                            )),
                                        ),
                                    ]),
                                )?;
                            }
                        }
                    }
                    if let Some(b) = found {
                        let point = (
                            ((b.left + b.width / 2.) * scale).round() as i32,
                            ((b.top + b.height / 2.) * scale).round() as i32,
                        );
                        let menu_rect = visual
                            .borrow()
                            .status()
                            .last_layout
                            .as_ref()
                            .and_then(|l| l.menu)
                            .map(|m| format!("{},{},{},{}", m.left, m.top, m.width, m.height))
                            .unwrap_or_default();
                        let path = options
                            .report
                            .with_file_name(format!("menu-default-select-point-{stage}.json"));
                        if !path.is_file() {
                            new_file(
                                &path,
                                &object(vec![
                                    ("pid", std::process::id().to_string()),
                                    ("stage", stage.to_string()),
                                    ("x", point.0.to_string()),
                                    ("y", point.1.to_string()),
                                    (
                                        "row",
                                        quote(&format!(
                                            "{},{},{},{}",
                                            b.left, b.top, b.width, b.height
                                        )),
                                    ),
                                    ("menu", quote(&menu_rect)),
                                    ("scale", format!("{:?}", scale)),
                                ]),
                            )?;
                        }
                        select_point = Some(point);
                    }
                }
                if release.is_file() && probe.complete()? {
                    if stage == 0 || (dismiss && stage == 2) || (two_click && stage == 2) {
                        let ready = q.open_request.is_some()
                            && q.last_layout.as_ref().is_some_and(|l| {
                                l.menu.is_some()
                                    && l.commands.iter().any(|c| {
                                        c.command_id == 117 && c.enabled && c.bounds.is_some()
                                    })
                            });
                        if !ready {
                            continue;
                        }
                    }
                    if dismiss && matches!(stage,1|3) {
                        let expected=if stage==1 {1} else {2};let native=registration.status();
                        if native.opened!=expected || native.published!=expected
                            || native.cancelled!=expected || native.native_dismissed!=expected
                            || native.actions_submitted!=0 || q.open_request.is_some() || q.deferred_replies!=0
                            || handle.snapshot().navigation.title!="menu-default-ready" {continue;}
                        let reply=q.last_reply.as_ref().ok_or("actual dismissal reply absent")?;
                        if reply.requested!=ContextMenuResponse::Cancel || reply.submitted!=ContextMenuResponse::Cancel
                            || reply.result.is_err() {return Err("widget did not cancel original menu".into());}
                    }
                    if edit_flow && stage == 3 {
                        let native = registration.status();
                        if native.actions_submitted != 2
                            || native.native_dismissed != 2
                            || q.open_request.is_some()
                            || q.deferred_replies != 0
                        {
                            continue;
                        }
                        let reply = q.last_reply.as_ref().ok_or("actual delete reply absent")?;
                        if reply.requested != (ContextMenuResponse::Select { command_id: 112 })
                            || reply.submitted != reply.requested
                            || reply.result.is_err()
                        {
                            return Err("widget did not admit Cut".into());
                        }
                        // The fixture page reports an emptied editable as this title.
                        if handle.snapshot().navigation.title != "menu-default-deleted" {
                            continue;
                        }
                        edit_verified = true;
                    }
                    if find_flow && stage == 3 {
                        let native = registration.status();
                        if native.actions_submitted != 2
                            || native.native_dismissed != 2
                            || q.open_request.is_some()
                            || q.deferred_replies != 0
                        {
                            continue;
                        }
                        let reply = q.last_reply.as_ref().ok_or("actual find reply absent")?;
                        if reply.requested != (ContextMenuResponse::Select { command_id: 130 })
                            || reply.submitted != reply.requested
                            || reply.result.is_err()
                        {
                            return Err("widget did not admit Find".into());
                        }
                        if let Some(f) = finds.as_ref() {
                            let s = f.status(browser).map_err(|e| format!("find status {e:?}"))?;
                            if s.started == 0 {
                                continue;
                            }
                            find_started = s.started;
                        }
                    }
                    if spelling_probe && stage == 1 {
                        let native = registration.status();
                        if native.actions_submitted != 1
                            || native.native_dismissed != 1
                            || q.open_request.is_some()
                            || q.deferred_replies != 0
                        {
                            continue;
                        }
                        let reply = q.last_reply.as_ref().ok_or("actual spelling reply absent")?;
                        let ContextMenuResponse::Select { command_id } = reply.requested else {
                            return Err("spelling reply was not a selection".into());
                        };
                        if !(200..=204).contains(&command_id)
                            || reply.submitted != reply.requested
                            || reply.result.is_err()
                        {
                            return Err("widget did not admit a spelling suggestion".into());
                        }
                        spelling_clicked = Some(command_id);
                    }
                    if source_probe && stage == 1 {
                        let native = registration.status();
                        if native.actions_submitted != 1
                            || native.native_dismissed != 1
                            || q.open_request.is_some()
                            || q.deferred_replies != 0
                        {
                            continue;
                        }
                        let reply = q.last_reply.as_ref().ok_or("actual source reply absent")?;
                        if reply.requested != (ContextMenuResponse::Select { command_id: 132 })
                            || reply.submitted != reply.requested
                            || reply.result.is_err()
                        {
                            return Err("widget did not admit View source".into());
                        }
                        // The menu slice witnesses that the command was admitted and
                        // submitted; the viewer's own acceptance lives in the
                        // SourceViewer scenario, so its counters are recorded, never
                        // used as a gate here.
                        if let Some(s) = sources.as_ref() {
                            let st = s
                                .status(browser)
                                .map_err(|e| format!("source status {e:?}"))?;
                            source_started = st.created.max(st.operations as u64);
                        }
                    }
                    if devtools_probe && stage == 1 {
                        let native = registration.status();
                        if native.actions_submitted != 1
                            || native.native_dismissed != 1
                            || q.open_request.is_some()
                            || q.deferred_replies != 0
                        {
                            continue;
                        }
                        let reply = q.last_reply.as_ref().ok_or("actual widget reply absent")?;
                        if reply.requested != (ContextMenuResponse::Select { command_id: 28440 })
                            || reply.submitted != reply.requested
                            || reply.result.is_err()
                        {
                            return Err("widget did not admit Show DevTools".into());
                        }
                        // Same association check CefSharp reads through HasDevTools.
                        let associated = rt.devtools_associated(browser);
                        if associated != Some(true) {
                            continue;
                        }
                        devtools_associated_after_menu = associated;
                    }
                    if structure_probe && stage == 1 {
                        let native = registration.status();
                        if native.actions_submitted != 1
                            || native.native_dismissed != 1
                            || q.open_request.is_some()
                            || q.deferred_replies != 0
                        {
                            continue;
                        }
                        let reply = q.last_reply.as_ref().ok_or("actual widget reply absent")?;
                        let expected = if submenu_probe { 28501 } else { 28500 };
                        if reply.requested != (ContextMenuResponse::Select { command_id: expected })
                            || reply.submitted != reply.requested
                            || reply.result.is_err()
                        {
                            return Err(format!("widget did not admit host item {expected}"));
                        }
                        // A host item carries no CEF command, so the page title is
                        // expected to stay unchanged here: only the recorded host
                        // item proves the click really selected that row.
                        if rt.host_menu_item() != Some(if submenu_probe { 28501 } else { 28500 }) {
                            return Err("host structural selection was not recorded".into());
                        }
                        structure_selected = true;
                    }
                    if !dismiss && !aux_probe && !structure_probe && stage == 1 {
                        let native = registration.status();
                        if native.actions_submitted != 1
                            || native.native_dismissed != 1
                            || handle.snapshot().navigation.title != "menu-default-selected:1"
                            || q.open_request.is_some()
                            || q.deferred_replies != 0
                        {
                            continue;
                        }
                        let reply = q.last_reply.as_ref().ok_or("actual widget reply absent")?;
                        if reply.requested != (ContextMenuResponse::Select { command_id: 117 })
                            || reply.submitted != reply.requested
                            || reply.result.is_err()
                        {
                            return Err("widget did not admit original SelectAll".into());
                        }
                    }
                    if std::fs::read_to_string(&release)
                        .map_err(|e| e.to_string())?
                        .trim()
                        != std::process::id().to_string()
                    {
                        return Err("menu stage release PID mismatch".into());
                    }
                    let trace = probe.finish()?;
                    new_file(
                        &options
                            .report
                            .with_file_name(format!("menu-default-observed-{stage}.json")),
                        &object(vec![
                            ("pid", std::process::id().to_string()),
                            ("stage", stage.to_string()),
                            ("window", window_json(probe.info()?)),
                            ("trace", input::trace_json(&trace)),
                        ]),
                    )?;
                    break;
                }
            }
            if dismiss && stage == 1 {capture=None;}
            if stage == teardown_stage {
                app_update(cx, |app| visual.borrow_mut().shutdown(app))?
                    .map_err(|e| format!("UI shutdown {e:?}"))?;
                out.released_view = Some(
                    update(cx, root, |host, w, cx| {
                        let v = entity.as_ref().unwrap();
                        v.update(cx, |v, cx| v.release_image(w, cx))?;
                        host.browser = None;
                        host.menu = None;
                        host.detached = true;
                        cx.notify();
                        Ok::<_, rust_cef_gpui::ViewError>(v.read(cx).diagnostics())
                    })?
                    .map_err(|e| format!("menu release {e:?}"))?,
                );
                drop(entity.take());
                rendered(cx, root).await?;
                let until = Instant::now() + Duration::from_secs(10);
                while handle.snapshot().state != SessionState::Closed
                    || weak.upgrade().is_some()
                    || visual.borrow().status().deferred_replies != 0
                {
                    if Instant::now() >= until {
                        return Err("menu owner/Browser did not retire".into());
                    }
                    turn(cx, rt, receiver, false).await?;
                    stats.check()?;
                }
                let lease = target
                    .lease
                    .snapshot()
                    .map_err(|e| format!("menu lease {e:?}"))?;
                if !lease.retired || !lease.is_drained() {
                    return Err("menu original lease not retired".into());
                }
                out.view_release_observed_closing = true;
                out.detached_alive = update(cx, root, |h, _, _| {
                    h.browser.is_none() && h.detached && h.renders > 1
                })?;
            }
        }
        let n = registration.status();
        if dismiss {
            if n.opened!=2 || n.published!=2 || n.cancelled!=2 || n.native_dismissed!=2
                || n.actions_submitted!=0 || n.post_failures!=0 || n.first_failure.is_some() {
                return Err(format!("menu dismissal exact native terminals {n:?}"));
            }
        } else if two_click && (n.opened != 2 || n.published != 2 || n.actions_submitted != 2) {
            return Err(format!("menu find exact native terminal {n:?}"));
        } else if !two_click
            && (n.opened != 1
                || n.published != 1
                || n.native_dismissed != 1
                || n.actions_submitted != 1)
            || n.post_failures != 0
            || n.first_failure.is_some()
        {
            return Err(format!("menu exact native terminal {n:?}"));
        }
        phase = if dismiss {
            5
        } else if aux_probe || spelling_probe {
            2
        } else {
            4
        };
        Ok(())
    }
    .await;
    let result = result.and_then(|()| {
        if structure_seen.0 && structure_seen.3 >= 8 {
            Ok(())
        } else {
            Err(format!(
                "real menu model lacked a separator row: rows={} separator={}",
                structure_seen.3, structure_seen.0
            ))
        }
    });
    let cleanup = app_update(cx, |app| visual.borrow_mut().shutdown(app))
        .and_then(|r| r.map_err(|e| format!("visual shutdown {e:?}")));
    let retirement = registration.retire();
    let probe_retire = match probe_slot.as_mut() {
        Some(probe) => app_update(cx, |_| probe.retire()).and_then(|r| r),
        None => Ok(()),
    };
    let _ = update(cx, root, |h, _, cx| {
        h.menu = None;
        h.browser = None;
        h.detached = true;
        cx.notify();
    });
    drop(entity.take());
    let _ = rendered(cx, root).await;
    let final_status = visual.borrow().status();
    let rt_host_menu_item = rt.host_menu_item();
    out.menu_default_detail = Some(object(vec![
        (
            "success",
            bool_json(
                result.is_ok() && cleanup.is_ok() && retirement.is_ok() && probe_retire.is_ok(),
            ),
        ),
        ("phase", phase.to_string()),
        (
            "initial_foreground",
            foreground_json(
                options.menu_foreground_approved,
                foreground_attempts,
                foreground_before,
                foreground_after,
            ),
        ),
        ("visual", visual_json(&final_status)),
        ("native", native_json(&registration.status())),
        ("full_menu_matrix", "false".into()),
        ("structure_probe", bool_json(structure_probe)),
        ("structure_selected", bool_json(structure_selected)),
        ("clicked_rows", format!("{:?}", final_status.clicked_rows)),
        ("opens_seen", final_status.opens_seen.to_string()),
        ("dismiss_updates", final_status.dismiss_updates.to_string()),
        ("withdraws", final_status.withdraws.to_string()),
        (
            "dismiss_click",
            match final_status.dismiss_click {
                Some([x, y]) => format!("[{x},{y}]"),
                None => "null".into(),
            },
        ),
        (
            "dismiss_popup",
            match &final_status.dismiss_popup {
                Some(b) => format!("[{},{},{},{}]", b.left, b.top, b.width, b.height),
                None => "null".into(),
            },
        ),
        (
            "host_menu_item",
            rt_host_menu_item
                .map(|v| v.to_string())
                .unwrap_or_else(|| "null".into()),
        ),
        ("menu_separator_from_cef", bool_json(structure_seen.0)),
        ("menu_checked_from_cef", bool_json(structure_seen.1)),
        ("menu_submenu_from_cef", bool_json(structure_seen.2)),
        ("menu_rows_from_cef", structure_seen.3.to_string()),
        ("synthetic_cef_input", "false".into()),
        ("real_widget_path", "true".into()),
        ("dismiss_probe", bool_json(options.menu_dismiss_probe)),
        ("devtools_probe", bool_json(options.menu_devtools_probe)),
        ("find_probe", bool_json(options.menu_find_probe)),
        ("find_started", find_started.to_string()),
        ("source_probe", bool_json(options.menu_source_probe)),
        ("source_started", source_started.to_string()),
        ("edit_probe", bool_json(options.menu_edit_probe)),
        ("edit_verified", bool_json(edit_verified)),
        ("spelling_probe", bool_json(options.menu_spelling_probe)),
        (
            "spelling_preferences_detail",
            quote(&registration.spelling_status().preferences_detail),
        ),
        (
            "spelling_preferences_enabled",
            bool_json(registration.spelling_status().preferences_enabled),
        ),
        (
            "spelling_offered",
            format!(
                "[{}]",
                spelling_offered
                    .iter()
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        ),
        (
            "spelling_clicked",
            spelling_clicked.map_or_else(|| "null".into(), |i| i.to_string()),
        ),
        (
            "offered_commands",
            format!(
                "[{}]",
                offered_commands
                    .iter()
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        ),
        (
            "devtools_associated_after_menu",
            devtools_associated_after_menu.map_or_else(|| "null".into(), bool_json),
        ),
        (
            "widget_dismiss_verified",
            bool_json(options.menu_dismiss_probe && result.is_ok() && phase == 5),
        ),
        (
            "widget_select_all_verified",
            bool_json(!options.menu_dismiss_probe && result.is_ok() && phase == 4),
        ),
    ]));
    // Keep the reason visible: a failure after the report is written would
    // otherwise leave only a bare exit code.
    if let Err(e) = &result {
        eprintln!("menu-default stage error: {e}");
    }
    if let Err(e) = &cleanup {
        eprintln!("menu-default visual shutdown error: {e}");
    }
    if let Err(e) = &retirement {
        eprintln!("menu-default registration retire error: {e:?}");
    }
    if let Err(e) = &probe_retire {
        eprintln!("menu-default probe retire error: {e}");
    }
    result?;
    cleanup?;
    retirement.map_err(|e| format!("menu registration retire {e:?}"))?;
    probe_retire?;
    Ok(())
}
