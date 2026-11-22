//! Owned menu context, independent of CEF pointers, HWNDs and UI toolkit types.
//!
//! Binding/frame values are captured identities, not proof of a live window,
//! native frame or navigation transaction. Adapters must requalify execution.
use std::{fmt, num::NonZeroUsize, sync::Arc};

use crate::{
    contracts::BrowserId, menu::ContextMenuRequestId, menu_model::MenuModel, ports::WindowBinding,
};

#[derive(Clone, PartialEq, Eq)]
pub struct MenuContext {
    pub binding: WindowBinding,
    /// Host binding revision, never a CEF/native navigation epoch.
    pub binding_revision: u64,
    /// Original view DIP anchor, checked against the delivered model at admission.
    pub x: i32,
    pub y: i32,
    /// Opaque owned CEF frame identity; do not fall back to a different frame.
    pub frame_id: String,
    pub selection: String,
    pub misspelled_word: String,
    pub dictionary_suggestions: Vec<String>,
}

impl fmt::Debug for MenuContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MenuContext")
            .field("binding", &self.binding)
            .field("binding_revision", &self.binding_revision)
            .field("x", &self.x)
            .field("y", &self.y)
            .field("frame_id_bytes", &self.frame_id.len())
            .field("selection_bytes", &self.selection.len())
            .field("misspelled_word_bytes", &self.misspelled_word.len())
            .field("suggestion_count", &self.dictionary_suggestions.len())
            .finish()
    }
}

/// Logical UTF-8 lengths and suggestion count, not allocator/RSS limits or
/// budgets for source/printing/DevTools results, which belong to separate work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MenuContextLimits {
    pub max_frame_id_bytes: NonZeroUsize,
    pub max_text_bytes: NonZeroUsize,
    pub max_suggestions: NonZeroUsize,
    pub max_total_bytes: NonZeroUsize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuContextError {
    WrongBrowser,
    InvalidBinding,
    AnchorMismatch,
    MissingFrame,
    FrameIdTooLong,
    TextTooLong,
    TooManySuggestions,
    TotalBytesExceeded,
}

impl MenuContext {
    pub fn validate(
        &self,
        browser: BrowserId,
        limits: MenuContextLimits,
    ) -> Result<(), MenuContextError> {
        if self.binding.browser != browser {
            return Err(MenuContextError::WrongBrowser);
        }
        if self.binding.window == 0 || self.binding_revision == 0 {
            return Err(MenuContextError::InvalidBinding);
        }
        if self.frame_id.is_empty() {
            return Err(MenuContextError::MissingFrame);
        }
        if self.frame_id.len() > limits.max_frame_id_bytes.get() {
            return Err(MenuContextError::FrameIdTooLong);
        }
        if self.dictionary_suggestions.len() > limits.max_suggestions.get() {
            return Err(MenuContextError::TooManySuggestions);
        }
        let mut total = self.frame_id.len();
        if total > limits.max_total_bytes.get() {
            return Err(MenuContextError::TotalBytesExceeded);
        }
        for text in std::iter::once(&self.selection)
            .chain(std::iter::once(&self.misspelled_word))
            .chain(self.dictionary_suggestions.iter())
        {
            if text.len() > limits.max_text_bytes.get() {
                return Err(MenuContextError::TextTooLong);
            }
            total = total
                .checked_add(text.len())
                .ok_or(MenuContextError::TotalBytesExceeded)?;
            if total > limits.max_total_bytes.get() {
                return Err(MenuContextError::TotalBytesExceeded);
            }
        }
        Ok(())
    }

    /// Equality of captured metadata only; the actual token must still be live.
    pub fn matches_binding(&self, binding: WindowBinding, revision: u64) -> bool {
        revision != 0 && self.binding == binding && self.binding_revision == revision
    }
}

/// The presenter and pending request share one immutable authoritative context.
/// An Arc::make_mut copy in the UI cannot alter the request book's context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuPresentation {
    pub request: ContextMenuRequestId,
    pub model: MenuModel,
    pub context: Arc<MenuContext>,
}
