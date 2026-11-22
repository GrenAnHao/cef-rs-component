//! Native CEF outbound drag payload extraction and asynchronous source completion ledger.
//!
//! Structural reference (`D:\System Dir\Downloads\CefSharp`, rev `41919703`):
//! - `CefSharp.Wpf/ChromiumWebBrowser.cs:919-973` (`StartDragging` copies/clones `dragData`,
//!   returns `true` immediately, completes `DoDragDrop` asynchronously with
//!   `DragSourceEndedAt` + `DragSourceSystemDragEnded`, and tracks `UpdateDragCursor`).
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use cef::*;
use rust_cef_core::contracts::BrowserId;
use rust_cef_core::outbound_drag::{
    DragOperationMask, DraggedPayload, MAX_DRAG_FILE_PATH_BYTES, MAX_DRAG_FILE_PATHS,
    OutboundDragFinish, OutboundDragLedger, OutboundDragStart,
};
use rust_cef_core::ports::EngineError;

use crate::client::read_cef_string_list;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn cef_mask_to_core(mask: DragOperationsMask) -> DragOperationMask {
    DragOperationMask(mask.as_ref().0 as u32)
}

pub(crate) fn core_mask_to_cef(mask: DragOperationMask) -> DragOperationsMask {
    DragOperationsMask::from(cef::sys::cef_drag_operations_mask_t(mask.bits() as i32))
}

fn cef_userfree_to_string(value: CefStringUserfree) -> Option<String> {
    let text = CefString::from(&value).to_string();
    (!text.is_empty()).then_some(text)
}

/// Copies only the fields indicated by `DragData` flags within bounded byte and entry limits.
pub(crate) fn extract_drag_payload(data: &DragData) -> DraggedPayload {
    let mut file_paths = Vec::new();
    if data.is_file() != 0 {
        let mut list = CefStringList::new();
        if data.file_paths(Some(&mut list)) != 0 {
            file_paths =
                read_cef_string_list(&mut list, MAX_DRAG_FILE_PATHS, MAX_DRAG_FILE_PATH_BYTES);
        }
    }
    let is_fragment = data.is_fragment() != 0;
    let is_link = data.is_link() != 0;
    let text = if is_fragment || is_link {
        cef_userfree_to_string(data.fragment_text())
    } else {
        None
    };
    let html = if is_fragment {
        cef_userfree_to_string(data.fragment_html())
    } else {
        None
    };
    let url = if is_link {
        cef_userfree_to_string(data.link_url())
    } else {
        None
    };
    DraggedPayload::bounded(text.as_deref(), html.as_deref(), url.as_deref(), file_paths)
}

/// Manages single-in-flight asynchronous outbound drag state per Browser.
pub(crate) struct OutboundDragManager {
    ledgers: Mutex<BTreeMap<BrowserId, OutboundDragLedger>>,
    active: AtomicU64,
}

impl OutboundDragManager {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            ledgers: Mutex::new(BTreeMap::new()),
            active: AtomicU64::new(0),
        })
    }

    pub(crate) fn begin(
        &self,
        browser: BrowserId,
        payload: DraggedPayload,
        allowed_ops: DragOperationMask,
        x: i32,
        y: i32,
    ) -> Result<OutboundDragStart, EngineError> {
        let mut guard = lock(&self.ledgers);
        let ledger = guard.entry(browser).or_default();
        let started = ledger.begin(browser, payload, allowed_ops, x, y)?;
        self.active.fetch_add(1, Ordering::AcqRel);
        Ok(started)
    }

    pub(crate) fn update_cursor(&self, browser: BrowserId, op: DragOperationMask) -> bool {
        let mut guard = lock(&self.ledgers);
        guard
            .get_mut(&browser)
            .is_some_and(|ledger| ledger.update_cursor(op))
    }

    pub(crate) fn complete(
        &self,
        finish: OutboundDragFinish,
    ) -> Result<OutboundDragFinish, EngineError> {
        let mut guard = lock(&self.ledgers);
        let ledger = guard
            .get_mut(&finish.drag_id.browser)
            .ok_or(EngineError::InvalidRequest)?;
        let done = ledger.finish(finish)?;
        self.active.fetch_sub(1, Ordering::AcqRel);
        Ok(done)
    }

    pub(crate) fn take_active_for_browser(&self, browser: BrowserId) -> Option<OutboundDragFinish> {
        let mut guard = lock(&self.ledgers);
        let mut ledger = guard.remove(&browser)?;
        let cancelled = ledger.cancel_active()?;
        self.active.fetch_sub(1, Ordering::AcqRel);
        Some(cancelled)
    }

    pub(crate) fn pending_count(&self) -> u64 {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) fn is_clear(&self) -> bool {
        self.pending_count() == 0 && lock(&self.ledgers).values().all(|l| !l.is_active())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::contracts::RuntimeGeneration;
    use rust_cef_core::outbound_drag::DRAG_OP_EVERY;

    fn browser(local: u64) -> BrowserId {
        BrowserId::new(RuntimeGeneration::new(1), local)
    }

    #[test]
    fn cef_and_core_drag_masks_round_trip() {
        for raw in [0u32, 1, 2, 16, 1 | 2 | 16, DRAG_OP_EVERY] {
            let core = DragOperationMask(raw);
            let cef = core_mask_to_cef(core);
            assert_eq!(cef_mask_to_core(cef), core);
        }
    }

    #[test]
    fn manager_tracks_concurrency_completion_and_close_release() {
        let mgr = OutboundDragManager::new();
        let payload = DraggedPayload::bounded(Some("drag"), None, None, Vec::new());

        let s1 = mgr
            .begin(browser(1), payload.clone(), DragOperationMask::COPY, 10, 20)
            .unwrap();
        assert_eq!(mgr.pending_count(), 1);
        assert!(!mgr.is_clear());

        assert_eq!(
            mgr.begin(browser(1), payload.clone(), DragOperationMask::COPY, 11, 21),
            Err(EngineError::Backpressure)
        );
        assert!(mgr.update_cursor(browser(1), DragOperationMask::LINK));

        // Stale or unknown browser cannot complete.
        assert_eq!(
            mgr.complete(OutboundDragFinish {
                drag_id: rust_cef_core::outbound_drag::DragSessionId {
                    browser: browser(2),
                    sequence: s1.drag_id.sequence,
                },
                x: 0,
                y: 0,
                operation: DragOperationMask::NONE,
            }),
            Err(EngineError::InvalidRequest)
        );
        assert_eq!(mgr.pending_count(), 1);

        let finish = OutboundDragFinish {
            drag_id: s1.drag_id,
            x: 40,
            y: 50,
            operation: DragOperationMask::COPY,
        };
        assert_eq!(mgr.complete(finish), Ok(finish));
        assert_eq!(mgr.pending_count(), 0);
        assert!(mgr.is_clear());
        assert_eq!(mgr.complete(finish), Err(EngineError::InvalidRequest));

        // Close cancels remaining active drag and drops barrier count to zero.
        let s2 = mgr
            .begin(browser(2), payload, DragOperationMask::MOVE, 5, 6)
            .unwrap();
        assert_eq!(mgr.pending_count(), 1);
        let cancelled = mgr.take_active_for_browser(browser(2)).unwrap();
        assert_eq!(cancelled.drag_id, s2.drag_id);
        assert_eq!(cancelled.operation, DragOperationMask::NONE);
        assert_eq!(mgr.pending_count(), 0);
        assert!(mgr.is_clear());
    }
}
