//! Opt-in fixture: an actual renderer drag request, declined by the embedder.
use rust_cef_core::{
    contracts::{BrowserId, ThreadMode},
    input::{InputKind, MouseButton},
    ports::{BrowserHandle, MouseModifiers, SurfaceKind},
};
use rust_cef_runtime::{runtime::Runtime, test_support::OutboundDragDenial};
use serde_json::{Value, json};

use super::windows::{Seen, fixture_url, send, wait};

pub const READY: &str = "outbound-ready";

pub fn page() -> String {
    fixture_url(concat!(
        "<title>outbound-ready</title><body style='margin:0;height:100vh;background:rgb(26,76,126)'>",
        "<div id='drag' draggable='true' style='position:absolute;left:20px;top:18px;width:180px;height:60px;background:#aac'>Drag</div>",
        "<button id='next' style='position:absolute;left:20px;top:160px;width:100px;height:36px'>Recovery</button>",
        "<script>let starts=0,clicks=0;",
        "document.getElementById('drag').addEventListener('dragstart',e=>{",
        "if(!e.isTrusted){document.title='unexpected-event';return}",
        "e.dataTransfer.setData('text/plain','owned fixture');document.title='outbound-start:'+ ++starts});",
        "document.getElementById('next').addEventListener('click',e=>{",
        "if(!e.isTrusted||starts!==1){document.title='unexpected-event';return}",
        "document.body.style.background='rgb(44,130,70)';document.title='outbound-recovered:'+ ++clicks});",
        "document.addEventListener('drop',()=>{document.title='unexpected-drop'});",
        "</script></body>"
    ))
}

pub fn receipt(rt: &Runtime, id: BrowserId) -> Option<OutboundDragDenial> {
    rt.probe_frame_state(id)?.outbound_drag_denial
}

pub fn exercise(
    rt: &mut Runtime,
    mode: ThreadMode,
    seen: &Seen,
    h: &BrowserHandle,
    url: &str,
) -> Result<Value, String> {
    let id = h.snapshot().browser;
    let initial = rt
        .probe_frame_state(id)
        .ok_or("missing original callback state")?;
    if initial.outbound_drag_denial.is_some() {
        return Err("outbound callback preceded the fixture gesture".into());
    }
    let source = rt
        .frame_source(id)
        .map_err(|e| format!("outbound frame source: {e:?}"))?;
    let baseline = source
        .latest(SurfaceKind::View)
        .ok_or("missing original outbound VIEW")?;
    let generation = baseline.info.surface_generation;
    let sequence = baseline.info.sequence;
    drop(baseline);
    let button = |x, y, up| InputKind::MouseButton {
        x,
        y,
        button: MouseButton::Left,
        up,
        click_count: 1,
        modifiers: MouseModifiers::default(),
    };
    let pointer = |x, y| InputKind::MouseMove {
        x,
        y,
        leaving: false,
        modifiers: MouseModifiers::default(),
    };
    send(h, pointer(40, 40))?;
    send(h, button(40, 40, false))?;
    send(h, pointer(140, 45))?;
    wait(
        rt,
        mode,
        seen,
        "actual renderer drag and CEF StartDragging denial",
        |rt| h.snapshot().navigation.title == "outbound-start:1" && receipt(rt, id).is_some(),
    )?;
    let denial = receipt(rt, id).ok_or("missing original denial")?;
    let start = h.snapshot();
    if denial.callbacks != 1
        || !denial.arguments_present
        || !denial.active
        || rt.diagnostics().cef_ui_thread_id != Some(denial.thread)
        || start.navigation.address != url
        || start.navigation.title != "outbound-start:1"
    {
        return Err("outbound request did not belong to the original live CEF route".into());
    }
    send(h, button(140, 45, true))?;
    send(h, pointer(60, 178))?;
    send(h, button(60, 178, false))?;
    send(h, button(60, 178, true))?;
    let mut recovered = None;
    wait(
        rt,
        mode,
        seen,
        "same Browser trusted click and fresh VIEW after denial",
        |_| {
            let snap = h.snapshot();
            if snap.is_ready()
                && !snap.navigation.is_loading
                && snap.navigation.title == "outbound-recovered:1"
                && snap.navigation.address == url
                && let Some(frame) = source.latest(SurfaceKind::View)
                && frame.info.browser == id
                && frame.info.surface_generation == generation
                && frame.info.sequence > sequence
                && frame.info.width == 320
                && frame.info.height == 240
                && frame
                    .cpu_pixels()
                    .and_then(|px| px.get(4 * (320 * 120 + 160)..4 * (320 * 120 + 160) + 4))
                    == Some(&[70, 130, 44, 255])
            {
                recovered = Some(frame.info.sequence);
            }
            recovered.is_some()
        },
    )?;
    if receipt(rt, id) != Some(denial.clone()) {
        return Err("unexpected further outbound callback".into());
    }
    Ok(json!({
        "browser": [id.runtime().value(), id.local()], "native": denial,
        "initial_sequence": sequence, "generation": generation, "recovered_sequence": recovered,
        "started_title": start.navigation.title, "recovered_title": h.snapshot().navigation.title,
        "same_address": true, "trusted_recovery_click": true, "recovered_center_bgra": [70, 130, 44, 255]
    }))
}
