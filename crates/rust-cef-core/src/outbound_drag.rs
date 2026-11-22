//! Pure-std contracts and bounded session ledger for asynchronous outbound drags.
//!
//! Structural reference (`D:\System Dir\Downloads\CefSharp`, rev `41919703`):
//! - `CefSharp.Wpf/ChromiumWebBrowser.cs:919-973` (`StartDragging` -> async `DoDragDrop`
//!   -> `DragSourceEndedAt` + `DragSourceSystemDragEnded`, `UpdateDragCursor`).

use std::fmt;

use crate::contracts::{BrowserId, truncate_utf8_bytes};
use crate::ports::EngineError;

pub const MAX_DRAG_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_DRAG_HTML_BYTES: usize = 256 * 1024;
pub const MAX_DRAG_URL_BYTES: usize = 8 * 1024;
pub const MAX_DRAG_FILE_PATHS: usize = 32;
pub const MAX_DRAG_FILE_PATH_BYTES: usize = 4 * 1024;

pub const DRAG_OP_NONE: u32 = 0;
pub const DRAG_OP_COPY: u32 = 1;
pub const DRAG_OP_LINK: u32 = 2;
pub const DRAG_OP_GENERIC: u32 = 4;
pub const DRAG_OP_PRIVATE: u32 = 8;
pub const DRAG_OP_MOVE: u32 = 16;
pub const DRAG_OP_DELETE: u32 = 32;
pub const DRAG_OP_EVERY: u32 = u32::MAX;

/// Host opt-in gate for web-initiated outbound OLE dragging.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutboundDragPolicy {
    #[default]
    Deny,
    AllowAsync,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DragOperationMask(pub u32);

impl DragOperationMask {
    pub const NONE: Self = Self(DRAG_OP_NONE);
    pub const COPY: Self = Self(DRAG_OP_COPY);
    pub const LINK: Self = Self(DRAG_OP_LINK);
    pub const MOVE: Self = Self(DRAG_OP_MOVE);

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, flag: u32) -> bool {
        (self.0 & flag) != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DragSessionId {
    pub browser: BrowserId,
    pub sequence: u64,
}

/// Owned, bounded snapshot extracted inside `RenderHandler::start_dragging`.
/// `Debug` redacts all dragged contents to byte counts and file entry count.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct DraggedPayload {
    pub text: Option<String>,
    pub html: Option<String>,
    pub url: Option<String>,
    pub file_paths: Vec<String>,
}

