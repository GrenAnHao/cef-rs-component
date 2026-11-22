//! Non-default test-support only: bounded, read-only original callback evidence.
//! No text/word/suggestion contents, native handles, writes or retries.
use crate::executor::NativeAudit;
use cef::{
    Browser, CefString, ContextMenuParams, ImplBrowser, ImplBrowserHost, ImplContextMenuParams,
    ImplListValue, ImplMenuModel, ImplPreferenceManager, ImplValue, MenuModel, ValueType,
};
use rust_cef_core::menu_context::MenuContext;
use std::sync::Mutex;
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct SpellingContextSnapshot {
    pub opening: u64,
    pub cef_ui_thread: u64,
    pub point: Option<[i32; 2]>,
    pub editable: Option<bool>,
    pub spell_check_enabled: Option<bool>,
    pub original_model_count: Option<usize>,
    pub original_suggestion0_index: Option<i32>,
    pub original_dictionary_index: Option<i32>,
    pub preference_enabled: Option<bool>,
    pub dictionary_languages: Option<Vec<String>>,
    pub legacy_dictionary: Option<String>,
    pub preference_error: Option<String>,
    pub copied: bool,
    pub frame_id_bytes: usize,
    pub selection_bytes: usize,
    pub word_bytes: usize,
    pub suggestions: usize,
    pub selection_matches_word: bool,
}
pub(crate) struct Book(Mutex<Option<SpellingContextSnapshot>>);
impl Book {
    pub fn new() -> Self {
        Self(Mutex::new(None))
    }
    pub fn begin(&self, s: SpellingContextSnapshot) -> bool {
        let mut row = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if s.opening == 0 || row.as_ref().is_some_and(|v| v.opening >= s.opening) {
            return false;
        }
        *row = Some(s);
        true
    }
    pub fn copied(&self, opening: u64, c: &MenuContext) -> bool {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some(row) = guard.as_mut().filter(|v| v.opening == opening && !v.copied) else {
            return false;
        };
        row.copied = true;
        row.frame_id_bytes = c.frame_id.len();
        row.selection_bytes = c.selection.len();
        row.word_bytes = c.misspelled_word.len();
        row.suggestions = c.dictionary_suggestions.len();
        row.selection_matches_word = c.selection == c.misspelled_word;
        true
    }
    pub fn snapshot(&self) -> Option<SpellingContextSnapshot> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}
pub(crate) fn capture_before(
    opening: u64,
    browser: &Browser,
    params: Option<&ContextMenuParams>,
    model: Option<&MenuModel>,
    audit: &NativeAudit,
) -> SpellingContextSnapshot {
    let mut s = SpellingContextSnapshot {
        opening,
        cef_ui_thread: crate::executor::current_thread_id(),
        ..Default::default()
    };
    if let Some(p) = params {
        audit.record();
        s.editable = Some(p.is_editable() != 0);
        audit.record();
        s.spell_check_enabled = Some(p.is_spell_check_enabled() != 0);
        audit.record();
        let x = p.xcoord();
        audit.record();
        s.point = Some([x, p.ycoord()]);
    }
    if let Some(m) = model {
        audit.record();
        s.original_model_count = Some(m.count());
        audit.record();
        s.original_suggestion0_index = Some(m.index_of(200));
        audit.record();
        s.original_dictionary_index = Some(m.index_of(206));
    }
    let result = (|| -> Result<(), String> {
        audit.record();
        let host = browser.host().ok_or("probe original host absent")?;
        audit.record();
        let context = host
            .request_context()
            .ok_or("probe original context absent")?;
        audit.record();
        let enabled = context
            .preference(Some(&CefString::from("browser.enable_spellchecking")))
            .ok_or("enable preference absent")?;
        audit.record();
        if enabled.get_type() != ValueType::BOOL {
            return Err("enable preference type".into());
        }
        audit.record();
        s.preference_enabled = Some(enabled.bool() != 0);
        audit.record();
        let dictionaries = context
            .preference(Some(&CefString::from("spellcheck.dictionaries")))
            .ok_or("dictionary preference absent")?;
        audit.record();
        if dictionaries.get_type() != ValueType::LIST {
            return Err("dictionary preference type".into());
        }
        audit.record();
        let list = dictionaries.list().ok_or("dictionary list absent")?;
        audit.record();
        let count = list.size();
        if count > 8 {
            return Err("dictionary list budget".into());
        }
        let mut languages = Vec::with_capacity(count);
        for index in 0..count {
            audit.record();
            if list.get_type(index) != ValueType::STRING {
                return Err("dictionary item type".into());
            }
            audit.record();
            let raw = list.string(index);
            languages.push(
                crate::menu_model::read_userfree(&raw, 64)
                    .map_err(|_| "dictionary language UTF8 budget")?,
            );
        }
        s.dictionary_languages = Some(languages);
        audit.record();
        let legacy = context
            .preference(Some(&CefString::from("spellcheck.dictionary")))
            .ok_or("legacy preference absent")?;
        audit.record();
        if legacy.get_type() != ValueType::STRING {
            return Err("legacy preference type".into());
        }
        audit.record();
        let raw = legacy.string();
        s.legacy_dictionary = Some(
            crate::menu_model::read_userfree(&raw, 64)
                .map_err(|_| "legacy language UTF8 budget")?,
        );
        Ok(())
    })();
    if let Err(e) = result {
        s.preference_error = Some(e);
    }
    s
}
#[cfg(test)]
mod tests {
    use super::*;
    use rust_cef_core::{
        contracts::{BrowserId, RuntimeGeneration},
        ports::WindowBinding,
    };
    #[test]
    fn prepublication_probe_is_single_slot_and_never_attaches_old_capture_to_a_new_opening() {
        let b = Book::new();
        assert!(b.begin(SpellingContextSnapshot {
            opening: 1,
            editable: Some(true),
            ..Default::default()
        }));
        assert!(b.begin(SpellingContextSnapshot {
            opening: 2,
            editable: Some(false),
            ..Default::default()
        }));
        let c = MenuContext {
            binding: WindowBinding {
                browser: BrowserId::new(RuntimeGeneration::new(9), 1),
                window: 1,
            },
            binding_revision: 1,
            x: 20,
            y: 20,
            frame_id: "original".into(),
            selection: "private word".into(),
            misspelled_word: String::new(),
            dictionary_suggestions: vec![],
        };
        assert!(!b.copied(1, &c));
        assert!(b.copied(2, &c));
        assert!(!b.copied(2, &c));
        assert!(!b.begin(SpellingContextSnapshot {
            opening: 1,
            ..Default::default()
        }));
        let s = b.snapshot().unwrap();
        assert_eq!(s.opening, 2);
        assert!(s.copied);
        assert_eq!(s.selection_bytes, 12);
        assert_eq!(s.word_bytes, 0);
        assert_eq!(s.editable, Some(false));
        assert!(!format!("{s:?}").contains("private word"));
    }
}
