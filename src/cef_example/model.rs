//! State model, text buffers, URL normalization, and CLI parsing for the
//! GPUI desktop browser example state and host configuration.

use std::{
    collections::{BTreeMap, VecDeque},
    ffi::OsString,
    fmt,
    ops::Range,
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use rust_cef_gpui::{
    contracts::{
        BrowserId, DraggableRegionRect, NavigationSnapshot, RenderPreference, ThreadMode,
        truncate_utf8_bytes, zoom_level_to_milli,
    },
    events::BeforeUnloadRequestId,
    host_capabilities::{
        AuthRequest, CertificateErrorRequest, ClientCertificateRequest, DownloadId,
        DownloadProgress, DownloadRequest, DownloadState, HostRequestId, JsDialogKind,
        PermissionRequest,
    },
};

#[cfg(test)]
use super::assets::DEFAULT_HOME_URL;
use super::assets::format_runtime_summary;

pub const MAX_EXAMPLE_TABS: usize = 32;
pub const MAX_TEXT_BUFFER_BYTES: usize = 16 * 1024;
const MAX_TEXT_UNDO_HISTORY: usize = 32;

pub fn now_monotonic_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let start = EPOCH.get_or_init(Instant::now);
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BitmapScalingKind {
    #[default]
    Linear,
    NearestNeighbor,
}

impl BitmapScalingKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Linear => "Linear",
            Self::NearestNeighbor => "NearestNeighbor",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Self::Linear => Self::NearestNeighbor,
            Self::NearestNeighbor => Self::Linear,
        }
    }
}

pub fn is_point_draggable(regions: &[DraggableRegionRect], x_dip: i32, y_dip: i32) -> bool {
    let contains = |r: &DraggableRegionRect| -> bool {
        r.width > 0
            && r.height > 0
            && x_dip >= r.x
            && x_dip < r.x.saturating_add(r.width)
            && y_dip >= r.y
            && y_dip < r.y.saturating_add(r.height)
    };
    let in_drag = regions.iter().any(|r| r.draggable && contains(r));
    let in_no_drag = regions.iter().any(|r| !r.draggable && contains(r));
    in_drag && !in_no_drag
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExampleHostMode {
    #[default]
    Osr,
    NativeHwnd,
}

#[derive(Clone, Debug)]
pub struct ExampleConfig {
    pub bundle: PathBuf,
    pub helper: PathBuf,
    pub cache_root: PathBuf,
    pub mode: ThreadMode,
    pub backend: RenderPreference,
    pub host_mode: ExampleHostMode,
    pub context_menu: bool,
    pub initial_url: Option<String>,
    pub smoke: Option<Duration>,
    pub verify_tests: bool,
}

impl ExampleConfig {
    #[allow(dead_code)]
    pub fn parse_standalone_args(
        args: impl IntoIterator<Item = OsString>,
    ) -> Result<Option<Self>, String> {
        let mut values = BTreeMap::new();
        let mut iter = args.into_iter().peekable();
        if iter.peek().is_some_and(|a| a == "--help") {
            iter.next();
            return if iter.next().is_none() {
                Ok(None)
            } else {
                Err("--help must be used alone".into())
            };
        }
        while let Some(arg) = iter.next() {
            let name = match arg.to_str() {
                Some(
                    n @ ("--bundle" | "--helper" | "--cache-root" | "--mode" | "--backend"
                    | "--host-mode" | "--context-menu" | "--url" | "--smoke-ms"
                    | "--verify-tests"),
                ) => n.to_owned(),
                _ => return Err(format!("unknown argument {:?}", arg)),
            };
            let val = iter
                .next()
                .ok_or_else(|| format!("missing value for {name}"))?;
            if values.insert(name.clone(), val).is_some() {
                return Err(format!("duplicate option {name}"));
            }
        }

        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(PathBuf::from))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let bundle = match values.remove("--bundle") {
            Some(v) => PathBuf::from(v),
            None => exe_dir.clone(),
        };
        if !bundle.is_absolute() {
            return Err("--bundle must be an explicit absolute path".into());
        }
        let helper = match values.remove("--helper") {
            Some(v) => PathBuf::from(v),
            None => bundle.join("rust-cef-helper.exe"),
        };
        if !helper.is_absolute() {
            return Err("--helper must be an explicit absolute path".into());
        }
        let cache_root = match values.remove("--cache-root") {
            Some(v) => PathBuf::from(v),
            None => default_cache_root(&bundle),
        };
        if !cache_root.is_absolute() {
            return Err("--cache-root must be an explicit absolute path".into());
        }

        let mode = match values.remove("--mode").as_ref().and_then(|v| v.to_str()) {
            None | Some("Threaded") => ThreadMode::Threaded,
            Some("Integrated") => ThreadMode::Integrated,
            Some(_) => return Err("--mode must name Integrated or Threaded".into()),
        };
        let backend = match values.remove("--backend").as_ref().and_then(|v| v.to_str()) {
            None | Some("D3d11") => RenderPreference::D3d11Required,
            Some("Software") => RenderPreference::SoftwareRequired,
            Some(_) => return Err("--backend must name Software or D3d11".into()),
        };
        let host_mode = match values
            .remove("--host-mode")
            .as_ref()
            .and_then(|v| v.to_str())
        {
            None | Some("osr") => ExampleHostMode::Osr,
            Some("native-hwnd") => ExampleHostMode::NativeHwnd,
            Some(_) => return Err("--host-mode must be osr or native-hwnd".into()),
        };
        let context_menu = match values
            .remove("--context-menu")
            .as_ref()
            .and_then(|v| v.to_str())
        {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => return Err("--context-menu must be true or false".into()),
        };
        let initial_url = values
            .remove("--url")
            .map(|v| {
                let raw = v
                    .into_string()
                    .map_err(|_| "--url must be valid UTF-8".to_owned())?;
                normalize_user_url(&raw).ok_or_else(|| "unsupported --url".to_owned())
            })
            .transpose()?;
        let smoke = values
            .remove("--smoke-ms")
            .map(|v| {
                v.to_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .filter(|ms| (100..=30000).contains(ms))
                    .map(Duration::from_millis)
                    .ok_or_else(|| "--smoke-ms must be an integer in 100..30000".to_owned())
            })
            .transpose()?;

        let verify_tests = match values
            .remove("--verify-tests")
            .as_ref()
            .and_then(|v| v.to_str())
        {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => return Err("--verify-tests must be true or false".into()),
        };

        Ok(Some(Self {
            bundle,
            helper,
            cache_root,
            mode,
            backend,
            host_mode,
            context_menu,
            initial_url,
            smoke,
            verify_tests,
        }))
    }
}

