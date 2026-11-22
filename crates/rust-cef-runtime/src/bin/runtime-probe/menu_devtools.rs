//! Menu-driven managed DevTools, following CefSharp's `ContextMenuHandler`.
//!
//! CefSharp appends "Show DevTools (Inspect)" (28440) and "Close DevTools"
//! (28441) to the model when the handler is constructed with
//! `addDevtoolsMenuItems: true`, and routes both commands to
//! `ShowDevTools`/`CloseDevTools`. This scenario drives the same two commands
//! through the real production reply path on a real CEF context menu, and
//! requires the browser host's own `has_dev_tools()` answer at both edges -
//! a published model or a queued task is not evidence that DevTools exists.
use rust_cef_core::{
    contracts::ThreadMode,
    menu::{ContextMenuResponse, MenuLimits},
    menu_context::MenuContextLimits,
    menu_lease::MenuLeaseOwner,
    ports::WindowBinding,
};
use rust_cef_runtime::{
    menu_bridge_fixture::MenuBridgeFixture,
    ports::{BrowserHandle, ContextMenuTarget},
    runtime::Runtime,
    test_support::{MenuDevToolsReport, ProbeScenario},
};
use std::{
    num::NonZeroUsize,
    time::{Duration, Instant},
};

type Tick = fn(&mut Runtime, ThreadMode) -> Result<(), String>;

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

fn bounded(
    rt: &mut Runtime,
    mode: ThreadMode,
    tick: Tick,
    label: &str,
    ready: impl Fn(&Runtime) -> bool,
) -> Result<(), String> {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        if ready(rt) {
            return Ok(());
        }
        if Instant::now() >= until {
            return Err(format!("menu devtools {label} timeout"));
        }
        tick(rt, mode)?;
    }
}

/// Open the real context menu, wait for the delivery and read its model.
fn deliver(
    rt: &mut Runtime,
    mode: ThreadMode,
    tick: Tick,
    menu: &MenuBridgeFixture,
    browser: rust_cef_core::contracts::BrowserId,
) -> Result<rust_cef_runtime::menu_bridge_fixture::BridgeDelivery, String> {
    // The bound-bridge fixture clicks a real OSR browser, so CEF itself builds
    // and delivers the model that our opt-in DevTools items were appended to.
    rt.probe_menu_bridge_open(browser, false)
        .map_err(|e| format!("menu open: {e:?}"))?;
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(d) = menu.take() {
            return Ok(d);
        }
        if Instant::now() >= until {
            return Err("menu delivery timeout".into());
        }
        tick(rt, mode)?;
    }
}

pub fn exercise(
    rt: &mut Runtime,
    mode: ThreadMode,
    tick: Tick,
    handle: &BrowserHandle,
    report: &mut MenuDevToolsReport,
) -> Result<(), String> {
    let browser = handle.snapshot().browser;
    rt.configure_devtools(nz(1))
        .map_err(|e| format!("configure_devtools: {e:?}"))?;
    report.configured = true;
    let owner = MenuLeaseOwner::new(
        WindowBinding {
            browser,
            window: browser.local(),
        },
        nz(1),
    )
    .map_err(|e| format!("menu devtools owner: {e:?}"))?;
    rt.configure_context_menus(rust_cef_runtime::ContextMenuConfig {
        capacity: nz(1),
        model_limits: MenuLimits {
            max_entries: nz(32),
            max_label_bytes: nz(256),
            max_total_label_bytes: nz(2048),
        },
        context_limits: MenuContextLimits {
            max_frame_id_bytes: nz(512),
            max_text_bytes: nz(4096),
            max_suggestions: nz(5),
            max_total_bytes: nz(8192),
        },
    })
    .map_err(|e| format!("configure menus: {e:?}"))?;
    let menu = MenuBridgeFixture::new(owner.lease());
    rt.bind_context_menu(ContextMenuTarget {
        lease: owner.lease(),
        delivery: menu.clone(),
    })
    .map_err(|e| format!("menu devtools binding: {e:?}"))?;

    // Show DevTools through the menu, as CefSharp's 28440 branch does.
    let open = deliver(rt, mode, tick, &menu, browser)?;
    report.offered = open
        .presentation
        .model
        .entries
        .iter()
        .filter_map(|e| match e {
            rust_cef_core::menu_model::MenuItem::Command(c) => Some(c.command_id),
            _ => None,
        })
        .collect();
    for required in [28440, 28441] {
        if !report.offered.contains(&required) {
            return Err(format!(
                "menu model is missing the CefSharp devtools item {required}: {:?}",
                report.offered
            ));
        }
    }
    let request = open.presentation.request;
    open.response
        .submit(request, ContextMenuResponse::Select { command_id: 28440 })
        .map_err(|e| format!("show devtools admission: {e:?}"))?;
    bounded(rt, mode, tick, "association after menu open", |rt| {
        rt.devtools_associated(browser) == Some(true)
    })?;
    report.associated_after_menu_open = rt.devtools_associated(browser);
    // While the auxiliary is associated its ledger row stays outstanding; the
    // association answer, not a zero pending count, is what proves it exists.
    report.pending_after_menu_open = rt.devtools_pending(browser);
    if report.pending_after_menu_open == 0 {
        return Err("devtools associated but no ledger row is tracked".into());
    }

    // Close DevTools through the menu, as CefSharp's 28441 branch does.
    let close = deliver(rt, mode, tick, &menu, browser)?;
    let request = close.presentation.request;
    close
        .response
        .submit(request, ContextMenuResponse::Select { command_id: 28441 })
        .map_err(|e| format!("close devtools admission: {e:?}"))?;
    bounded(rt, mode, tick, "association after menu close", |rt| {
        rt.devtools_associated_after_close(browser) == Some(false)
    })?;
    report.associated_after_menu_close = rt.devtools_associated_after_close(browser);
    bounded(rt, mode, tick, "drain after menu close", |rt| {
        rt.devtools_pending(browser) == 0
    })?;
    report.pending_after_menu_close = rt.devtools_pending(browser);
    report.phase = rt.devtools_phase(browser).map(str::to_owned);
    Ok(())
}

#[allow(dead_code)]
fn _scenario_is_explicit_only() -> ProbeScenario {
    ProbeScenario::MenuDevTools
}
