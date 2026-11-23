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
pub const MAX_DRAG_FILE_NAME_UTF16: usize = 240;
pub const MAX_DRAG_FILE_CONTENTS_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_DRAG_IMAGE_DIMENSION: u32 = 1024;
pub const MAX_DRAG_IMAGE_BYTES: usize = 4 * 1024 * 1024;

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
    pub const EVERY: Self = Self(DRAG_OP_EVERY);

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

/// Bounded top-down 32-bit unpremultiplied BGRA drag preview image captured from
/// `CefDragData::GetImage()` + `GetImageHotspot()` for Win32 `SHDRAGIMAGE`.
#[derive(Clone, PartialEq, Eq)]
pub struct DragImage {
    pub width: u32,
    pub height: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub bgra: Vec<u8>,
}

impl DragImage {
    pub fn bounded(
        width: u32,
        height: u32,
        hotspot_x: i32,
        hotspot_y: i32,
        bgra: Vec<u8>,
    ) -> Option<Self> {
        if width == 0
            || height == 0
            || width > MAX_DRAG_IMAGE_DIMENSION
            || height > MAX_DRAG_IMAGE_DIMENSION
        {
            return None;
        }
        let expected = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        if expected == 0 || expected > MAX_DRAG_IMAGE_BYTES || bgra.len() != expected {
            return None;
        }
        Some(Self {
            width,
            height,
            hotspot_x: hotspot_x.clamp(0, width as i32),
            hotspot_y: hotspot_y.clamp(0, height as i32),
            bgra,
        })
    }

    pub fn validate(&self) -> Result<(), EngineError> {
        if self.width == 0
            || self.height == 0
            || self.width > MAX_DRAG_IMAGE_DIMENSION
            || self.height > MAX_DRAG_IMAGE_DIMENSION
        {
            return Err(EngineError::InvalidRequest);
        }
        let expected = (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or(EngineError::InvalidRequest)?;
        if expected == 0 || expected > MAX_DRAG_IMAGE_BYTES || self.bgra.len() != expected {
            return Err(EngineError::InvalidRequest);
        }
        Ok(())
    }
}

impl fmt::Debug for DragImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DragImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("hotspot_x", &self.hotspot_x)
            .field("hotspot_y", &self.hotspot_y)
            .field("bgra_bytes", &self.bgra.len())
            .finish()
    }
}

/// Bounded virtual file (e.g., an `<img>` dragged out of a web page) extracted
/// from `CefDragData::GetFileName()` + `GetFileContents()` for Windows
/// `CFSTR_FILEDESCRIPTORW` + `CFSTR_FILECONTENTS`.
#[derive(Clone, PartialEq, Eq)]
pub struct DraggedFileContents {
    pub file_name: String,
    pub bytes: Vec<u8>,
}

fn infer_image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1A\n") {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.starts_with(b"BM") {
        Some("bmp")
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" && &bytes[8..12] == b"avif" {
        Some("avif")
    } else {
        let trimmed = bytes
            .iter()
            .position(|b| !b.is_ascii_whitespace())
            .map_or(&[][..], |i| &bytes[i..]);
        if trimmed.starts_with(b"<svg") || trimmed.starts_with(b"<?xml") {
            Some("svg")
        } else {
            None
        }
    }
}

fn is_windows_reserved_stem(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn sanitize_virtual_file_name(suggested: Option<&str>, bytes: &[u8]) -> String {
    let leaf = suggested
        .and_then(|s| s.rsplit(['/', '\\']).find(|seg| !seg.is_empty()))
        .unwrap_or("");
    let mut cleaned = String::with_capacity(leaf.len());
    for ch in leaf.chars() {
        if ch <= '\u{1F}'
            || ch == '\u{7F}'
            || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        {
            cleaned.push('_');
        } else {
            cleaned.push(ch);
        }
    }
    let trimmed = cleaned.trim_matches(|c: char| c == ' ' || c == '.');
    let inferred_ext = infer_image_extension(bytes);

    let (raw_stem, raw_ext) = match trimmed.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && !e.is_empty() && e.len() <= 16 && e.is_ascii() => {
            (s, Some(e))
        }
        _ => (trimmed, None),
    };

    let mut stem = if raw_stem.is_empty() || raw_stem == "." || raw_stem == ".." {
        if inferred_ext.is_some() {
            "image".to_string()
        } else {
            "download".to_string()
        }
    } else {
        raw_stem
            .trim_matches(|c: char| c == ' ' || c == '.')
            .to_string()
    };
    if stem.is_empty() || is_windows_reserved_stem(&stem) {
        stem = format!("_{stem}");
        if stem == "_" {
            stem = "image".to_string();
        }
    }

    let ext = raw_ext.or(inferred_ext);
    let ext_units = ext.map_or(0, |e| 1 + e.encode_utf16().count());
    let max_stem_units = MAX_DRAG_FILE_NAME_UTF16.saturating_sub(ext_units).max(1);
    let mut kept_stem = String::new();
    let mut units = 0usize;
    for ch in stem.chars() {
        let len = ch.len_utf16();
        if units + len > max_stem_units {
            break;
        }
        kept_stem.push(ch);
        units += len;
    }
    let kept_stem = kept_stem.trim_end_matches([' ', '.']);
    let kept_stem = if kept_stem.is_empty() {
        "image"
    } else {
        kept_stem
    };
    match ext {
        Some(e) => format!("{kept_stem}.{e}"),
        None => kept_stem.to_string(),
    }
}