impl DraggedPayload {
    /// Extracts bounded UTF-8 strings and sanitized file paths within limits.
    pub fn bounded(
        text: Option<&str>,
        html: Option<&str>,
        url: Option<&str>,
        file_paths: impl IntoIterator<Item = String>,
    ) -> Self {
        let bound_opt = |value: Option<&str>, max: usize, strip_ctl: bool| -> Option<String> {
            let raw = value?;
            if raw.is_empty() {
                return None;
            }
            if strip_ctl && raw.bytes().any(|b| b == 0 || b == b'\r' || b == b'\n') {
                return None;
            }
            let truncated = truncate_utf8_bytes(raw, max);
            if truncated.is_empty() {
                None
            } else {
                Some(truncated)
            }
        };

        let mut sanitized_paths = Vec::new();
        for path in file_paths {
            if sanitized_paths.len() >= MAX_DRAG_FILE_PATHS {
                break;
            }
            if !path.is_empty() && !path.contains('\0') && path.len() <= MAX_DRAG_FILE_PATH_BYTES {
                sanitized_paths.push(path);
            }
        }

        Self {
            text: bound_opt(text, MAX_DRAG_TEXT_BYTES, false),
            html: bound_opt(html, MAX_DRAG_HTML_BYTES, false),
            url: bound_opt(url, MAX_DRAG_URL_BYTES, true),
            file_paths: sanitized_paths,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.as_deref().is_none_or(str::is_empty)
            && self.html.as_deref().is_none_or(str::is_empty)
            && self.url.as_deref().is_none_or(str::is_empty)
            && self.file_paths.is_empty()
    }

    pub fn validate(&self) -> Result<(), EngineError> {
        if self.is_empty() {
            return Err(EngineError::InvalidRequest);
        }
        if let Some(text) = &self.text {
            if text.is_empty() || text.len() > MAX_DRAG_TEXT_BYTES {
                return Err(EngineError::InvalidRequest);
            }
        }
        if let Some(html) = &self.html {
            if html.is_empty() || html.len() > MAX_DRAG_HTML_BYTES {
                return Err(EngineError::InvalidRequest);
            }
        }
        if let Some(url) = &self.url {
            if url.is_empty()
                || url.len() > MAX_DRAG_URL_BYTES
                || url.bytes().any(|b| b == 0 || b == b'\r' || b == b'\n')
            {
                return Err(EngineError::InvalidRequest);
            }
        }
        if self.file_paths.len() > MAX_DRAG_FILE_PATHS {
            return Err(EngineError::InvalidRequest);
        }
        for path in &self.file_paths {
            if path.is_empty() || path.contains('\0') || path.len() > MAX_DRAG_FILE_PATH_BYTES {
                return Err(EngineError::InvalidRequest);
            }
        }
        Ok(())
    }
}

impl fmt::Debug for DraggedPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DraggedPayload")
            .field("text_bytes", &self.text.as_ref().map_or(0, String::len))
            .field("html_bytes", &self.html.as_ref().map_or(0, String::len))
            .field("url_bytes", &self.url.as_ref().map_or(0, String::len))
            .field("file_count", &self.file_paths.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct OutboundDragStart {
    pub drag_id: DragSessionId,
    pub payload: DraggedPayload,
    pub allowed_ops: DragOperationMask,
    pub x: i32,
    pub y: i32,
}

impl fmt::Debug for OutboundDragStart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutboundDragStart")
            .field("drag_id", &self.drag_id)
            .field("payload", &self.payload)
            .field("allowed_ops", &self.allowed_ops)
            .field("x", &self.x)
            .field("y", &self.y)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutboundDragFinish {
    pub drag_id: DragSessionId,
    pub x: i32,
    pub y: i32,
    pub operation: DragOperationMask,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveDrag {
    drag_id: DragSessionId,
    start_x: i32,
    start_y: i32,
    last_cursor_op: DragOperationMask,
}

/// Tracks at most one active asynchronous outbound drag session per browser.
#[derive(Debug)]
pub struct OutboundDragLedger {
    next_sequence: u64,
    active: Option<ActiveDrag>,
}

impl Default for OutboundDragLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl OutboundDragLedger {
    pub fn new() -> Self {
        Self {
            next_sequence: 1,
            active: None,
        }
    }

    pub fn begin(
        &mut self,
        browser: BrowserId,
        payload: DraggedPayload,
        allowed_ops: DragOperationMask,
        x: i32,
        y: i32,
    ) -> Result<OutboundDragStart, EngineError> {
        payload.validate()?;
        if self.active.is_some() {
            return Err(EngineError::Backpressure);
        }
        let sequence = self.next_sequence;
        let next = sequence
            .checked_add(1)
            .ok_or(EngineError::Backend { code: None })?;
        self.next_sequence = next;
        let drag_id = DragSessionId { browser, sequence };
        self.active = Some(ActiveDrag {
            drag_id,
            start_x: x,
            start_y: y,
            last_cursor_op: DragOperationMask::NONE,
        });
        Ok(OutboundDragStart {
            drag_id,
            payload,
            allowed_ops,
            x,
            y,
        })
    }

    pub fn update_cursor(&mut self, op: DragOperationMask) -> bool {
        let Some(active) = &mut self.active else {
            return false;
        };
        active.last_cursor_op = op;
        true
    }

    pub fn finish(
        &mut self,
        finish: OutboundDragFinish,
    ) -> Result<OutboundDragFinish, EngineError> {
        let Some(active) = self.active else {
            return Err(EngineError::InvalidRequest);
        };
        if active.drag_id != finish.drag_id {
            return Err(EngineError::InvalidRequest);
        }
        self.active = None;
        Ok(finish)
    }

    pub fn cancel_active(&mut self) -> Option<OutboundDragFinish> {
        let active = self.active.take()?;
        Some(OutboundDragFinish {
            drag_id: active.drag_id,
            x: active.start_x,
            y: active.start_y,
            operation: DragOperationMask::NONE,
        })
    }

    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub fn active_id(&self) -> Option<DragSessionId> {
        self.active.map(|active| active.drag_id)
    }

    pub fn last_cursor_op(&self) -> DragOperationMask {
        self.active
            .map_or(DragOperationMask::NONE, |active| active.last_cursor_op)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::RuntimeGeneration;

    fn browser(local: u64) -> BrowserId {
        BrowserId::new(RuntimeGeneration::new(1), local)
    }

    #[test]
    fn default_policy_is_deny() {
        assert_eq!(OutboundDragPolicy::default(), OutboundDragPolicy::Deny);
    }

    #[test]
    fn bounded_payload_truncates_utf8_and_filters_paths() {
        let long_text = "中".repeat(MAX_DRAG_TEXT_BYTES);
        let long_html = "<div>".to_owned() + &"a".repeat(MAX_DRAG_HTML_BYTES);
        let long_url = "https://example.com/".to_owned() + &"q".repeat(MAX_DRAG_URL_BYTES);
        let mut paths: Vec<String> = (0..(MAX_DRAG_FILE_PATHS + 5))
            .map(|i| format!(r"C:\tmp\file-{i}.txt"))
            .collect();
        paths.insert(0, String::new());
        paths.insert(1, "bad\0path".to_owned());
        paths.insert(
            2,
            "C:\\".to_owned() + &"x".repeat(MAX_DRAG_FILE_PATH_BYTES + 1),
        );

        let payload =
            DraggedPayload::bounded(Some(&long_text), Some(&long_html), Some(&long_url), paths);
        assert_eq!(payload.validate(), Ok(()));
        assert!(payload.text.as_ref().unwrap().len() <= MAX_DRAG_TEXT_BYTES);
        assert!(payload.html.as_ref().unwrap().len() <= MAX_DRAG_HTML_BYTES);
        assert!(payload.url.as_ref().unwrap().len() <= MAX_DRAG_URL_BYTES);
        assert_eq!(payload.file_paths.len(), MAX_DRAG_FILE_PATHS);
        assert!(payload.file_paths.iter().all(|p| !p.contains('\0')));
    }

    #[test]
    fn debug_redacts_all_payload_fields() {
        let payload = DraggedPayload {
            text: Some("TOP_SECRET_TEXT_123".into()),
            html: Some("<p>TOP_SECRET_HTML_456</p>".into()),
            url: Some("https://secret.example.invalid/token=999".into()),
            file_paths: vec![r"C:\Users\secret\vault.txt".into()],
        };
        let start = OutboundDragStart {
            drag_id: DragSessionId {
                browser: browser(7),
                sequence: 1,
            },
            payload,
            allowed_ops: DragOperationMask::COPY,
            x: 12,
            y: 34,
        };
        let dbg = format!("{start:?}");
        assert!(!dbg.contains("TOP_SECRET"));
        assert!(!dbg.contains("secret.example"));
        assert!(!dbg.contains("vault.txt"));
        assert!(dbg.contains("text_bytes: 19"));
        assert!(dbg.contains("file_count: 1"));
    }

    #[test]
    fn ledger_enforces_single_in_flight_drag_and_exact_id_completion() {
        let mut ledger = OutboundDragLedger::new();
        let payload = DraggedPayload::bounded(Some("drag text"), None, None, Vec::new());
        let started = ledger
            .begin(browser(1), payload.clone(), DragOperationMask::COPY, 10, 20)
            .unwrap();
        assert!(ledger.is_active());
        assert_eq!(ledger.active_id(), Some(started.drag_id));

        // Second concurrent drag is rejected.
        assert_eq!(
            ledger.begin(browser(1), payload, DragOperationMask::COPY, 15, 25),
            Err(EngineError::Backpressure)
        );
        assert!(ledger.update_cursor(DragOperationMask::MOVE));
        assert_eq!(ledger.last_cursor_op(), DragOperationMask::MOVE);

        // Stale or foreign drag_id finish is rejected without clearing active.
        let wrong_id = DragSessionId {
            browser: browser(2),
            sequence: started.drag_id.sequence,
        };
        assert_eq!(
            ledger.finish(OutboundDragFinish {
                drag_id: wrong_id,
                x: 0,
                y: 0,
                operation: DragOperationMask::NONE,
            }),
            Err(EngineError::InvalidRequest)
        );
        assert!(ledger.is_active());

        // Matching drag_id completes cleanly and duplicate is rejected.
        let done = ledger
            .finish(OutboundDragFinish {
                drag_id: started.drag_id,
                x: 50,
                y: 60,
                operation: DragOperationMask::COPY,
            })
            .unwrap();
        assert_eq!(done.operation, DragOperationMask::COPY);
        assert!(!ledger.is_active());
        assert_eq!(ledger.finish(done), Err(EngineError::InvalidRequest));
    }

    #[test]
    fn cancel_active_returns_none_op_at_start_coordinates() {
        let mut ledger = OutboundDragLedger::new();
        assert_eq!(ledger.cancel_active(), None);

        let payload = DraggedPayload::bounded(None, None, Some("https://example.com"), Vec::new());
        let started = ledger
            .begin(browser(3), payload, DragOperationMask::LINK, 42, 84)
            .unwrap();
        let cancelled = ledger.cancel_active().unwrap();
        assert_eq!(cancelled.drag_id, started.drag_id);
        assert_eq!(cancelled.x, 42);
        assert_eq!(cancelled.y, 84);
        assert_eq!(cancelled.operation, DragOperationMask::NONE);
        assert!(!ledger.is_active());
    }
}
