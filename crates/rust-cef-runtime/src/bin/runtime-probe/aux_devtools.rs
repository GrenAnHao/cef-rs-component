//! Managed DevTools: association, explicit close and parent-close drain.
//!
//! Follows the CefSharp template (`CefBrowserHostWrapper::ShowDevTools` /
//! `HasDevTools` / `CloseDevTools`): a real DevTools webview exists only when the
//! browser host itself reports the association, and the two ways of ending it -
//! explicit `close_dev_tools` and closing the parent Browser - are recorded
//! separately because they are different edges of the same state.
use rust_cef_core::{
    contracts::{SessionState, ThreadMode},
    ports::{BrowserHandle, CloseMode},
};
use rust_cef_runtime::{runtime::Runtime, test_support::AuxDevToolsReport};
use std::{
    num::NonZeroUsize,
    time::{Duration, Instant},
};

type Tick = fn(&mut Runtime, ThreadMode) -> Result<(), String>;

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
            return Err(format!("aux devtools {label} timeout"));
        }
        tick(rt, mode)?;
    }
}

/// Configure once and open one auxiliary, returning what was observed.
fn open_once(
    rt: &mut Runtime,
    mode: ThreadMode,
    tick: Tick,
    handle: &BrowserHandle,
) -> Result<(u64, Option<String>, Option<bool>), String> {
    let browser = handle.snapshot().browser;
    rt.open_devtools(browser)
        .map_err(|e| format!("open_devtools: {e:?}"))?;
    bounded(rt, mode, tick, "open", |rt| {
        rt.devtools_pending(browser) > 0
    })?;
    let pending = rt.devtools_pending(browser);
    let phase = rt.devtools_phase(browser).map(str::to_owned);
    if pending == 0 {
        return Err("aux devtools reported open with no pending task".into());
    }
    // A pending slot alone does not prove DevTools exists; require the host's own
    // association answer, which is what CefSharp reads in HasDevTools.
    bounded(rt, mode, tick, "association", |rt| {
        rt.devtools_associated(browser).is_some()
    })?;
    let associated = rt.devtools_associated(browser);
    if associated != Some(true) {
        return Err(format!(
            "aux devtools show issued but host reports no associated DevTools browser: {associated:?}"
        ));
    }
    Ok((pending, phase, associated))
}

/// Path A: explicit `close_dev_tools` must remove the association and retire the
/// auxiliary slot while the parent Browser stays alive.
fn explicit_close(
    rt: &mut Runtime,
    mode: ThreadMode,
    tick: Tick,
    handle: &BrowserHandle,
    proof: &mut AuxDevToolsReport,
) -> Result<(), String> {
    let browser = handle.snapshot().browser;
    let (_, _, associated) = open_once(rt, mode, tick, handle)?;
    proof.explicit_associated_after_open = associated;
    rt.close_dev_tools(browser)
        .map_err(|e| format!("close_dev_tools: {e:?}"))?;
    bounded(rt, mode, tick, "explicit-retire", |rt| {
        rt.devtools_pending(browser) == 0
    })?;
    proof.explicit_retired_after_close = true;
    bounded(rt, mode, tick, "explicit-association", |rt| {
        rt.devtools_associated_after_close(browser).is_some()
    })?;
    proof.explicit_associated_after_close = rt.devtools_associated_after_close(browser);
    if proof.explicit_associated_after_close != Some(false) {
        return Err(format!(
            "close_dev_tools did not clear the host association: {:?}",
            proof.explicit_associated_after_close
        ));
    }
    Ok(())
}

/// Path B: closing the parent Browser must drain an auxiliary that is still in
/// flight, and the retirement must precede shutdown.
fn parent_close(
    rt: &mut Runtime,
    mode: ThreadMode,
    tick: Tick,
    handle: &BrowserHandle,
    proof: &mut AuxDevToolsReport,
) -> Result<(), String> {
    let browser = handle.snapshot().browser;
    let (pending, phase, associated) = open_once(rt, mode, tick, handle)?;
    proof.pending_after_open = pending;
    proof.phase_after_open = phase;
    proof.associated_after_open = associated;
    let _ticket = handle.request_close(CloseMode::Force);
    bounded(rt, mode, tick, "drain", |rt| {
        rt.devtools_pending(browser) == 0
    })?;
    proof.pending_after_close = rt.devtools_pending(browser);
    proof.phase_after_close = rt.devtools_phase(browser).map(str::to_owned);
    bounded(rt, mode, tick, "closed", |_| {
        handle.snapshot().state == SessionState::Closed
    })?;
    proof.closed = true;
    Ok(())
}

pub(crate) fn exercise(
    rt: &mut Runtime,
    mode: ThreadMode,
    tick: Tick,
    handle: &BrowserHandle,
    proof: &mut AuxDevToolsReport,
) -> Result<(), String> {
    // configure_devtools is single-shot by contract, so it happens exactly once
    // and both paths reuse the same configured capacity.
    let capacity = NonZeroUsize::new(1).ok_or("aux devtools capacity")?;
    rt.configure_devtools(capacity)
        .map_err(|e| format!("configure_devtools: {e:?}"))?;
    proof.configured = true;
    explicit_close(rt, mode, tick, handle, proof)?;
    parent_close(rt, mode, tick, handle, proof)?;
    Ok(())
}