impl DraggedFileContents {
    pub fn bounded(suggested_name: Option<&str>, bytes: Vec<u8>) -> Option<Self> {
        if bytes.is_empty() || bytes.len() > MAX_DRAG_FILE_CONTENTS_BYTES {
            return None;
        }
        let file_name = sanitize_virtual_file_name(suggested_name, &bytes);
        Some(Self { file_name, bytes })
    }

    pub fn validate(&self) -> Result<(), EngineError> {
        if self.bytes.is_empty() || self.bytes.len() > MAX_DRAG_FILE_CONTENTS_BYTES {
            return Err(EngineError::InvalidRequest);
        }
        if self.file_name.is_empty()
            || self.file_name == "."
            || self.file_name == ".."
            || self.file_name.encode_utf16().count() > MAX_DRAG_FILE_NAME_UTF16
            || self.file_name.chars().any(|c| {
                c <= '\u{1F}'
                    || c == '\u{7F}'
                    || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
            })
        {
            return Err(EngineError::InvalidRequest);
        }
        Ok(())
    }
}

impl fmt::Debug for DraggedFileContents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DraggedFileContents")
            .field("file_name_bytes", &self.file_name.len())
            .field("content_bytes", &self.bytes.len())
            .finish()
    }
}

/// Owned, bounded snapshot extracted inside `RenderHandler::start_dragging`.
/// `Debug` redacts all dragged contents to byte counts and file entry count.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct DraggedPayload {
    pub text: Option<String>,
    pub html: Option<String>,
    pub url: Option<String>,
    pub file_paths: Vec<String>,
    pub file_contents: Option<DraggedFileContents>,
    pub image: Option<DragImage>,
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
            file_contents: None,
            image: None,
        }
    }

    pub fn with_file_contents(mut self, file_contents: Option<DraggedFileContents>) -> Self {
        self.file_contents = file_contents;
        self
    }

    pub fn with_image(mut self, image: Option<DragImage>) -> Self {
        self.image = image;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.text.as_deref().is_none_or(str::is_empty)
            && self.html.as_deref().is_none_or(str::is_empty)
            && self.url.as_deref().is_none_or(str::is_empty)
            && self.file_paths.is_empty()
            && self.file_contents.is_none()
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
        if let Some(file_contents) = &self.file_contents {
            file_contents.validate()?;
        }
        if let Some(image) = &self.image {
            image.validate()?;
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
            .field(
                "file_contents_bytes",
                &self.file_contents.as_ref().map_or(0, |fc| fc.bytes.len()),
            )
            .field("has_image", &self.image.is_some())
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
            file_contents: DraggedFileContents::bounded(
                Some("SECRET_IMAGE_NAME.png"),
                b"\x89PNG\r\n\x1A\nsecret_bytes".to_vec(),
            ),
            image: None,
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
        assert!(!dbg.contains("SECRET_IMAGE_NAME"));
        assert!(!dbg.contains("secret_bytes"));
        assert!(dbg.contains("text_bytes: 19"));
        assert!(dbg.contains("file_count: 1"));
        assert!(dbg.contains("file_contents_bytes: 20"));
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

    #[test]
    fn drag_image_bounds_and_validation() {
        assert!(DragImage::bounded(0, 10, 0, 0, vec![]).is_none());
        assert!(DragImage::bounded(MAX_DRAG_IMAGE_DIMENSION + 1, 1, 0, 0, vec![]).is_none());
        assert!(DragImage::bounded(2, 2, 0, 0, vec![0; 12]).is_none());
        let img = DragImage::bounded(4, 2, -5, 99, vec![0xAA; 4 * 2 * 4]).unwrap();
        assert_eq!(img.hotspot_x, 0);
        assert_eq!(img.hotspot_y, 2);
        assert_eq!(img.validate(), Ok(()));
        let debug = format!("{img:?}");
        assert!(debug.contains("bgra_bytes: 32"));
        assert!(!debug.contains("170"));
    }

    #[test]
    fn dragged_file_contents_sanitizes_names_and_infers_image_extensions() {
        assert!(DraggedFileContents::bounded(Some("a.png"), vec![]).is_none());
        let png = b"\x89PNG\r\n\x1A\n1234".to_vec();
        let fc = DraggedFileContents::bounded(Some(r"..\..\evil:name?.png"), png.clone()).unwrap();
        assert_eq!(fc.file_name, "evil_name_.png");
        assert_eq!(fc.validate(), Ok(()));

        let unnamed = DraggedFileContents::bounded(None, png).unwrap();
        assert_eq!(unnamed.file_name, "image.png");
        assert_eq!(unnamed.validate(), Ok(()));

        let reserved =
            DraggedFileContents::bounded(Some("CON"), vec![0xFF, 0xD8, 0xFF, 0xE0]).unwrap();
        assert_eq!(reserved.file_name, "_CON.jpg");
        assert_eq!(reserved.validate(), Ok(()));

        let only_contents = DraggedPayload::bounded(None, None, None, Vec::new())
            .with_file_contents(Some(reserved));
        assert!(!only_contents.is_empty());
        assert_eq!(only_contents.validate(), Ok(()));
    }
}
