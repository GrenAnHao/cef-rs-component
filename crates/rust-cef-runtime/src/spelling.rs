//! Fixed-reference spell commands. Native APIs operate on current WebContents
//! selection / BrowserContext custom dictionary and return void. Issued is NOT
//! effect/flush completion. No query/word/suggestion contents in diagnostics.
use crate::{menu_bridge::Scope, registry::UiRegistry};
use cef::{Browser, CefString, Frame, ImplBrowser, ImplBrowserHost};
use rust_cef_core::{menu::ContextMenuRequestId, menu_context::MenuContext, ports::EngineError};
use std::{
    num::NonZeroUsize,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpellingConfig {
    pub max_word_bytes: NonZeroUsize,
    /// Explicit authorization for native custom-dictionary writes. Memory/OTR is
    /// NOT a proof of private or nonpersistent dictionary scope. No OS-dictionary
    /// isolation claim. Validation never enables this without separate scope proof.
    pub allow_dictionary_write: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpellingFailure {
    Unavailable,
    StaleOpening,
    MissingWord,
    MissingSuggestion,
    SelectionMismatch,
    Budget,
    NativeUnavailable,
    WrongCommand,
    CounterExhausted,
}
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "test-support", derive(serde::Serialize))]
pub struct SpellingStatus {
    pub native_issued: u64,
    pub rejected: u64,
    pub dictionary_issued: u64,
    pub replacement_issued: u64,
    pub last_command: Option<i32>,
    pub last_menu_sequence: Option<u64>,
    pub last_word_bytes: usize,
    pub last_replacement_bytes: usize,
    /// CEF supplies no success/result/flush callback for these void APIs.
    pub effect_verified: bool,
    /// browser.enable_spellchecking + spellcheck.dictionaries were accepted by CEF.
    pub preferences_enabled: bool,
    /// Which step of the preference application ran or failed.
    pub preferences_detail: String,
    pub first_failure: Option<String>,
}
pub(crate) struct Spelling {
    config: OnceLock<SpellingConfig>,
    opening: AtomicU64,
    faulted: AtomicBool,
    status: Mutex<SpellingStatus>,
}
pub(crate) enum Action<'a> {
    Replace(&'a str),
    Add(&'a str),
}
impl Spelling {
    pub fn new() -> Self {
        Self {
            config: OnceLock::new(),
            opening: AtomicU64::new(0),
            faulted: AtomicBool::new(false),
            status: Mutex::new(SpellingStatus::default()),
        }
    }
    pub fn configure(&self, c: SpellingConfig) -> Result<(), EngineError> {
        self.config.set(c).map_err(|_| EngineError::InvalidRequest)
    }
    pub fn is_configured(&self) -> bool {
        self.config.get().is_some()
    }
    pub fn configured(&self, id: i32) -> bool {
        !self.faulted.load(Ordering::Acquire)
            && self
                .config
                .get()
                .is_some_and(|c| id != 206 || c.allow_dictionary_write)
    }
    pub fn opening(&self) -> u64 {
        self.opening.load(Ordering::Acquire)
    }
    pub fn advance(&self) {
        let current = self.opening();
        match current.checked_add(1) {
            Some(next) => {
                if self
                    .opening
                    .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    self.faulted.store(true, Ordering::Release);
                }
            }
            None => {
                self.faulted.store(true, Ordering::Release);
            }
        }
    }
    pub fn current(&self, opening: u64) -> bool {
        opening != 0 && opening == self.opening() && !self.faulted.load(Ordering::Acquire)
    }
    pub fn plan<'a>(
        &self,
        context: &'a MenuContext,
        id: i32,
        opening: u64,
    ) -> Result<Action<'a>, SpellingFailure> {
        let config = self.config.get().ok_or(SpellingFailure::Unavailable)?;
        if !self.current(opening) {
            return Err(SpellingFailure::StaleOpening);
        }
        if context.misspelled_word.is_empty() {
            return Err(SpellingFailure::MissingWord);
        }
        if context.misspelled_word.len() > config.max_word_bytes.get() {
            return Err(SpellingFailure::Budget);
        }
        if context.selection != context.misspelled_word {
            return Err(SpellingFailure::SelectionMismatch);
        }
        match id {
            200..=204 => {
                let word = context
                    .dictionary_suggestions
                    .get((id - 200) as usize)
                    .filter(|s| !s.is_empty())
                    .ok_or(SpellingFailure::MissingSuggestion)?;
                if word.len() > config.max_word_bytes.get() {
                    return Err(SpellingFailure::Budget);
                }
                Ok(Action::Replace(word))
            }
            206 if config.allow_dictionary_write => Ok(Action::Add(&context.misspelled_word)),
            206 => Err(SpellingFailure::Unavailable),
            _ => Err(SpellingFailure::WrongCommand),
        }
    }
    pub fn validate_model(
        &self,
        context: &MenuContext,
        model: &rust_cef_core::menu_model::MenuModel,
    ) -> Result<(), SpellingFailure> {
        use rust_cef_core::menu_model::{MenuChild, MenuItem};
        let check = |id| {
            if matches!(id, 200..=204 | 206) {
                self.plan(context, id, self.opening()).map(|_| ())
            } else {
                Ok(())
            }
        };
        for item in &model.entries {
            match item {
                MenuItem::Command(c) => check(c.command_id)?,
                MenuItem::Submenu(g) => {
                    for item in &g.entries {
                        if let MenuChild::Command(c) = item {
                            check(c.command_id)?;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
    /// Records whether CEF accepted the spellchecking preferences. This is a
    /// preference admission, not proof that the renderer produced suggestions.
    pub fn note_preferences(&self, enabled: bool, detail: &str) {
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        s.preferences_enabled = enabled;
        s.preferences_detail = detail.to_string();
    }
    pub fn status(&self) -> SpellingStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    fn increment(&self, counter: &mut u64) -> bool {
        if let Some(next) = counter.checked_add(1) {
            *counter = next;
            true
        } else {
            self.faulted.store(true, Ordering::Release);
            false
        }
    }
    fn reject(&self, menu: ContextMenuRequestId, id: i32, e: SpellingFailure) {
        let mut s = self.status.lock().unwrap_or_else(|p| p.into_inner());
        if !self.increment(&mut s.rejected) {
            s.first_failure
                .get_or_insert(format!("{:?}", SpellingFailure::CounterExhausted));
        }
        s.last_menu_sequence = Some(menu.sequence);
        s.last_command = Some(id);
        s.first_failure.get_or_insert(format!("{e:?}"));
    }
    fn issued(
        &self,
        menu: ContextMenuRequestId,
        id: i32,
        context: &MenuContext,
        replacement: usize,
    ) {
        let mut s = self.status.lock().unwrap_or_else(|p| p.into_inner());
        let total_ok = self.increment(&mut s.native_issued);
        let action_ok = if id == 206 {
            self.increment(&mut s.dictionary_issued)
        } else {
            self.increment(&mut s.replacement_issued)
        };
        if !total_ok || !action_ok {
            s.first_failure
                .get_or_insert(format!("{:?}", SpellingFailure::CounterExhausted));
        }
        s.last_command = Some(id);
        s.last_menu_sequence = Some(menu.sequence);
        s.last_word_bytes = context.misspelled_word.len();
        s.last_replacement_bytes = replacement;
    }
}
impl UiRegistry {
    pub(crate) fn perform_spelling(
        &self,
        scope: &Scope,
        menu: ContextMenuRequestId,
        browser: &Browser,
        frame: &Frame,
        id: i32,
    ) {
        let Some(binding) = scope.registration.upgrade() else {
            scope.fail("spelling registration gone");
            return;
        };
        let result = (|| {
            if !scope.current() || !self.menu_original_frame_valid(scope, browser, frame) {
                return Err(SpellingFailure::StaleOpening);
            }
            let action = binding
                .spelling
                .plan(&scope.context, id, scope.spelling_opening)?;
            self.menu_audit().record();
            let host = browser.host().ok_or(SpellingFailure::NativeUnavailable)?;
            // No native object or DOM transaction is encoded in the scalar opening.
            // Recheck known cutpoints, then invoke only on this original CEF UI call.
            if !scope.current()
                || !binding.spelling.current(scope.spelling_opening)
                || !self.menu_original_frame_valid(scope, browser, frame)
            {
                return Err(SpellingFailure::StaleOpening);
            }
            let (word, replacement) = match action {
                Action::Replace(word) => (word, word.len()),
                Action::Add(word) => (word, 0),
            };
            let text = CefString::from(word);
            self.menu_audit().record();
            if id == 206 {
                host.add_word_to_dictionary(Some(&text));
            } else {
                host.replace_misspelling(Some(&text));
            }
            binding
                .spelling
                .issued(menu, id, &scope.context, replacement);
            scope.note_action();
            Ok(())
        })();
        if let Err(e) = result {
            binding.spelling.reject(menu, id, e);
            scope.fail(format!("spelling operation rejected: {e:?}"));
        }
    }
}
#[cfg(test)]
mod tests;
