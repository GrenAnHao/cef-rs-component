//! Owned, bounded inbound drag-and-drop data (files, URLs, plain text, HTML).
//!
//! Structural reference (`D:\System Dir\Downloads\CefSharp`, rev `41919703`):
//! - `CefSharp.Wpf/ChromiumWebBrowser.cs:1738-1797` (`OnDragEnter`/`Over`/`Leave`/`Drop`)
//! - `CefSharp.Wpf/Internals/WpfExtensions.cs:132-228` (`GetDragData` CF_HDROP, URL, Text, CF_HTML)

use std::path::PathBuf;

use crate::{
    outbound_drag::DragOperationMask,
    ports::{EngineError, MouseModifiers},
};

pub const MAX_INBOUND_DRAG_FILES: usize = 64;
pub const MAX_INBOUND_TEXT_BYTES: usize = 256 * 1024;
pub const MAX_INBOUND_HTML_BYTES: usize = 512 * 1024;
pub const MAX_INBOUND_URL_BYTES: usize = 8192;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InboundDragPayload {
    pub files: Vec<PathBuf>,
    pub url: Option<String>,
    pub url_title: Option<String>,
    pub text: Option<String>,
    pub html: Option<String>,
    pub html_base_url: Option<String>,
}

impl InboundDragPayload {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
            && self.url.as_ref().is_none_or(|s| s.is_empty())
            && self.text.as_ref().is_none_or(|s| s.is_empty())
            && self.html.as_ref().is_none_or(|s| s.is_empty())
    }

    pub fn validate(&self) -> Result<(), EngineError> {
        if self.is_empty() || self.files.len() > MAX_INBOUND_DRAG_FILES {
            return Err(EngineError::InvalidRequest);
        }
        for path in &self.files {
            let Some(s) = path.to_str() else {
                return Err(EngineError::InvalidRequest);
            };
            if s.is_empty() || s.contains('\0') {
                return Err(EngineError::InvalidRequest);
            }
        }
        if self
            .url
            .as_ref()
            .is_some_and(|s| s.len() > MAX_INBOUND_URL_BYTES || s.contains('\0'))
            || self
                .url_title
                .as_ref()
                .is_some_and(|s| s.len() > MAX_INBOUND_URL_BYTES || s.contains('\0'))
            || self
                .text
                .as_ref()
                .is_some_and(|s| s.len() > MAX_INBOUND_TEXT_BYTES || s.contains('\0'))
            || self
                .html
                .as_ref()
                .is_some_and(|s| s.len() > MAX_INBOUND_HTML_BYTES || s.contains('\0'))
            || self
                .html_base_url
                .as_ref()
                .is_some_and(|s| s.len() > MAX_INBOUND_URL_BYTES || s.contains('\0'))
        {
            return Err(EngineError::InvalidRequest);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboundDragCommand {
    Enter {
        payload: InboundDragPayload,
        x: i32,
        y: i32,
        modifiers: MouseModifiers,
        allowed_ops: DragOperationMask,
    },
    Over {
        x: i32,
        y: i32,
        modifiers: MouseModifiers,
        allowed_ops: DragOperationMask,
    },
    Leave,
    Drop {
        x: i32,
        y: i32,
        modifiers: MouseModifiers,
    },
}

impl InboundDragCommand {
    pub fn validate(&self) -> Result<(), EngineError> {
        match self {
            Self::Enter { payload, x, y, .. } => {
                if *x < 0 || *y < 0 {
                    return Err(EngineError::InvalidRequest);
                }
                payload.validate()
            }
            Self::Over { x, y, .. } | Self::Drop { x, y, .. } => {
                if *x < 0 || *y < 0 {
                    return Err(EngineError::InvalidRequest);
                }
                Ok(())
            }
            Self::Leave => Ok(()),
        }
    }
}

/// Extract fragment HTML and optional `SourceURL` from Windows `CF_HTML` clipboard bytes.
/// Matches `CefSharp.Wpf/Internals/WpfExtensions.cs:175-225`.
pub fn parse_cf_html_fragment(raw: &[u8]) -> Option<(String, Option<String>)> {
    if raw.is_empty() || raw.len() > MAX_INBOUND_HTML_BYTES {
        return None;
    }
    let text = String::from_utf8_lossy(raw);
    let mut start_fragment: Option<usize> = None;
    let mut end_fragment: Option<usize> = None;
    let mut source_url: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim_end_matches('\r');
        if let Some(val) = trimmed.strip_prefix("StartFragment:") {
            start_fragment = val.trim().parse::<usize>().ok();
        } else if let Some(val) = trimmed.strip_prefix("EndFragment:") {
            end_fragment = val.trim().parse::<usize>().ok();
        } else if let Some(val) = trimmed.strip_prefix("SourceURL:") {
            let url = val.trim();
            if !url.is_empty() && url.len() <= MAX_INBOUND_URL_BYTES {
                source_url = Some(url.to_owned());
            }
        }
    }
    if let (Some(start), Some(end)) = (start_fragment, end_fragment)
        && start <= end
        && end <= raw.len()
    {
        let slice = String::from_utf8_lossy(&raw[start..end]).trim().to_owned();
        if !slice.is_empty() {
            return Some((slice, source_url));
        }
    }
    let fallback = text.trim_matches('\0').trim().to_owned();
    if fallback.is_empty() {
        None
    } else {
        Some((fallback, source_url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cf_html_fragment_offsets_and_source_url() {
        let fragment = b"<b>Hello</b>";
        let prefix = b"Version:0.9\r\nStartHTML:00000097\r\nEndHTML:00000171\r\nStartFragment:";
        let body_start = "<html><body><!--StartFragment-->";
        let body_end = "<!--EndFragment--></body></html>";
        let header_placeholder = format!(
            "Version:0.9\r\nStartHTML:{:08}\r\nEndHTML:{:08}\r\nStartFragment:{:08}\r\nEndFragment:{:08}\r\nSourceURL:https://cn.bing.com/\r\n",
            0, 0, 0, 0
        );
        let start_frag = header_placeholder.len() + body_start.len();
        let end_frag = start_frag + fragment.len();
        let header = format!(
            "Version:0.9\r\nStartHTML:{:08}\r\nEndHTML:{:08}\r\nStartFragment:{:08}\r\nEndFragment:{:08}\r\nSourceURL:https://cn.bing.com/\r\n",
            header_placeholder.len(),
            end_frag + body_end.len(),
            start_frag,
            end_frag
        );
        let _ = prefix;
        let mut raw = Vec::new();
        raw.extend_from_slice(header.as_bytes());
        raw.extend_from_slice(body_start.as_bytes());
        raw.extend_from_slice(fragment);
        raw.extend_from_slice(body_end.as_bytes());

        let parsed = parse_cf_html_fragment(&raw).expect("valid CF_HTML");
        assert_eq!(parsed.0, "<b>Hello</b>");
        assert_eq!(parsed.1.as_deref(), Some("https://cn.bing.com/"));
    }

    #[test]
    fn inbound_drag_command_validates_bounds_and_non_empty() {
        let empty = InboundDragCommand::Enter {
            payload: InboundDragPayload::default(),
            x: 10,
            y: 10,
            modifiers: MouseModifiers::default(),
            allowed_ops: DragOperationMask::EVERY,
        };
        assert_eq!(empty.validate(), Err(EngineError::InvalidRequest));

        let neg = InboundDragCommand::Over {
            x: -1,
            y: 0,
            modifiers: MouseModifiers::default(),
            allowed_ops: DragOperationMask::COPY,
        };
        assert_eq!(neg.validate(), Err(EngineError::InvalidRequest));

        let ok = InboundDragCommand::Enter {
            payload: InboundDragPayload {
                text: Some("hi".into()),
                ..Default::default()
            },
            x: 0,
            y: 0,
            modifiers: MouseModifiers::default(),
            allowed_ops: DragOperationMask::EVERY,
        };
        assert_eq!(ok.validate(), Ok(()));
    }
}
