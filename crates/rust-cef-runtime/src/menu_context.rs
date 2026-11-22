//! Callback-local owned context capture. No native handle enters the result.
//! These helpers are not installed as a handler and do not admit/answer requests.
//! CEF getters/list_value allocate internally before Runtime UTF8 limits apply.
use crate::menu_model::{CopyError, copy_cef_menu_model, read_native_utf16, read_userfree};
use cef::{CefStringList, ImplBrowser, ImplContextMenuParams, ImplFrame};
use rust_cef_core::{
    contracts::BrowserId,
    menu::MenuLimits,
    menu_context::{MenuContext, MenuContextError, MenuContextLimits},
    menu_model::MenuModel,
    ports::WindowBinding,
};

/// Host-captured metadata, NOT proof of a live UI token or native navigation.
/// Caller must qualify the registry's full BrowserId/native Browser mapping.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CaptureScope {
    pub(crate) browser: BrowserId,
    pub(crate) binding: WindowBinding,
    pub(crate) revision: u64,
}
impl CaptureScope {
    fn validate(self) -> Result<(), ContextError> {
        if self.binding.browser != self.browser {
            return Err(ContextError::Model(MenuContextError::WrongBrowser));
        }
        if self.binding.window == 0 || self.revision == 0 {
            return Err(ContextError::Model(MenuContextError::InvalidBinding));
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Field {
    Frame,
    Selection,
    Word,
    Suggestion(usize),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContextError {
    WrongThread,
    InvalidFrame,
    ForeignNativeBrowser,
    ListUnavailable,
    InconsistentSuggestions,
    ListChanged,
    SuggestionFetchFailed(usize),
    Text { field: Field, error: CopyError },
    Model(MenuContextError),
    Menu(CopyError),
}
trait TextReader {
    fn point(&self) -> (i32, i32);
    fn text(&self, field: Field, limit: usize) -> Result<String, CopyError>;
}
trait Suggestions {
    fn count(&self) -> Result<usize, ContextError>;
    fn present(&self) -> bool;
    fn text(&self, index: usize, limit: usize) -> Result<String, ContextError>;
}
struct NativeText<'a> {
    frame: &'a cef::Frame,
    params: &'a cef::ContextMenuParams,
}
impl TextReader for NativeText<'_> {
    fn point(&self) -> (i32, i32) {
        (self.params.xcoord(), self.params.ycoord())
    }
    fn text(&self, field: Field, limit: usize) -> Result<String, CopyError> {
        let value = match field {
            Field::Frame => self.frame.identifier(),
            Field::Selection => self.params.selection_text(),
            Field::Word => self.params.misspelled_word(),
            Field::Suggestion(_) => return Err(CopyError::InvalidTextView),
        };
        read_userfree(&value, limit)
    }
}

/// Owns the CEF list exactly once. Never clone or use its eager IntoIterator.
struct NativeSuggestions {
    list: CefStringList,
    reported_present: bool,
}
impl NativeSuggestions {
    fn raw(&self) -> Result<*mut cef::sys::_cef_string_list_t, ContextError> {
        let raw: *const cef::sys::_cef_string_list_t = (&self.list).into();
        if raw.is_null() {
            Err(ContextError::ListUnavailable)
        } else {
            Ok(raw.cast_mut())
        }
    }
    fn capture(params: &cef::ContextMenuParams) -> Result<Self, ContextError> {
        let mut value = Self {
            list: CefStringList::new(),
            reported_present: false,
        };
        value.raw()?; // alloc failure must not be passed to the native getter
        value.reported_present = params.dictionary_suggestions(Some(&mut value.list)) != 0;
        Ok(value)
    }
}
/// Header lives on Rust's stack; CEF owns only its populated buffer. Clear the
/// buffer on every path. userfree_free here would wrongly free the stack header.
struct NativeValue(cef::sys::_cef_string_utf16_t);
impl NativeValue {
    fn empty() -> Self {
        Self(cef::sys::_cef_string_utf16_t {
            str_: std::ptr::null_mut(),
            length: 0,
            dtor: None,
        })
    }
}
impl Drop for NativeValue {
    fn drop(&mut self) {
        // SAFETY: initialized empty, then populated solely by CEF list_value.
        // Clear invokes the matching CEF buffer destructor, not header free.
        unsafe { cef::sys::cef_string_utf16_clear(&mut self.0) };
    }
}
impl Suggestions for NativeSuggestions {
    fn count(&self) -> Result<usize, ContextError> {
        // SAFETY: self owns the live non-null list; size does not mutate it.
        Ok(unsafe { cef::sys::cef_string_list_size(self.raw()?) })
    }
    fn present(&self) -> bool {
        self.reported_present
    }
    fn text(&self, index: usize, limit: usize) -> Result<String, ContextError> {
        // Avoid native DCHECK_LT as well as guarding stale/out-of-range indices.
        if index >= self.count()? {
            return Err(ContextError::SuggestionFetchFailed(index));
        }
        let mut value = NativeValue::empty();
        // SAFETY: owned non-null list, checked index, initialized writable header.
        let ok = unsafe { cef::sys::cef_string_list_value(self.raw()?, index, &mut value.0) };
        if ok == 0 {
            return Err(ContextError::SuggestionFetchFailed(index));
        }
        // SAFETY: value owns this CEF output until after the synchronous read;
        // it is not mutated, cloned, retained or sent elsewhere while borrowed.
        unsafe { read_native_utf16(&value.0, limit) }.map_err(|error| ContextError::Text {
            field: Field::Suggestion(index),
            error,
        })
    }
}
struct Budget {
    remaining: usize,
    limits: MenuContextLimits,
}
impl Budget {
    fn per_field(&self, field: Field) -> usize {
        if field == Field::Frame {
            self.limits.max_frame_id_bytes.get()
        } else {
            self.limits.max_text_bytes.get()
        }
    }
    fn ceiling(&self, field: Field) -> usize {
        self.per_field(field).min(self.remaining)
    }
    fn limit_error(&self, field: Field) -> ContextError {
        ContextError::Model(if self.remaining < self.per_field(field) {
            MenuContextError::TotalBytesExceeded
        } else if field == Field::Frame {
            MenuContextError::FrameIdTooLong
        } else {
            MenuContextError::TextTooLong
        })
    }
    fn accept(
        &mut self,
        field: Field,
        result: Result<String, ContextError>,
    ) -> Result<String, ContextError> {
        let value = match result {
            Err(ContextError::Text {
                error: CopyError::LabelBudget,
                ..
            }) => return Err(self.limit_error(field)),
            other => other?,
        };
        if value.len() > self.ceiling(field) {
            return Err(self.limit_error(field));
        }
        self.remaining = self
            .remaining
            .checked_sub(value.len())
            .ok_or(ContextError::Model(MenuContextError::TotalBytesExceeded))?;
        Ok(value)
    }
    fn field(&mut self, source: &impl TextReader, field: Field) -> Result<String, ContextError> {
        let result = source
            .text(field, self.ceiling(field))
            .map_err(|error| ContextError::Text { field, error });
        self.accept(field, result)
    }
    fn suggestions(&mut self, source: &impl Suggestions) -> Result<Vec<String>, ContextError> {
        let count = source.count()?;
        if count > self.limits.max_suggestions.get() {
            return Err(ContextError::Model(MenuContextError::TooManySuggestions));
        }
        // false means none (also returned by invalid params); it is NOT a general
        // error code. Callback lifetime must be established by the caller.
        if source.present() != (count != 0) {
            return Err(ContextError::InconsistentSuggestions);
        }
        let mut result = Vec::with_capacity(count);
        for index in 0..count {
            let field = Field::Suggestion(index);
            let value = source.text(index, self.ceiling(field));
            result.push(self.accept(field, value)?);
        }
        if source.count()? != count {
            return Err(ContextError::ListChanged);
        }
        Ok(result)
    }
}
fn capture<S: Suggestions>(
    scope: CaptureScope,
    limits: MenuContextLimits,
    source: &impl TextReader,
    make_list: impl FnOnce() -> Result<S, ContextError>,
) -> Result<MenuContext, ContextError> {
    scope.validate()?;
    let mut budget = Budget {
        remaining: limits.max_total_bytes.get(),
        limits,
    };
    let (x, y) = source.point();
    let frame_id = budget.field(source, Field::Frame)?;
    if frame_id.is_empty() {
        return Err(ContextError::Model(MenuContextError::MissingFrame));
    }
    let selection = budget.field(source, Field::Selection)?;
    let misspelled_word = budget.field(source, Field::Word)?;
    let dictionary_suggestions = budget.suggestions(&make_list()?)?;
    let context = MenuContext {
        binding: scope.binding,
        binding_revision: scope.revision,
        x,
        y,
        frame_id,
        selection,
        misspelled_word,
        dictionary_suggestions,
    };
    context
        .validate(scope.browser, limits)
        .map_err(ContextError::Model)?;
    Ok(context)
}

/// Original CEF callback only, outside all registry/request/GPUI locks/borrows.
/// Registry must first qualify full BrowserId/native Browser and live host token.
/// Captured metadata is not later execution permission or a navigation epoch.
pub(crate) fn copy_cef_context(
    scope: CaptureScope,
    native_browser: &cef::Browser,
    frame: &cef::Frame,
    params: &cef::ContextMenuParams,
    limits: MenuContextLimits,
) -> Result<MenuContext, ContextError> {
    scope.validate()?;
    if cef::currently_on(cef::ThreadId::UI) == 0 {
        return Err(ContextError::WrongThread);
    }
    if native_browser.is_valid() == 0 || frame.is_valid() == 0 {
        return Err(ContextError::InvalidFrame);
    }
    let mut owner = frame.browser().ok_or(ContextError::ForeignNativeBrowser)?;
    if owner.is_valid() == 0 || native_browser.is_same(Some(&mut owner)) == 0 {
        return Err(ContextError::ForeignNativeBrowser);
    }
    let context = capture(scope, limits, &NativeText { frame, params }, || {
        NativeSuggestions::capture(params)
    })?;
    if native_browser.is_valid() == 0 || frame.is_valid() == 0 {
        return Err(ContextError::InvalidFrame);
    }
    Ok(context)
}

/// Combined raw model/context capture uses the original DIP anchor once. No
/// native object/pointer or pending continuation enters this returned tuple.
pub(crate) fn copy_cef_request(
    scope: CaptureScope,
    native_browser: &cef::Browser,
    frame: &cef::Frame,
    params: &cef::ContextMenuParams,
    model: &cef::MenuModel,
    menu_limits: MenuLimits,
    context_limits: MenuContextLimits,
) -> Result<(MenuModel, MenuContext), ContextError> {
    let context = copy_cef_context(scope, native_browser, frame, params, context_limits)?;
    let owned = copy_cef_menu_model(model, context.x, context.y, menu_limits)
        .map_err(ContextError::Menu)?;
    if native_browser.is_valid() == 0 || frame.is_valid() == 0 {
        return Err(ContextError::InvalidFrame);
    }
    Ok((owned, context))
}

#[cfg(test)]
#[path = "menu_context/tests.rs"]
mod tests;