#[allow(dead_code)]
pub const STANDALONE_USAGE: &str = "gpui-cef-example [--bundle <dir>] [--helper <exe>] [--cache-root <dir>] [--mode Threaded|Integrated] [--backend D3d11|Software] [--host-mode osr|native-hwnd] [--context-menu true|false] [--url <url>] [--smoke-ms 100..30000] [--verify-tests true|false]";

#[allow(dead_code)]
fn default_cache_root(bundle: &PathBuf) -> PathBuf {
    let cargo_root = PathBuf::from(r"E:\cargo-target\gpui-cef-poc\tmp");
    if cargo_root.exists() {
        cargo_root.join("cef-example-cache")
    } else {
        bundle
            .parent()
            .unwrap_or(bundle.as_path())
            .join("gpui-cef-example-cache")
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActiveInput {
    #[default]
    None,
    Address,
    ExecuteJs,
    EvaluateJs,
    JsPrompt,
    FindQuery,
    AuthUser,
    AuthPass,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuCategory {
    File,
    View,
    Tests,
    ChromeUrls,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WordCharClass {
    Whitespace,
    Word,
    Punct,
}

fn classify_word_char(ch: char) -> WordCharClass {
    if ch.is_whitespace() {
        WordCharClass::Whitespace
    } else if ch.is_alphanumeric() || ch == '_' {
        WordCharClass::Word
    } else {
        WordCharClass::Punct
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextBuffer {
    pub text: String,
    pub selected_range: Range<usize>,
    pub selection_reversed: bool,
    pub marked_range: Option<Range<usize>>,
    pub scroll_x: f32,
    pub scroll_y: f32,
    pub last_interaction_ms: u64,
    undo_stack: Vec<(String, Range<usize>)>,
}

impl TextBuffer {
    pub fn new(text: impl Into<String>) -> Self {
        let raw = text.into();
        let initial = truncate_utf8_bytes(&raw, MAX_TEXT_BUFFER_BYTES);
        let len = initial.len();
        Self {
            text: initial,
            selected_range: len..len,
            selection_reversed: false,
            marked_range: None,
            scroll_x: 0.0,
            scroll_y: 0.0,
            last_interaction_ms: now_monotonic_ms(),
            undo_stack: Vec::new(),
        }
    }

    pub fn touch(&mut self) {
        self.last_interaction_ms = now_monotonic_ms();
    }

    fn clamp_char_boundary(&self, offset: usize) -> usize {
        let mut idx = offset.min(self.text.len());
        while idx > 0 && !self.text.is_char_boundary(idx) {
            idx -= 1;
        }
        idx
    }

    fn sanitize_range(&self, range: Range<usize>) -> Range<usize> {
        let a = self.clamp_char_boundary(range.start);
        let b = self.clamp_char_boundary(range.end);
        a.min(b)..a.max(b)
    }

    pub fn cursor_offset(&self) -> usize {
        let raw = if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        };
        self.clamp_char_boundary(raw)
    }

    pub fn anchor_offset(&self) -> usize {
        let raw = if self.selection_reversed {
            self.selected_range.end
        } else {
            self.selected_range.start
        };
        self.clamp_char_boundary(raw)
    }

    pub fn selected_text(&self) -> &str {
        let range = self.sanitize_range(self.selected_range.clone());
        &self.text[range]
    }

    #[cfg(test)]
    pub fn is_selected_all(&self) -> bool {
        !self.text.is_empty() && self.selected_range == (0..self.text.len())
    }

    pub fn focus_select_all(&mut self) {
        self.selected_range = 0..self.text.len();
        self.selection_reversed = false;
        self.marked_range = None;
        self.touch();
    }

    #[cfg(test)]
    pub fn collapse_to_end(&mut self) {
        let len = self.text.len();
        self.selected_range = len..len;
        self.selection_reversed = false;
        self.marked_range = None;
        self.touch();
    }

    pub fn set_from_external(&mut self, next: &str) {
        self.text.clear();
        self.text
            .push_str(&truncate_utf8_bytes(next, MAX_TEXT_BUFFER_BYTES));
        let len = self.text.len();
        self.selected_range = len..len;
        self.selection_reversed = false;
        self.marked_range = None;
        self.scroll_x = 0.0;
        self.scroll_y = 0.0;
        self.undo_stack.clear();
    }

    pub fn move_to(&mut self, offset: usize) {
        let clamped = self.clamp_char_boundary(offset);
        self.selected_range = clamped..clamped;
        self.selection_reversed = false;
        self.touch();
    }

    pub fn select_to(&mut self, offset: usize) {
        let anchor = self.anchor_offset();
        let active = self.clamp_char_boundary(offset);
        if active < anchor {
            self.selected_range = active..anchor;
            self.selection_reversed = true;
        } else {
            self.selected_range = anchor..active;
            self.selection_reversed = false;
        }
        self.touch();
    }

    pub fn prev_char_boundary(&self, offset: usize) -> usize {
        let cur = self.clamp_char_boundary(offset);
        self.text[..cur]
            .char_indices()
            .next_back()
            .map_or(0, |(idx, _)| idx)
    }

    pub fn next_char_boundary(&self, offset: usize) -> usize {
        let cur = self.clamp_char_boundary(offset);
        self.text[cur..]
            .chars()
            .next()
            .map_or(self.text.len(), |ch| cur + ch.len_utf8())
    }

    pub fn line_start_boundary(&self, offset: usize) -> usize {
        let cur = self.clamp_char_boundary(offset);
        self.text[..cur].rfind('\n').map_or(0, |idx| idx + 1)
    }

    pub fn line_end_boundary(&self, offset: usize) -> usize {
        let cur = self.clamp_char_boundary(offset);
        self.text[cur..]
            .find('\n')
            .map_or(self.text.len(), |rel| cur + rel)
    }

    pub fn prev_word_boundary(&self, offset: usize) -> usize {
        let cur = self.clamp_char_boundary(offset);
        if cur == 0 {
            return 0;
        }
        let mut iter = self.text[..cur].char_indices().rev().peekable();
        if let Some(&(_, '\n')) = iter.peek() {
            return cur - 1;
        }
        while let Some(&(_, ch)) = iter.peek() {
            if ch == '\n' || !ch.is_whitespace() {
                break;
            }
            iter.next();
        }
        let Some((idx, first)) = iter.next() else {
            return self.line_start_boundary(cur);
        };
        if first == '\n' {
            return idx + 1;
        }
        let target_class = classify_word_char(first);
        let mut start = idx;
        for (i, ch) in iter {
            if ch == '\n' || classify_word_char(ch) != target_class {
                break;
            }
            start = i;
        }
        start
    }

    pub fn next_word_boundary(&self, offset: usize) -> usize {
        let cur = self.clamp_char_boundary(offset);
        if cur >= self.text.len() {
            return self.text.len();
        }
        let mut iter = self.text[cur..].char_indices().peekable();
        if let Some(&(_, '\n')) = iter.peek() {
            return cur + 1;
        }
        while let Some(&(_, ch)) = iter.peek() {
            if ch == '\n' || !ch.is_whitespace() {
                break;
            }
            iter.next();
        }
        let Some((rel_start, first)) = iter.next() else {
            return self.line_end_boundary(cur);
        };
        if first == '\n' {
            return cur + rel_start;
        }
        let target_class = classify_word_char(first);
        let mut end = cur + rel_start + first.len_utf8();
        for (rel, ch) in iter {
            if ch == '\n' || classify_word_char(ch) != target_class {
                return cur + rel;
            }
            end = cur + rel + ch.len_utf8();
        }
        end
    }

    pub fn select_word_at(&mut self, offset: usize) {
        if self.text.is_empty() {
            self.move_to(0);
            return;
        }
        let line_start = self.line_start_boundary(offset);
        let line_end = self.line_end_boundary(offset);
        let line = &self.text[line_start..line_end];
        if line.is_empty() {
            self.move_to(line_start);
            return;
        }
        let rel = self.clamp_char_boundary(offset).saturating_sub(line_start);
        let probe = if rel < line.len() {
            rel
        } else {
            line.char_indices().next_back().map_or(0, |(i, _)| i)
        };
        let Some(pivot) = line[probe..].chars().next() else {
            self.move_to(line_start);
            return;
        };
        let target_class = classify_word_char(pivot);
        let mut start = probe;
        for (i, ch) in line[..probe].char_indices().rev() {
            if classify_word_char(ch) != target_class {
                break;
            }
            start = i;
        }
        let mut end = probe + pivot.len_utf8();
        for (rel_after, ch) in line[end..].char_indices() {
            if classify_word_char(ch) != target_class {
                end += rel_after;
                self.selected_range = (line_start + start)..(line_start + end);
                self.selection_reversed = false;
                self.marked_range = None;
                self.touch();
                return;
            }
        }
        self.selected_range = (line_start + start)..line_end;
        self.selection_reversed = false;
        self.marked_range = None;
        self.touch();
    }

    pub fn select_line_at(&mut self, offset: usize, multiline: bool) {
        if !multiline {
            self.focus_select_all();
            return;
        }
        let start = self.line_start_boundary(offset);
        let end = self.line_end_boundary(offset);
        self.selected_range = start..end;
        self.selection_reversed = false;
        self.marked_range = None;
        self.touch();
    }

    fn push_undo(&mut self) {
        if let Some((last_text, last_sel)) = self.undo_stack.last()
            && *last_text == self.text
            && *last_sel == self.selected_range
        {
            return;
        }
        if self.undo_stack.len() >= MAX_TEXT_UNDO_HISTORY {
            self.undo_stack.remove(0);
        }
        self.undo_stack
            .push((self.text.clone(), self.selected_range.clone()));
    }

    pub fn undo(&mut self) -> bool {
        let Some((prev_text, prev_sel)) = self.undo_stack.pop() else {
            return false;
        };
        self.text = prev_text;
        self.selected_range = self.sanitize_range(prev_sel);
        self.selection_reversed = false;
        self.marked_range = None;
        self.touch();
        true
    }

    fn filter_incoming_text(incoming: &str, multiline: bool) -> String {
        let mut out = String::with_capacity(incoming.len());
        let mut chars = incoming.chars().peekable();
        while let Some(ch) = chars.next() {
            if multiline {
                if ch == '\r' {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    out.push('\n');
                } else if ch == '\t' {
                    out.push_str("  ");
                } else if ch == '\n' || !ch.is_control() {
                    out.push(ch);
                }
            } else if ch == '\r' {
                continue;
            } else if ch == '\n' || ch == '\t' {
                out.push(' ');
            } else if !ch.is_control() {
                out.push(ch);
            }
        }
        out
    }

    fn replace_byte_range(
        &mut self,
        range: Range<usize>,
        incoming: &str,
        multiline: bool,
        record_undo: bool,
    ) {
        let r = self.sanitize_range(range);
        let clean = Self::filter_incoming_text(incoming, multiline);
        let removed_bytes = r.end - r.start;
        let base_len = self.text.len().saturating_sub(removed_bytes);
        let room = MAX_TEXT_BUFFER_BYTES.saturating_sub(base_len);
        let limited = truncate_utf8_bytes(&clean, room);
        if record_undo && (!r.is_empty() || !limited.is_empty()) {
            self.push_undo();
        }
        self.text.replace_range(r.clone(), &limited);
        let next_caret = r.start + limited.len();
        self.selected_range = next_caret..next_caret;
        self.selection_reversed = false;
        self.marked_range = None;
        self.touch();
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn insert_str(&mut self, incoming: &str) {
        self.insert_str_filtered(incoming, false);
    }

    pub fn insert_str_filtered(&mut self, incoming: &str, multiline: bool) {
        let target = self
            .marked_range
            .clone()
            .unwrap_or_else(|| self.sanitize_range(self.selected_range.clone()));
        self.replace_byte_range(target, incoming, multiline, true);
    }

    pub fn backspace(&mut self) {
        let sel = self.sanitize_range(self.selected_range.clone());
        let range = if !sel.is_empty() {
            sel
        } else {
            let cur = self.cursor_offset();
            let prev = self.prev_char_boundary(cur);
            prev..cur
        };
        if !range.is_empty() {
            self.replace_byte_range(range, "", false, true);
        } else {
            self.touch();
        }
    }

    pub fn delete_forward(&mut self) {
        let sel = self.sanitize_range(self.selected_range.clone());
        let range = if !sel.is_empty() {
            sel
        } else {
            let cur = self.cursor_offset();
            let next = self.next_char_boundary(cur);
            cur..next
        };
        if !range.is_empty() {
            self.replace_byte_range(range, "", false, true);
        } else {
            self.touch();
        }
    }

    pub fn delete_word_backward(&mut self) {
        let sel = self.sanitize_range(self.selected_range.clone());
        let range = if !sel.is_empty() {
            sel
        } else {
            let cur = self.cursor_offset();
            let prev = self.prev_word_boundary(cur);
            prev..cur
        };
        if !range.is_empty() {
            self.replace_byte_range(range, "", false, true);
        } else {
            self.touch();
        }
    }

    pub fn delete_word_forward(&mut self) {
        let sel = self.sanitize_range(self.selected_range.clone());
        let range = if !sel.is_empty() {
            sel
        } else {
            let cur = self.cursor_offset();
            let next = self.next_word_boundary(cur);
            cur..next
        };
        if !range.is_empty() {
            self.replace_byte_range(range, "", false, true);
        } else {
            self.touch();
        }
    }

    pub fn text_len_utf16(&self) -> usize {
        self.text.encode_utf16().count()
    }

    pub fn offset_to_utf16(&self, utf8_offset: usize) -> usize {
        let clamped = self.clamp_char_boundary(utf8_offset);
        self.text[..clamped].encode_utf16().count()
    }

    pub fn offset_from_utf16(&self, utf16_offset: usize) -> usize {
        let mut utf8 = 0usize;
        let mut utf16 = 0usize;
        for ch in self.text.chars() {
            if utf16 >= utf16_offset {
                break;
            }
            utf16 += ch.len_utf16();
            utf8 += ch.len_utf8();
        }
        utf8
    }

    pub fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    pub fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        let a = self.offset_from_utf16(range_utf16.start);
        let b = self.offset_from_utf16(range_utf16.end);
        a.min(b)..a.max(b)
    }

    pub fn unmark(&mut self) {
        self.marked_range = None;
        self.touch();
    }

    pub fn replace_in_utf16_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        multiline: bool,
    ) {
        let target = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.sanitize_range(self.selected_range.clone()));
        self.replace_byte_range(target, new_text, multiline, true);
    }

    pub fn replace_and_mark_in_utf16_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_utf16: Option<Range<usize>>,
        multiline: bool,
    ) {
        let target = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.sanitize_range(self.selected_range.clone()));
        let r = self.sanitize_range(target);
        let clean = Self::filter_incoming_text(new_text, multiline);
        let removed_bytes = r.end - r.start;
        let base_len = self.text.len().saturating_sub(removed_bytes);
        let room = MAX_TEXT_BUFFER_BYTES.saturating_sub(base_len);
        let limited = truncate_utf8_bytes(&clean, room);
        if self.marked_range.is_none() && (!r.is_empty() || !limited.is_empty()) {
            self.push_undo();
        }
        let insertion_start_utf16 = self.offset_to_utf16(r.start);
        self.text.replace_range(r.clone(), &limited);
        let marked_end = r.start + limited.len();
        self.marked_range = if !limited.is_empty() {
            Some(r.start..marked_end)
        } else {
            None
        };
        let inserted_len_utf16 = limited.encode_utf16().count();
        self.selected_range = match new_selected_utf16 {
            Some(sel) => {
                let lo = sel.start.min(sel.end).min(inserted_len_utf16);
                let hi = sel.start.max(sel.end).min(inserted_len_utf16);
                self.range_from_utf16(&(insertion_start_utf16 + lo..insertion_start_utf16 + hi))
            }
            None => marked_end..marked_end,
        };
        self.selection_reversed = false;
        self.touch();
    }

    pub fn wipe_secret(&mut self) {
        let mut bytes = std::mem::take(&mut self.text).into_bytes();
        bytes.fill(0);
        std::hint::black_box(&bytes);
        drop(bytes);
        for (old, _) in self.undo_stack.drain(..) {
            let mut old_bytes = old.into_bytes();
            old_bytes.fill(0);
            std::hint::black_box(&old_bytes);
        }
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        self.scroll_x = 0.0;
        self.scroll_y = 0.0;
        self.touch();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingFindFollowUp {
    StopClearSelection,
    Search {
        forward: bool,
        force_find_next: bool,
    },
}

#[derive(Clone, Debug)]
pub struct TabFindState {
    pub open: bool,
    pub open_flag: Arc<AtomicBool>,
    pub query: TextBuffer,
    pub match_case: bool,
    pub active_ordinal: i32,
    pub matches: i32,
    pub search_in_progress: bool,
    pub pending_after_finish: Option<PendingFindFollowUp>,
    pub latest_operation_seq: u64,
    pub quarantined: bool,
    pub last_error: Option<String>,
    pub last_submitted_query: String,
    pub last_submitted_match_case: bool,
}

impl Default for TabFindState {
    fn default() -> Self {
        Self {
            open: false,
            open_flag: Arc::new(AtomicBool::new(false)),
            query: TextBuffer::new(""),
            match_case: false,
            active_ordinal: 0,
            matches: 0,
            search_in_progress: false,
            pending_after_finish: None,
            latest_operation_seq: 0,
            quarantined: false,
            last_error: None,
            last_submitted_query: String::new(),
            last_submitted_match_case: false,
        }
    }
}

impl TabFindState {
    pub fn set_open(&mut self, open: bool) {
        self.open = open;
        self.open_flag.store(open, Ordering::Release);
        if !open {
            if self.search_in_progress {
                self.pending_after_finish = Some(PendingFindFollowUp::StopClearSelection);
            } else {
                self.pending_after_finish = None;
            }
        }
    }

    #[allow(dead_code)]
    pub fn sync_open_flag(&self) {
        self.open_flag.store(self.open, Ordering::Release);
    }

    #[allow(dead_code)]
    pub fn reset_results(&mut self) {
        self.active_ordinal = 0;
        self.matches = 0;
        self.search_in_progress = false;
        self.pending_after_finish = None;
        self.last_error = None;
    }

    pub fn on_document_navigated(&mut self) {
        self.last_submitted_query.clear();
        if !self.search_in_progress {
            self.active_ordinal = 0;
            self.matches = 0;
            if !self.quarantined {
                self.last_error = None;
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DownloadInfoView {
    pub last_action: String,
    pub browser_id: Option<BrowserId>,
    pub download_id: Option<DownloadId>,
    pub id: Option<u64>,
    pub url: String,
    pub original_url: String,
    pub suggested_file_name: String,
    pub mime_type: String,
    pub full_path: String,
    pub is_in_progress: bool,
    pub is_complete: bool,
    pub is_cancelled: bool,
    pub cancelled_reason: Option<String>,
    pub current_speed: i64,
    pub percent_complete: Option<i32>,
    pub total_bytes: Option<u64>,
    pub received_bytes: u64,
}

#[allow(dead_code)]
pub type DownloadEntry = DownloadInfoView;

#[derive(Clone, Debug, PartialEq)]
pub struct PendingJsDialog {
    pub id: HostRequestId,
    pub kind: JsDialogKind,
    pub origin_url: String,
    pub message: String,
    pub prompt_input: TextBuffer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPermissionModal {
    pub tab_id: u64,
    pub browser_id: BrowserId,
    pub request: PermissionRequest,
}

#[derive(Clone)]
pub struct PendingAuthModal {
    pub tab_id: u64,
    pub browser_id: BrowserId,
    pub request: AuthRequest,
    pub username_input: TextBuffer,
    pub password_input: TextBuffer,
}

impl fmt::Debug for PendingAuthModal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingAuthModal")
            .field("tab_id", &self.tab_id)
            .field("browser_id", &self.browser_id)
            .field("request", &self.request)
            .field("username_input", &"<redacted>")
            .field("password_input", &"<redacted>")
            .finish()
    }
}

impl PendingAuthModal {
    pub fn wipe_inputs(&mut self) {
        self.username_input.wipe_secret();
        self.password_input.wipe_secret();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingCertKind {
    ServerError(CertificateErrorRequest),
    ClientSelect(ClientCertificateRequest),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingCertModal {
    pub tab_id: u64,
    pub browser_id: BrowserId,
    pub kind: PendingCertKind,
}

impl PendingCertModal {
    pub fn request_id(&self) -> HostRequestId {
        match &self.kind {
            PendingCertKind::ServerError(r) => r.id,
            PendingCertKind::ClientSelect(r) => r.id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingBeforeUnloadModal {
    pub tab_id: u64,
    pub browser_id: BrowserId,
    pub request: BeforeUnloadRequestId,
    pub is_reload: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoticeDialog {
    pub title: String,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct ExampleTabModel {
    pub id: u64,
    pub title: String,
    pub address: String,
    pub address_input: TextBuffer,
    pub status_message: String,
    pub output_message: String,
    pub is_loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub loading_progress_per_mille: u16,
    pub zoom_level_milli: i32,
    pub audio_muted: bool,
    pub devtools_open: bool,
    pub show_sidebar: bool,
    pub show_download_info: bool,
    pub watermark_opacity: f32,
    pub render_angle_deg: f32,
    pub bitmap_scaling: BitmapScalingKind,
    pub execute_js_input: TextBuffer,
    pub evaluate_js_input: TextBuffer,
    pub evaluate_js_result: String,
    pub draggable_region_count: usize,
    pub draggable_regions: Vec<DraggableRegionRect>,
    pub v8_context_count: usize,
    pub focused_node_tag: String,
    pub download: DownloadInfoView,
    pub find: TabFindState,
    pub closing_pending: bool,
}

impl ExampleTabModel {
    pub fn new(id: u64, initial_url: String, show_sidebar: bool) -> Self {
        Self {
            id,
            title: "Loading...".into(),
            address_input: TextBuffer::new(initial_url.clone()),
            address: initial_url,
            status_message: String::new(),
            output_message: format_runtime_summary(""),
            is_loading: true,
            can_go_back: false,
            can_go_forward: false,
            loading_progress_per_mille: 0,
            zoom_level_milli: 0,
            audio_muted: false,
            devtools_open: false,
            show_sidebar,
            show_download_info: false,
            watermark_opacity: 0.95,
            render_angle_deg: 0.0,
            bitmap_scaling: BitmapScalingKind::Linear,
            execute_js_input: TextBuffer::new("document.body.style.backgroundColor = 'red';"),
            evaluate_js_input: TextBuffer::new("1 + 1"),
            evaluate_js_result: String::new(),
            draggable_region_count: 0,
            draggable_regions: Vec::new(),
            v8_context_count: 0,
            focused_node_tag: String::new(),
            download: DownloadInfoView {
                last_action: "None".into(),
                ..DownloadInfoView::default()
            },
            find: TabFindState::default(),
            closing_pending: false,
        }
    }

    pub fn apply_snapshot(&mut self, nav: &NavigationSnapshot, editing_address: bool) {
        if !nav.title.is_empty() {
            self.title = nav.title.clone();
        }
        if !nav.address.is_empty() && nav.address != self.address {
            self.address = nav.address.clone();
            if !editing_address {
                self.address_input.set_from_external(&nav.address);
            }
        }
        self.is_loading = nav.is_loading;
        self.can_go_back = nav.can_go_back;
        self.can_go_forward = nav.can_go_forward;
        self.loading_progress_per_mille = nav.loading_progress_per_mille.min(1000);
        self.audio_muted = nav.audio_muted;
        if nav.zoom_level_milli != self.zoom_level_milli {
            self.zoom_level_milli = nav.zoom_level_milli;
        }
        if !nav.status_message.is_empty() {
            self.status_message = nav.status_message.clone();
        }
    }

    pub fn sync_zoom_from_view(&mut self, level: f64) {
        if let Ok(milli) = zoom_level_to_milli(level) {
            self.zoom_level_milli = milli;
        }
    }

    pub fn apply_download_request(&mut self, browser_id: BrowserId, req: &DownloadRequest) {
        self.download.last_action = "OnBeforeDownload".into();
        self.download.browser_id = Some(browser_id);
        self.download.download_id = Some(req.id);
        self.download.id = Some(req.id.sequence);
        self.download.url = req.url.clone();
        self.download.original_url = req.original_url.clone();
        self.download.suggested_file_name = req.suggested_name.clone();
        self.download.mime_type = req.mime_type.clone();
        self.download.total_bytes = req.total_bytes;
        self.download.received_bytes = 0;
        self.download.current_speed = 0;
        self.download.percent_complete = None;
        self.download.is_in_progress = true;
        self.download.is_complete = false;
        self.download.is_cancelled = false;
        self.download.cancelled_reason = None;
    }

    pub fn apply_download_progress(&mut self, p: &DownloadProgress) {
        self.download.last_action = "OnDownloadUpdated".into();
        self.download.browser_id = Some(p.id.browser);
        self.download.download_id = Some(p.id);
        self.download.id = Some(p.id.sequence);
        self.download.received_bytes = p.received_bytes;
        self.download.total_bytes = p.total_bytes;
        self.download.current_speed = p.current_speed;
        self.download.percent_complete = (p.percent_complete >= 0).then_some(p.percent_complete);
        self.download.is_in_progress = p.state == DownloadState::InProgress;
        self.download.is_complete = p.state == DownloadState::Complete;
        self.download.is_cancelled = matches!(
            p.state,
            DownloadState::Canceled | DownloadState::Interrupted
        );
        if self.download.is_cancelled {
            self.download
                .cancelled_reason
                .get_or_insert_with(|| format!("{:?}", p.state));
        } else if self.download.is_in_progress || self.download.is_complete {
            self.download.cancelled_reason = None;
        }
        if let Some(full) = &p.full_path {
            self.download.full_path = full.to_string_lossy().into_owned();
        }
    }

    pub fn mark_download_cancelled(&mut self, reason: impl Into<String>) {
        self.download.is_in_progress = false;
        self.download.is_cancelled = true;
        self.download.cancelled_reason = Some(reason.into());
        self.download.last_action = "Cancelled".into();
    }
}

#[derive(Default)]
pub struct ExampleAppModel {
    pub tabs: Vec<ExampleTabModel>,
    pub active_tab: Option<u64>,
    pub active_input: ActiveInput,
    pub open_menu: Option<MenuCategory>,
    pub ask_save_as: bool,
    pub active_native_file_dialog: Option<HostRequestId>,
    pub active_save_as_download: Option<DownloadId>,
    pub pending_js_dialog: Option<PendingJsDialog>,
    pub pending_permission: VecDeque<PendingPermissionModal>,
    pub pending_auth: VecDeque<PendingAuthModal>,
    pub pending_cert: VecDeque<PendingCertModal>,
    pub pending_before_unload: VecDeque<PendingBeforeUnloadModal>,
    pub notice_dialog: Option<NoticeDialog>,
    pub window_close_pending: bool,
    pub quit_requested: bool,
    pub active_find_open: Arc<AtomicBool>,
}

impl ExampleAppModel {
    pub fn add_tab(&mut self, id: u64, url: String, show_sidebar: bool) -> bool {
        if self.tabs.len() >= MAX_EXAMPLE_TABS || self.tabs.iter().any(|t| t.id == id) {
            return false;
        }
        self.tabs.push(ExampleTabModel::new(id, url, show_sidebar));
        self.active_tab = Some(id);
        self.sync_active_find_flag();
        true
    }

    pub fn switch_tab(&mut self, id: u64) -> bool {
        if self.tabs.iter().any(|t| t.id == id) {
            self.active_tab = Some(id);
            self.active_input = ActiveInput::None;
            self.open_menu = None;
            self.sync_active_find_flag();
            true
        } else {
            false
        }
    }

    pub fn close_tab(&mut self, id: u64) -> bool {
        let Some(idx) = self.tabs.iter().position(|t| t.id == id) else {
            return false;
        };
        let browser_id = self.tabs[idx].download.browser_id;
        self.tabs.remove(idx);
        self.clear_modals_for_tab(id, browser_id);
        if self.tabs.is_empty() {
            self.active_tab = None;
            self.window_close_pending = false;
            self.quit_requested = true;
            self.sync_active_find_flag();
            return true;
        }
        if self.active_tab == Some(id) {
            let next_idx = idx.min(self.tabs.len() - 1);
            self.active_tab = Some(self.tabs[next_idx].id);
            self.active_input = ActiveInput::None;
        }
        self.sync_active_find_flag();
        true
    }

    pub fn sync_active_find_flag(&self) {
        let open = self.active_tab().is_some_and(|t| t.find.open);
        self.active_find_open.store(open, Ordering::Release);
    }

    pub fn has_blocking_modal(&self) -> bool {
        self.pending_js_dialog.is_some()
            || self.notice_dialog.is_some()
            || self.active_native_file_dialog.is_some()
            || self.active_save_as_download.is_some()
            || !self.pending_permission.is_empty()
            || !self.pending_auth.is_empty()
            || !self.pending_cert.is_empty()
            || !self.pending_before_unload.is_empty()
            || self.open_menu.is_some()
    }

    pub fn enqueue_permission(&mut self, modal: PendingPermissionModal) {
        if !self
            .pending_permission
            .iter()
            .any(|m| m.request.id == modal.request.id)
        {
            self.pending_permission.push_back(modal);
        }
    }

    pub fn cancel_permission(&mut self, id: HostRequestId) -> bool {
        let before = self.pending_permission.len();
        self.pending_permission.retain(|m| m.request.id != id);
        self.pending_permission.len() != before
    }

    pub fn pop_front_permission(&mut self) -> Option<PendingPermissionModal> {
        self.pending_permission.pop_front()
    }

    pub fn enqueue_auth(&mut self, modal: PendingAuthModal) {
        if !self
            .pending_auth
            .iter()
            .any(|m| m.request.id == modal.request.id)
        {
            self.pending_auth.push_back(modal);
        }
    }

    pub fn cancel_auth(&mut self, id: HostRequestId) -> bool {
        let mut removed = false;
        let mut kept = VecDeque::with_capacity(self.pending_auth.len());
        while let Some(mut modal) = self.pending_auth.pop_front() {
            if modal.request.id == id {
                modal.wipe_inputs();
                removed = true;
            } else {
                kept.push_back(modal);
            }
        }
        self.pending_auth = kept;
        if removed
            && matches!(
                self.active_input,
                ActiveInput::AuthUser | ActiveInput::AuthPass
            )
        {
            self.active_input = ActiveInput::None;
        }
        removed
    }

    pub fn pop_front_auth(&mut self) -> Option<PendingAuthModal> {
        let popped = self.pending_auth.pop_front();
        if popped.is_some()
            && matches!(
                self.active_input,
                ActiveInput::AuthUser | ActiveInput::AuthPass
            )
        {
            self.active_input = ActiveInput::None;
        }
        popped
    }

    pub fn enqueue_cert(&mut self, modal: PendingCertModal) {
        let id = modal.request_id();
        if !self.pending_cert.iter().any(|m| m.request_id() == id) {
            self.pending_cert.push_back(modal);
        }
    }

    pub fn cancel_cert(&mut self, id: HostRequestId) -> bool {
        let before = self.pending_cert.len();
        self.pending_cert.retain(|m| m.request_id() != id);
        self.pending_cert.len() != before
    }

    pub fn pop_front_cert(&mut self) -> Option<PendingCertModal> {
        self.pending_cert.pop_front()
    }

    pub fn enqueue_before_unload(&mut self, modal: PendingBeforeUnloadModal) {
        if !self
            .pending_before_unload
            .iter()
            .any(|m| m.browser_id == modal.browser_id && m.request == modal.request)
        {
            self.pending_before_unload.push_back(modal);
        }
    }

    pub fn pop_front_before_unload(&mut self) -> Option<PendingBeforeUnloadModal> {
        self.pending_before_unload.pop_front()
    }

    pub fn clear_active_file_dialog_if(&mut self, id: HostRequestId) -> bool {
        if self.active_native_file_dialog == Some(id) {
            self.active_native_file_dialog = None;
            true
        } else {
            false
        }
    }

    pub fn clear_modals_for_tab(&mut self, tab_id: u64, browser_id: Option<BrowserId>) {
        let matches_target =
            |tid: u64, bid: BrowserId| tid == tab_id || browser_id.is_some_and(|b| b == bid);
        self.pending_permission
            .retain(|m| !matches_target(m.tab_id, m.browser_id));
        let mut kept_auth = VecDeque::with_capacity(self.pending_auth.len());
        let mut wiped_active_auth = false;
        while let Some(mut m) = self.pending_auth.pop_front() {
            if matches_target(m.tab_id, m.browser_id) {
                m.wipe_inputs();
                wiped_active_auth = true;
            } else {
                kept_auth.push_back(m);
            }
        }
        self.pending_auth = kept_auth;
        if wiped_active_auth
            && matches!(
                self.active_input,
                ActiveInput::AuthUser | ActiveInput::AuthPass
            )
        {
            self.active_input = ActiveInput::None;
        }
        self.pending_cert
            .retain(|m| !matches_target(m.tab_id, m.browser_id));
        self.pending_before_unload
            .retain(|m| !matches_target(m.tab_id, m.browser_id));
        if let Some(bid) = browser_id {
            if self
                .active_native_file_dialog
                .is_some_and(|id| id.browser == bid)
            {
                self.active_native_file_dialog = None;
            }
            if self
                .active_save_as_download
                .is_some_and(|id| id.browser == bid)
            {
                self.active_save_as_download = None;
            }
            if self
                .pending_js_dialog
                .as_ref()
                .is_some_and(|d| d.id.browser == bid)
            {
                self.pending_js_dialog = None;
            }
        }
    }

    pub fn active_tab(&self) -> Option<&ExampleTabModel> {
        let id = self.active_tab?;
        self.tabs.iter().find(|t| t.id == id)
    }

    pub fn tab(&self, id: u64) -> Option<&ExampleTabModel> {
        self.tabs.iter().find(|t| t.id == id)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut ExampleTabModel> {
        let id = self.active_tab?;
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    pub fn tab_mut(&mut self, id: u64) -> Option<&mut ExampleTabModel> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }
}

pub fn normalize_user_url(input: &str) -> Option<String> {
    if input.bytes().any(|b| b == 0 || b.is_ascii_control()) || input.len() > MAX_TEXT_BUFFER_BYTES
    {
        return None;
    }
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "about:blank" {
        return Some("about:blank".into());
    }
    if trimmed.starts_with("https://")
        || trimmed.starts_with("http://")
        || trimmed.starts_with("custom://")
        || trimmed.starts_with("test://")
        || trimmed.starts_with("chrome://")
    {
        return Some(trimmed.to_owned());
    }
    if !trimmed.contains("://") && !trimmed.contains(char::is_whitespace) {
        let authority = trimmed.split('/').next().unwrap_or("");
        if let Some((host, port)) = authority.split_once(':') {
            if host.is_empty() || port.is_empty() || port.parse::<u16>().is_err() {
                return None;
            }
        }
        return Some(format!("https://{trimmed}"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_normalization_handles_cef_rs_gpui_and_web_urls() {
        assert_eq!(
            normalize_user_url("  https://cef-rs-gpui.example/home.html "),
            Some("https://cef-rs-gpui.example/home.html".into())
        );
        assert_eq!(
            normalize_user_url("custom://cef-rs-gpui/SchemeTest.html"),
            Some("custom://cef-rs-gpui/SchemeTest.html".into())
        );
        assert_eq!(
            normalize_user_url("cef-rs-gpui.example/BindingTest.html"),
            Some("https://cef-rs-gpui.example/BindingTest.html".into())
        );
        assert_eq!(
            normalize_user_url("chrome://chrome-urls"),
            Some("chrome://chrome-urls".into())
        );
        assert_eq!(normalize_user_url("javascript:alert(1)"), None);
        assert_eq!(normalize_user_url("  "), None);
    }

    #[test]
    fn text_buffer_select_all_replaces_and_edits_cleanly() {
        let mut buf = TextBuffer::new("https://old.example");
        buf.focus_select_all();
        assert!(buf.is_selected_all());
        buf.insert_str("a");
        assert_eq!(buf.text, "a");
        assert!(!buf.is_selected_all());
        buf.insert_str("bc");
        assert_eq!(buf.text, "abc");
        buf.backspace();
        assert_eq!(buf.text, "ab");
        buf.focus_select_all();
        buf.backspace();
        assert_eq!(buf.text, "");
    }

    #[test]
    fn text_buffer_cursor_selection_word_ime_and_undo_work_cleanly() {
        let mut buf = TextBuffer::new("https://cef-rs-gpui.example/home");
        assert_eq!(buf.cursor_offset(), buf.text.len());
        let word_prev = buf.prev_word_boundary(buf.text.len());
        assert_eq!(&buf.text[word_prev..], "home");
        buf.delete_word_backward();
        assert_eq!(buf.text, "https://cef-rs-gpui.example/");
        assert!(buf.undo());
        assert_eq!(buf.text, "https://cef-rs-gpui.example/home");

        buf.move_to(buf.text.len());
        buf.select_to(word_prev);
        assert!(buf.selection_reversed);
        assert_eq!(buf.selected_text(), "home");
        buf.insert_str_filtered("index", false);
        assert_eq!(buf.text, "https://cef-rs-gpui.example/index");

        buf.select_word_at(9);
        assert_eq!(buf.selected_text(), "cef");
        buf.select_word_at(16);
        assert_eq!(buf.selected_text(), "gpui");

        let mut ime = TextBuffer::new("A🙂Z");
        assert_eq!(ime.text_len_utf16(), 4);
        ime.replace_and_mark_in_utf16_range(Some(1..3), "é🙂", Some(1..3), false);
        assert_eq!(ime.text, "Aé🙂Z");
        assert_eq!(ime.marked_range, Some(1..7));
        assert_eq!(ime.selected_range, 3..7);
        ime.replace_in_utf16_range(None, "漢", false);
        assert_eq!(ime.text, "A漢Z");
        assert_eq!(ime.marked_range, None);
        assert_eq!(ime.selected_range, 4..4);

        let mut multi = TextBuffer::new("a\nb");
        multi.move_to(2);
        assert_eq!(multi.line_start_boundary(3), 2);
        assert_eq!(multi.line_end_boundary(0), 1);
        multi.insert_str_filtered("x\ny", true);
        assert_eq!(multi.text, "a\nx\nyb");
        multi.focus_select_all();
        multi.collapse_to_end();
        assert!(multi.selected_range.is_empty());
        assert_eq!(multi.cursor_offset(), multi.text.len());
    }

    #[test]
    fn app_model_manages_tabs_and_quits_when_last_closed() {
        let mut model = ExampleAppModel::default();
        assert!(model.add_tab(1, DEFAULT_HOME_URL.into(), true));
        assert!(model.add_tab(2, "custom://cef-rs-gpui/SchemeTest.html".into(), false));
        assert_eq!(model.active_tab, Some(2));
        assert!(model.switch_tab(1));
        assert_eq!(model.active_tab().unwrap().show_sidebar, true);
        assert!(model.close_tab(1));
        assert_eq!(model.active_tab, Some(2));
        assert!(!model.quit_requested);
        assert!(model.close_tab(2));
        assert!(model.quit_requested);
    }

    #[test]
    fn standalone_cli_accepts_zero_args_and_explicit_overrides() {
        let cfg = ExampleConfig::parse_standalone_args(Vec::<OsString>::new())
            .unwrap()
            .unwrap();
        assert_eq!(cfg.mode, ThreadMode::Threaded);
        assert_eq!(cfg.backend, RenderPreference::D3d11Required);
        assert_eq!(cfg.host_mode, ExampleHostMode::Osr);
        assert!(!cfg.context_menu);
        assert!(cfg.smoke.is_none());

        let custom = ExampleConfig::parse_standalone_args([
            OsString::from("--mode"),
            OsString::from("Integrated"),
            OsString::from("--backend"),
            OsString::from("Software"),
            OsString::from("--host-mode"),
            OsString::from("native-hwnd"),
            OsString::from("--context-menu"),
            OsString::from("true"),
            OsString::from("--smoke-ms"),
            OsString::from("1500"),
            OsString::from("--verify-tests"),
            OsString::from("true"),
            OsString::from("--url"),
            OsString::from("custom://cef-rs-gpui/diagnostics.html"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(custom.mode, ThreadMode::Integrated);
        assert_eq!(custom.backend, RenderPreference::SoftwareRequired);
        assert_eq!(custom.host_mode, ExampleHostMode::NativeHwnd);
        assert!(custom.context_menu);
        assert_eq!(custom.smoke, Some(Duration::from_millis(1500)));
        assert!(custom.verify_tests);
        assert_eq!(
            custom.initial_url.as_deref(),
            Some("custom://cef-rs-gpui/diagnostics.html")
        );
    }

    #[test]
    fn draggable_region_hit_test_respects_no_drag_holes() {
        let regions = vec![
            DraggableRegionRect {
                x: 20,
                y: 20,
                width: 400,
                height: 200,
                draggable: true,
            },
            DraggableRegionRect {
                x: 60,
                y: 60,
                width: 120,
                height: 80,
                draggable: false,
            },
        ];
        assert!(is_point_draggable(&regions, 30, 30));
        assert!(!is_point_draggable(&regions, 80, 90));
        assert!(is_point_draggable(&regions, 300, 180));
        assert!(!is_point_draggable(&regions, 5, 5));
    }

    #[test]
    fn app_model_modal_queues_preserve_fifo_order_and_prune_stale_ids() {
        use rust_cef_gpui::contracts::RuntimeGeneration;
        use rust_cef_gpui::host_capabilities::PermissionKind;

        let rt = RuntimeGeneration::new(1);
        let b1 = BrowserId::new(rt, 1);
        let b2 = BrowserId::new(rt, 2);
        let mut model = ExampleAppModel::default();
        assert!(model.add_tab(10, DEFAULT_HOME_URL.into(), false));
        assert!(model.add_tab(20, DEFAULT_HOME_URL.into(), false));

        // Tab find open_flag synchronizes with active tab.
        assert!(!model.active_find_open.load(Ordering::Acquire));
        model.tab_mut(20).unwrap().find.set_open(true);
        model.sync_active_find_flag();
        assert!(model.active_find_open.load(Ordering::Acquire));
        model.switch_tab(10);
        assert!(!model.active_find_open.load(Ordering::Acquire));

        let p1 = HostRequestId {
            browser: b1,
            sequence: 1,
        };
        let p2 = HostRequestId {
            browser: b2,
            sequence: 2,
        };
        model.enqueue_permission(PendingPermissionModal {
            tab_id: 10,
            browser_id: b1,
            request: PermissionRequest {
                id: p1,
                kind: PermissionKind::MediaAccess,
                requesting_origin: "https://a.test".into(),
                requested_permissions: 1,
            },
        });
        model.enqueue_permission(PendingPermissionModal {
            tab_id: 20,
            browser_id: b2,
            request: PermissionRequest {
                id: p2,
                kind: PermissionKind::PermissionPrompt,
                requesting_origin: "https://b.test".into(),
                requested_permissions: 2,
            },
        });
        // Duplicate request id is ignored.
        model.enqueue_permission(PendingPermissionModal {
            tab_id: 10,
            browser_id: b1,
            request: PermissionRequest {
                id: p1,
                kind: PermissionKind::MediaAccess,
                requesting_origin: "https://a.test".into(),
                requested_permissions: 1,
            },
        });
        assert_eq!(model.pending_permission.len(), 2);
        assert!(model.cancel_permission(p1));
        assert!(!model.cancel_permission(p1));
        assert_eq!(model.pop_front_permission().unwrap().request.id, p2);

        // Auth modal redacts Debug output and wipes buffers on cancel/tab close.
        let a1 = HostRequestId {
            browser: b1,
            sequence: 3,
        };
        let auth_modal = PendingAuthModal {
            tab_id: 10,
            browser_id: b1,
            request: AuthRequest {
                id: a1,
                origin_url: "https://auth.test".into(),
                is_proxy: false,
                host: "auth.test".into(),
                port: 443,
                realm: "TestRealm".into(),
                scheme: "basic".into(),
            },
            username_input: TextBuffer::new("alice"),
            password_input: TextBuffer::new("s3cr3t!"),
        };
        let dbg = format!("{auth_modal:?}");
        assert!(!dbg.contains("alice") && !dbg.contains("s3cr3t!"));
        assert!(dbg.contains("<redacted>"));

        model.enqueue_auth(auth_modal);
        model.active_input = ActiveInput::AuthPass;
        model.clear_modals_for_tab(10, Some(b1));
        assert!(model.pending_auth.is_empty());
        assert_eq!(model.active_input, ActiveInput::None);

        let mut buf = TextBuffer::new("sensitive");
        buf.insert_str("-more");
        buf.wipe_secret();
        assert!(buf.text.is_empty());
        assert!(!buf.undo());
    }
}
