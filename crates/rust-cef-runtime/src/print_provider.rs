//! Host-confirmed print dialog handoff. The callback is a CEF object owned by
//! the CEF UI thread, so it lives in a UI-thread local map: no Send/Sync is
//! claimed, no native object is touched under a Registry or data lock.
use cef::{ImplPrintDialogCallback, ImplPrintJobCallback, PrintDialogCallback, PrintJobCallback};
use rust_cef_core::contracts::BrowserId;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    sync::atomic::{AtomicBool, Ordering},
};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Explicit host opt-in. Without it every dialog is cancelled, never queued.
pub(crate) fn enable() {
    ENABLED.store(true, Ordering::Release);
}

fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

thread_local! {
    static PENDING: RefCell<BTreeMap<BrowserId, PrintDialogCallback>> = RefCell::new(BTreeMap::new());
    static JOB: RefCell<Option<(BrowserId, String, u64, u64)>> = RefCell::new(None);
}

/// Pre-approve exactly one print job. CEF has no job cancel, so this decision
/// must precede on_print_job; the token is single-use per Browser/document.
pub(crate) fn approve_job(
    browser: BrowserId,
    document: &str,
    registration: std::num::NonZeroU64,
    document_epoch: u64,
) -> bool {
    if !enabled() || document.is_empty() || document_epoch == 0 {
        return false;
    }
    JOB.with(|job| {
        *job.borrow_mut() = Some((
            browser,
            document.to_owned(),
            registration.get(),
            document_epoch,
        ));
        true
    })
}

/// Whether a stored print dialog is still waiting for the host decision.
pub(crate) fn has_dialog(browser: BrowserId) -> bool {
    PENDING.with(|pending| pending.borrow().contains_key(&browser))
}

/// Consume the token only when Browser and document name both match.
pub(crate) fn take_job(
    browser: BrowserId,
    document: &str,
    current_epoch: u64,
) -> Option<std::num::NonZeroU64> {
    JOB.with(|job| {
        let mut job = job.borrow_mut();
        let matched = matches!(
            job.as_ref(),
            Some((owner, name, _, _)) if *owner == browser && name == document
        );
        if !matched {
            return None;
        }
        let token = job.take()?;
        // A navigation between approval and the job callback invalidates it.
        if token.3 != current_epoch {
            return None;
        }
        std::num::NonZeroU64::new(token.2)
    })
}

/// Continue the job CEF is waiting on. CEF UI thread only.
pub(crate) fn continue_job(callback: Option<&mut PrintJobCallback>) {
    if let Some(callback) = callback {
        callback.cont();
    }
}

/// Store one dialog callback for the host to confirm. UI thread only.
pub(crate) fn store(browser: BrowserId, callback: Option<&mut PrintDialogCallback>) -> bool {
    if !enabled() {
        return false;
    }
    let Some(callback) = callback else {
        return false;
    };
    PENDING.with(|pending| {
        let mut pending = pending.borrow_mut();
        if pending.contains_key(&browser) {
            return false;
        }
        pending.insert(browser, callback.clone());
        true
    })
}

/// Continue or cancel the stored dialog. Called on the CEF UI thread only.
pub(crate) fn resolve(browser: BrowserId, accept: bool) {
    let callback = PENDING.with(|pending| pending.borrow_mut().remove(&browser));
    if let Some(callback) = callback {
        if accept {
            callback.cont(None);
        } else {
            callback.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::contracts::RuntimeGeneration;

    #[test]
    fn job_token_is_single_use_and_browser_bound() {
        enable();
        let first = BrowserId::new(RuntimeGeneration::new(1), 1);
        let second = BrowserId::new(RuntimeGeneration::new(1), 2);
        let reg = std::num::NonZeroU64::new(7).unwrap();
        assert!(approve_job(first, "document.txt", reg, 3));
        assert!(take_job(second, "document.txt", 3).is_none());
        assert!(take_job(first, "another.txt", 3).is_none());
        assert_eq!(take_job(first, "document.txt", 3), Some(reg));
        assert!(take_job(first, "document.txt", 3).is_none());
        // A stale epoch cancels instead of continuing.
        assert!(approve_job(first, "stale.txt", reg, 3));
        assert!(take_job(first, "stale.txt", 4).is_none());
        assert!(!approve_job(first, "", reg, 3));
        assert!(!approve_job(first, "document.txt", reg, 0));
    }
}
