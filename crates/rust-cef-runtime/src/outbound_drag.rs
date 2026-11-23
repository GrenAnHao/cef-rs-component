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
    DragImage, DragOperationMask, DraggedFileContents, DraggedPayload,
    MAX_DRAG_FILE_CONTENTS_BYTES, MAX_DRAG_FILE_PATH_BYTES, MAX_DRAG_FILE_PATHS,
    MAX_DRAG_IMAGE_BYTES, OutboundDragFinish, OutboundDragLedger, OutboundDragStart,
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

/// Extracts Chromium's rendered drag ghost bitmap (`CefDragData::GetImage` +
/// `GetImageHotspot`) as bounded top-down unpremultiplied 32-bit BGRA for Win32
/// `IDragSourceHelper::InitializeFromBitmap` (matching Chromium's
/// `ui/base/dragdrop/os_exchange_data_provider_win.cc`).
fn extract_drag_image(data: &DragData, scale_factor: f32) -> Option<DragImage> {
    if data.has_image() == 0 {
        return None;
    }
    let image = data.image()?;
    if image.is_empty() != 0 {
        return None;
    }
    let req_scale = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };
    let mut actual_scale = req_scale;
    let mut rep_w = 0i32;
    let mut rep_h = 0i32;
    if image.representation_info(
        req_scale,
        Some(&mut actual_scale),
        Some(&mut rep_w),
        Some(&mut rep_h),
    ) == 0
        || !actual_scale.is_finite()
        || actual_scale <= 0.0
    {
        actual_scale = 1.0;
    }
    let mut pixel_width = 0i32;
    let mut pixel_height = 0i32;
    let binary = image.as_bitmap(
        actual_scale,
        ColorType::BGRA_8888,
        AlphaType::POSTMULTIPLIED,
        Some(&mut pixel_width),
        Some(&mut pixel_height),
    )?;
    if pixel_width <= 0 || pixel_height <= 0 {
        return None;
    }
    let width = u32::try_from(pixel_width).ok()?;
    let height = u32::try_from(pixel_height).ok()?;
    let expected = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    let size = binary.size();
    if expected == 0 || expected > MAX_DRAG_IMAGE_BYTES || size != expected {
        return None;
    }
    let mut bgra = vec![0u8; expected];
    let copied = binary.data(Some(&mut bgra), 0);
    if copied != expected {
        return None;
    }
    let hotspot = data.image_hotspot();
    let hotspot_x = (hotspot.x as f32 * actual_scale).round() as i32;
    let hotspot_y = (hotspot.y as f32 * actual_scale).round() as i32;
    DragImage::bounded(width, height, hotspot_x, hotspot_y, bgra)
}

wrap_write_handler! {
    struct DragFileWriteHandler {
        buf: Arc<Mutex<Vec<u8>>>,
        max_bytes: usize,
    }

    impl WriteHandler {
        fn write(&self, ptr: *const u8, size: usize, n: usize) -> usize {
            if ptr.is_null() || size == 0 || n == 0 {
                return 0;
            }
            let mut guard = lock(&self.buf);
            let remaining = self.max_bytes.saturating_sub(guard.len());
            let elements = (remaining / size).min(n);
            let Some(copy_len) = elements.checked_mul(size) else {
                return 0;
            };
            if copy_len == 0 {
                return 0;
            }
            let slice = unsafe { std::slice::from_raw_parts(ptr, copy_len) };
            guard.extend_from_slice(slice);
            elements
        }

        fn seek(&self, _offset: i64, _whence: ::std::os::raw::c_int) -> ::std::os::raw::c_int {
            -1
        }

        fn tell(&self) -> i64 {
            lock(&self.buf).len() as i64
        }

        fn flush(&self) -> ::std::os::raw::c_int {
            0
        }

        fn may_block(&self) -> ::std::os::raw::c_int {
            0
        }
    }
}

/// Extracts virtual file bytes dragged out of the web view (e.g. an `<img>`)
/// via `CefDragData::GetFileName()` + `GetFileContents()` (bounded to
/// `MAX_DRAG_FILE_CONTENTS_BYTES`).
fn extract_file_contents(data: &DragData) -> Option<DraggedFileContents> {
    let expected = data.file_contents(None);
    if expected == 0 || expected > MAX_DRAG_FILE_CONTENTS_BYTES {
        return None;
    }
    let buf = Arc::new(Mutex::new(Vec::with_capacity(expected)));
    let mut handler = DragFileWriteHandler::new(Arc::clone(&buf), MAX_DRAG_FILE_CONTENTS_BYTES);
    let mut writer = stream_writer_create_for_handler(Some(&mut handler))?;
    let written = data.file_contents(Some(&mut writer));
    drop(writer);
    drop(handler);
    if written == 0 {
        return None;
    }
    let bytes = std::mem::take(&mut *lock(&buf));
    if bytes.is_empty() || bytes.len() > MAX_DRAG_FILE_CONTENTS_BYTES {
        return None;
    }
    let suggested = cef_userfree_to_string(data.file_name());
    DraggedFileContents::bounded(suggested.as_deref(), bytes)
}

/// Copies all supported fields from `CefDragData` within bounded byte and entry limits.
pub(crate) fn extract_drag_payload(data: &DragData, scale_factor: f32) -> DraggedPayload {
    let mut file_paths = Vec::new();
    let is_file = data.is_file() != 0;
    if is_file {
        let mut list = CefStringList::new();
        if data.file_paths(Some(&mut list)) != 0 {
            file_paths =
                read_cef_string_list(&mut list, MAX_DRAG_FILE_PATHS, MAX_DRAG_FILE_PATH_BYTES);
        }
    }
    let file_contents = extract_file_contents(data);
    let is_fragment = data.is_fragment() != 0;
    let is_link = data.is_link() != 0;
    let text = if is_fragment || is_link {
        cef_userfree_to_string(data.fragment_text())
    } else {
        None
    };
    let html = if is_fragment || is_file || file_contents.is_some() {
        cef_userfree_to_string(data.fragment_html())
    } else {
        None
    };
    let url = if is_link {
        cef_userfree_to_string(data.link_url())
    } else {
        None
    };
    let image = extract_drag_image(data, scale_factor);
    DraggedPayload::bounded(text.as_deref(), html.as_deref(), url.as_deref(), file_paths)
        .with_file_contents(file_contents)
        .with_image(image)
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
