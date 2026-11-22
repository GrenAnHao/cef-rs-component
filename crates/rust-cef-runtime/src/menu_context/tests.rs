use super::*;
use crate::menu_model::copy_utf16;
use rust_cef_core::contracts::RuntimeGeneration;
use std::{cell::Cell, num::NonZeroUsize, rc::Rc};
fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}
fn limits() -> MenuContextLimits {
    MenuContextLimits {
        max_frame_id_bytes: nz(32),
        max_text_bytes: nz(32),
        max_suggestions: nz(5),
        max_total_bytes: nz(128),
    }
}
fn scope() -> CaptureScope {
    let browser = BrowserId::new(RuntimeGeneration::new(2), 1);
    CaptureScope {
        browser,
        binding: WindowBinding { browser, window: 9 },
        revision: 4,
    }
}
struct Text {
    fields: [Vec<u16>; 3],
    reads: Cell<usize>,
    points: Cell<usize>,
}
fn text() -> Text {
    Text {
        fields: ["frame-secret", "中🙂", "word"].map(|s| s.encode_utf16().collect()),
        reads: Cell::new(0),
        points: Cell::new(0),
    }
}
impl TextReader for Text {
    fn point(&self) -> (i32, i32) {
        self.points.set(self.points.get() + 1);
        (-3, 9)
    }
    fn text(&self, field: Field, limit: usize) -> Result<String, CopyError> {
        self.reads.set(self.reads.get() + 1);
        let index = match field {
            Field::Frame => 0,
            Field::Selection => 1,
            Field::Word => 2,
            _ => return Err(CopyError::InvalidTextView),
        };
        copy_utf16(&self.fields[index], limit)
    }
}
struct List {
    items: Vec<Vec<u16>>,
    reported: Option<usize>,
    present: bool,
    churn: bool,
    counts: Cell<usize>,
    reads: Rc<Cell<usize>>,
    fail: bool,
}
fn list(words: &[&str]) -> List {
    List {
        items: words.iter().map(|s| s.encode_utf16().collect()).collect(),
        reported: None,
        present: !words.is_empty(),
        churn: false,
        counts: Cell::new(0),
        reads: Rc::new(Cell::new(0)),
        fail: false,
    }
}
impl Suggestions for List {
    fn count(&self) -> Result<usize, ContextError> {
        let n = self.counts.get();
        self.counts.set(n + 1);
        Ok(self.reported.unwrap_or(self.items.len()) + usize::from(self.churn && n > 0))
    }
    fn present(&self) -> bool {
        self.present
    }
    fn text(&self, i: usize, limit: usize) -> Result<String, ContextError> {
        self.reads.set(self.reads.get() + 1);
        if self.fail {
            return Err(ContextError::SuggestionFetchFailed(i));
        }
        copy_utf16(&self.items[i], limit).map_err(|error| ContextError::Text {
            field: Field::Suggestion(i),
            error,
        })
    }
}
#[test]
fn context_capture_keeps_exact_owned_fields_anchor_and_redacted_debug() {
    let src = text();
    let mut l = limits();
    l.max_total_bytes = nz(29);
    let c = capture(scope(), l, &src, || Ok(list(&["中", "xyz"]))).unwrap();
    assert_eq!(
        (c.x, c.y, c.binding, c.binding_revision),
        (-3, 9, scope().binding, 4)
    );
    assert_eq!(
        (&*c.frame_id, &*c.selection, &*c.misspelled_word),
        ("frame-secret", "中🙂", "word")
    );
    assert_eq!(c.dictionary_suggestions, vec!["中", "xyz"]);
    assert_eq!(src.reads.get(), 3);
    assert_eq!(src.points.get(), 1);
    let debug = format!("{c:?}");
    for secret in ["frame-secret", "中🙂", "word\"", "xyz"] {
        assert!(!debug.contains(secret));
    }
    let mut short = l;
    short.max_total_bytes = nz(28);
    assert_eq!(
        capture(scope(), short, &text(), || Ok(list(&["中", "xyz"]))),
        Err(ContextError::Model(MenuContextError::TotalBytesExceeded))
    );
    let mut empty = text();
    empty.fields[1].clear();
    empty.fields[2].clear();
    let mut exact = limits();
    exact.max_total_bytes = nz(12);
    assert!(
        capture(scope(), exact, &empty, || Ok(list(&[]))).is_ok(),
        "optional empty fields remain valid with zero bytes left"
    );
}
#[test]
fn context_capture_refuses_binding_and_text_budgets_before_list_materialization() {
    let src = text();
    let factories = Cell::new(0);
    let mut wrong = scope();
    wrong.binding.window = 0;
    assert_eq!(
        capture(wrong, limits(), &src, || {
            factories.set(1);
            Ok(list(&[]))
        }),
        Err(ContextError::Model(MenuContextError::InvalidBinding))
    );
    assert_eq!(src.reads.get(), 0);
    assert_eq!(src.points.get(), 0);
    assert_eq!(factories.get(), 0);
    wrong = scope();
    wrong.binding.browser = BrowserId::new(RuntimeGeneration::new(3), 1);
    assert_eq!(
        capture(wrong, limits(), &src, || Ok(list(&[]))),
        Err(ContextError::Model(MenuContextError::WrongBrowser))
    );
    for (field, error) in [
        (0, MenuContextError::FrameIdTooLong),
        (1, MenuContextError::TextTooLong),
        (2, MenuContextError::TextTooLong),
    ] {
        let mut src = text();
        src.fields[field] = vec![65; 33];
        assert_eq!(
            capture(scope(), limits(), &src, || {
                factories.set(1);
                Ok(list(&[]))
            }),
            Err(ContextError::Model(error))
        );
        assert_eq!(factories.get(), 0);
    }
    let mut src = text();
    src.fields[0].clear();
    assert_eq!(
        capture(scope(), limits(), &src, || Ok(list(&[]))),
        Err(ContextError::Model(MenuContextError::MissingFrame))
    );
    assert_eq!(src.reads.get(), 1);
    let mut bad = text();
    bad.fields[1] = vec![0xD800];
    assert_eq!(
        capture(scope(), limits(), &bad, || Ok(list(&[]))),
        Err(ContextError::Text {
            field: Field::Selection,
            error: CopyError::InvalidUtf16
        })
    );
    let mut small = limits();
    small.max_total_bytes = nz(11);
    assert_eq!(
        capture(scope(), small, &text(), || {
            factories.set(1);
            Ok(list(&[]))
        }),
        Err(ContextError::Model(MenuContextError::TotalBytesExceeded))
    );
    assert_eq!(factories.get(), 0);
}
#[test]
fn suggestion_capture_bounds_count_and_reports_presence_churn_and_item_failure() {
    let mut huge = list(&[]);
    huge.reported = Some(usize::MAX);
    let reads = huge.reads.clone();
    let mut b = Budget {
        remaining: 128,
        limits: limits(),
    };
    assert_eq!(
        b.suggestions(&huge),
        Err(ContextError::Model(MenuContextError::TooManySuggestions))
    );
    assert_eq!(reads.get(), 0);
    for (words, present) in [(vec![], true), (vec!["A"], false)] {
        let mut l = list(&words);
        l.present = present;
        assert_eq!(
            b.suggestions(&l),
            Err(ContextError::InconsistentSuggestions)
        );
        assert_eq!(l.reads.get(), 0);
    }
    let mut changed = list(&["A"]);
    changed.churn = true;
    assert_eq!(b.suggestions(&changed), Err(ContextError::ListChanged));
    let mut failed = list(&["A"]);
    failed.fail = true;
    assert_eq!(
        b.suggestions(&failed),
        Err(ContextError::SuggestionFetchFailed(0))
    );
    let l = list(&["中"]);
    let mut exact = limits();
    exact.max_text_bytes = nz(3);
    let mut b = Budget {
        remaining: 3,
        limits: exact,
    };
    assert_eq!(b.suggestions(&l).unwrap(), vec!["中"]);
    let mut b = Budget {
        remaining: 2,
        limits: exact,
    };
    assert_eq!(
        b.suggestions(&l),
        Err(ContextError::Model(MenuContextError::TotalBytesExceeded))
    );
}
#[test]
fn cef_string_list_indexed_copy_uses_real_fixed_ffi_and_clears_each_stack_value() {
    // Pinned string_list_impl.cc and string_types_impl.cc use local vectors/new/
    // delete; no CEF context/thread/pump access. This is NOT a browser menu test.
    let list = CefStringList::new();
    let source = NativeSuggestions {
        list,
        reported_present: true,
    };
    let raw = source.raw().unwrap();
    for word in ["中🙂", "A"] {
        let mut units: Vec<u16> = word.encode_utf16().collect();
        let value = cef::sys::_cef_string_utf16_t {
            str_: units.as_mut_ptr(),
            length: units.len(),
            dtor: None,
        };
        // SAFETY: owned non-null list and live initialized UTF16 view. CEF append
        // makes its own copy before this view/Vec is released; no borrowed list clone.
        unsafe { cef::sys::cef_string_list_append(raw, &value) };
    }
    assert_eq!(source.count().unwrap(), 2);
    let mut l = limits();
    l.max_text_bytes = nz(8);
    l.max_total_bytes = nz(8);
    let mut b = Budget {
        remaining: 8,
        limits: l,
    };
    let owned = b.suggestions(&source).unwrap();
    assert_eq!(owned, vec!["中🙂", "A"]);
    let mut short = Budget {
        remaining: 7,
        limits: l,
    };
    assert_eq!(
        short.suggestions(&source),
        Err(ContextError::Model(MenuContextError::TotalBytesExceeded))
    );
    assert_eq!(
        source.text(0, 6),
        Err(ContextError::Text {
            field: Field::Suggestion(0),
            error: CopyError::LabelBudget
        })
    );
    assert_eq!(
        source.text(2, 8),
        Err(ContextError::SuggestionFetchFailed(2)),
        "out of range must not enter native DCHECK"
    );
    assert_eq!(source.text(0, 8).unwrap(), "中🙂");
    assert_eq!(source.count().unwrap(), 2);
    drop(source);
    assert_eq!(
        owned,
        vec!["中🙂", "A"],
        "owned UTF8 survives native list destruction"
    );
    println!("round35_native_string_ffi=passed; cef_initialize=false; browser_menu_getters=false");
}
