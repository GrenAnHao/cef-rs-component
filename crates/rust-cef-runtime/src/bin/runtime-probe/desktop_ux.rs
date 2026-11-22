//! Real CEF Desktop UX & Config integration probe scenario.
use rust_cef_core::{
    contracts::{
        BrowserCursor, BrowserOptions, ConsolePolicy, ConsoleSeverity, RuntimeSettings,
        ShortcutFilter, ShortcutId, ShortcutRule, ThreadMode,
    },
    input::{InputEvent, InputKind, MouseButton},
    ports::{BrowserCommand, BrowserEvent, BrowserHandle, MouseModifiers},
};
use rust_cef_runtime::{runtime::Runtime, test_support::DesktopUxReport};
use std::time::{Duration, Instant};

type Tick = fn(&mut Runtime, ThreadMode) -> Result<(), String>;

pub fn runtime_settings() -> RuntimeSettings {
    RuntimeSettings {
        user_agent_product: Some("GpuiCefDesktopUx/1.0".into()),
        locale: Some("en-US".into()),
        background_color: Some(0xFF10_2030),
        ..Default::default()
    }
}

pub fn browser_options() -> BrowserOptions {
    BrowserOptions::default()
        .with_frame_rate(60)
        .with_tooltip_events(true)
        .with_console_policy(ConsolePolicy {
            enabled: true,
            max_message_bytes: 256,
        })
        .with_shortcut_filter(
            ShortcutFilter::new(vec![ShortcutRule {
                id: ShortcutId(1),
                windows_key_code: 0x4D, // 'M'
                shift: false,
                control: true,
                alt: false,
                platform: false,
            }])
            .expect("valid shortcut rule"),
        )
}

pub fn fixture_url() -> String {
    // Preserve solid RGB(26,76,126) (#1a4c7e) center pixel for the frame witness.
    let html = r#"<!doctype html><body style="margin:0;background:#1a4c7e"><a id="ux" href="https://example.invalid/ux-link" title="Desktop UX Tooltip" onclick="event.preventDefault()" style="position:fixed;inset:0;display:block;cursor:pointer;color:transparent;text-decoration:none">ux</a><script>
console.warn('ux-probe-warn');
document.title = 'desktop-ux:ready';
document.addEventListener('keydown', e => {
  if (e.ctrlKey && e.key.toLowerCase() === 'm') {
    document.title = 'desktop-ux:shortcut-leaked';
  } else if (e.key.toLowerCase() === 'a') {
    document.title = 'desktop-ux:key-a';
  }
});
</script></body>"#;
    let mut url = String::from("data:text/html,");
    for b in html.bytes() {
        use std::fmt::Write;
        write!(url, "%{b:02X}").expect("String write");
    }
    url
}

fn record_events(handle: &BrowserHandle, report: &mut DesktopUxReport) {
    let batch = handle.take_ux_events();
    report.pulled_ux_event_count = report.pulled_ux_event_count.saturating_add(batch.len());
    for event in batch {
        match event {
            BrowserEvent::CursorChanged(BrowserCursor::PointingHand) => {
                report.cursor_pointing_hand = true;
            }
            BrowserEvent::TooltipChanged(text) if !text.is_empty() => {
                report.tooltip_text = Some(text);
            }
            BrowserEvent::StatusMessageChanged(text) if !text.is_empty() => {
                report.status_observed = true;
            }
            BrowserEvent::LoadingProgressChanged(progress) => {
                report.loading_progress_max = report.loading_progress_max.max(progress);
            }
            BrowserEvent::ConsoleMessage {
                severity: ConsoleSeverity::Warning,
                message,
                ..
            } if message.contains("ux-probe-warn") => {
                report.console_warn_observed = true;
            }
            BrowserEvent::ZoomLevelChanged(milli) => {
                report.zoom_event_milli = Some(milli);
            }
            BrowserEvent::AudioMutedChanged(muted) => {
                report.muted_event = Some(muted);
            }
            BrowserEvent::HostShortcutTriggered { id } => {
                report.shortcut_id = Some(id.0);
            }
            _ => {}
        }
    }
}

fn submit_input(handle: &BrowserHandle, kind: InputKind) -> Result<(), String> {
    let epoch = handle
        .input_snapshot()
        .map_err(|e| format!("desktop_ux input epoch: {e:?}"))?
        .epoch;
    handle
        .submit(BrowserCommand::Input(InputEvent { epoch, kind }))
        .map(|_| ())
        .map_err(|e| format!("desktop_ux submit input: {e:?}"))
}

fn wait_until(
    rt: &mut Runtime,
    mode: ThreadMode,
    handle: &BrowserHandle,
    tick: Tick,
    report: &mut DesktopUxReport,
    deadline: Instant,
    label: &str,
    mut pred: impl FnMut(&BrowserHandle, &DesktopUxReport) -> bool,
) -> Result<(), String> {
    loop {
        record_events(handle, report);
        let snap = handle.snapshot();
        if snap.navigation.title == "desktop-ux:shortcut-leaked" {
            report.page_saw_shortcut_key = true;
            return Err("Ctrl+M leaked to page script instead of pre-key intercept".into());
        }
        if pred(handle, report) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "desktop_ux {label} timeout (report={report:?}, snap={snap:?})"
            ));
        }
        tick(rt, mode)?;
    }
}

pub fn run(
    rt: &mut Runtime,
    mode: ThreadMode,
    handle: &BrowserHandle,
    tick: Tick,
    sink_ux_events: &std::sync::Mutex<Vec<BrowserEvent>>,
) -> Result<DesktopUxReport, String> {
    let mut report = DesktopUxReport::default();
    let deadline = Instant::now() + Duration::from_secs(20);

    wait_until(
        rt,
        mode,
        handle,
        tick,
        &mut report,
        deadline,
        "ready",
        |h, _| {
            let s = h.snapshot();
            s.is_ready() && !s.navigation.is_loading && s.navigation.title == "desktop-ux:ready"
        },
    )?;

    handle
        .submit(BrowserCommand::SetFocus(true))
        .map_err(|e| format!("desktop_ux focus: {e:?}"))?;
    for up in [false, true] {
        submit_input(
            handle,
            InputKind::MouseButton {
                x: 80,
                y: 80,
                button: MouseButton::Left,
                up,
                click_count: 1,
                modifiers: Default::default(),
            },
        )?;
    }
    submit_input(
        handle,
        InputKind::MouseMove {
            x: 80,
            y: 80,
            leaving: false,
            modifiers: Default::default(),
        },
    )?;

    // Chromium fires tooltip after a short hover delay (~500ms); wait for cursor/status/tooltip.
    let mut nudged = false;
    let hover_start = Instant::now();
    let hover_deadline = Instant::now() + Duration::from_secs(6);
    loop {
        record_events(handle, &mut report);
        if report.cursor_pointing_hand && report.status_observed && report.tooltip_text.is_some() {
            break;
        }
        if !nudged && hover_start.elapsed() >= Duration::from_millis(350) {
            nudged = true;
            submit_input(
                handle,
                InputKind::MouseMove {
                    x: 82,
                    y: 82,
                    leaving: false,
                    modifiers: Default::default(),
                },
            )?;
        }
        if Instant::now() >= hover_deadline {
            return Err(format!(
                "desktop_ux hover events timeout: cursor={}, status={}, tooltip={:?}",
                report.cursor_pointing_hand, report.status_observed, report.tooltip_text
            ));
        }
        tick(rt, mode)?;
    }

    handle
        .submit(BrowserCommand::SetZoomLevel(1.5))
        .map_err(|e| format!("desktop_ux set_zoom_level: {e:?}"))?;
    wait_until(
        rt,
        mode,
        handle,
        tick,
        &mut report,
        deadline,
        "zoom",
        |h, r| r.zoom_event_milli == Some(1500) && h.snapshot().navigation.zoom_level_milli == 1500,
    )?;
    report.zoom_snapshot_milli = Some(handle.snapshot().navigation.zoom_level_milli);

    handle
        .submit(BrowserCommand::SetAudioMuted(true))
        .map_err(|e| format!("desktop_ux set_audio_muted: {e:?}"))?;
    wait_until(
        rt,
        mode,
        handle,
        tick,
        &mut report,
        deadline,
        "mute",
        |h, r| r.muted_event == Some(true) && h.snapshot().navigation.audio_muted,
    )?;
    report.muted_snapshot = handle.snapshot().navigation.audio_muted;

    let ctrl_mods = MouseModifiers {
        control: true,
        ..Default::default()
    };
    for up in [false, true] {
        submit_input(
            handle,
            InputKind::Key {
                windows_key_code: 0x4D,
                native_key_code: 0,
                up,
                repeat: false,
                system: false,
                modifiers: ctrl_mods,
            },
        )?;
    }
    wait_until(
        rt,
        mode,
        handle,
        tick,
        &mut report,
        deadline,
        "shortcut Ctrl+M",
        |_, r| r.shortcut_id == Some(1),
    )?;

    for up in [false, true] {
        submit_input(
            handle,
            InputKind::Key {
                windows_key_code: 0x41,
                native_key_code: 0,
                up,
                repeat: false,
                system: false,
                modifiers: Default::default(),
            },
        )?;
    }
    wait_until(
        rt,
        mode,
        handle,
        tick,
        &mut report,
        deadline,
        "passthrough key A",
        |h, _| h.snapshot().navigation.title == "desktop-ux:key-a",
    )?;
    report.passthrough_key_title = Some(handle.snapshot().navigation.title);
    for event in sink_ux_events
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
    {
        if let BrowserEvent::LoadingProgressChanged(progress) = event {
            report.loading_progress_max = report.loading_progress_max.max(*progress);
        }
        if let BrowserEvent::ConsoleMessage {
            severity: ConsoleSeverity::Warning,
            message,
            ..
        } = event
            && message.contains("ux-probe-warn")
        {
            report.console_warn_observed = true;
        }
    }
    if report.loading_progress_max == 0 || !report.console_warn_observed {
        return Err(format!(
            "desktop_ux missing progress or console event: {report:?}"
        ));
    }
    Ok(report)
}
