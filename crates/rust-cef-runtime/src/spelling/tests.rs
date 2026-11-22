use super::*;
use rust_cef_core::{
    contracts::{BrowserId, RuntimeGeneration},
    menu_model::{MenuCommand, MenuItem, MenuModel},
    ports::WindowBinding,
};
fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}
fn context() -> MenuContext {
    MenuContext {
        binding: WindowBinding {
            browser: BrowserId::new(RuntimeGeneration::new(5), 1),
            window: 9,
        },
        binding_revision: 1,
        x: 20,
        y: 20,
        frame_id: "frame".into(),
        selection: "mispeling".into(),
        misspelled_word: "mispeling".into(),
        dictionary_suggestions: vec![
            "spelling".into(),
            "misspelling".into(),
            "misspelling2".into(),
            "misspelling3".into(),
            "misspelling4".into(),
        ],
    }
}
fn state() -> Spelling {
    let s = Spelling::new();
    s.configure(SpellingConfig {
        max_word_bytes: nz(16),
        allow_dictionary_write: false,
    })
    .unwrap();
    s.advance();
    s
}
fn model(ids: &[i32]) -> MenuModel {
    MenuModel {
        x: 20,
        y: 20,
        entries: ids
            .iter()
            .map(|id| {
                MenuItem::Command(MenuCommand {
                    command_id: *id,
                    label: "Owned action".into(),
                    enabled: true,
                    visible: true,
                    checked: None,
                    radio_group: None,
                })
            })
            .collect(),
    }
}
#[test]
fn original_bounded_suggestions_use_only_five_reference_indices_and_explicit_dictionary_write() {
    let s = state();
    let mut c = context();
    for index in 0..5 {
        assert!(
            matches!(s.plan(&c,200+index,s.opening()),Ok(Action::Replace(word)) if word==c.dictionary_suggestions[index as usize])
        );
    }
    assert!(matches!(
        s.plan(&c, 205, s.opening()),
        Err(SpellingFailure::WrongCommand)
    ));
    assert!(matches!(
        s.plan(&c, 206, s.opening()),
        Err(SpellingFailure::Unavailable)
    ));
    c.dictionary_suggestions.truncate(1);
    assert!(matches!(
        s.plan(&c, 204, s.opening()),
        Err(SpellingFailure::MissingSuggestion)
    ));
    c.selection = "another node".into();
    assert!(matches!(
        s.plan(&c, 200, s.opening()),
        Err(SpellingFailure::SelectionMismatch)
    ));
    c.selection = c.misspelled_word.clone();
    c.dictionary_suggestions[0] = "x".repeat(17);
    assert!(matches!(
        s.plan(&c, 200, s.opening()),
        Err(SpellingFailure::Budget)
    ));
    let writable = Spelling::new();
    writable
        .configure(SpellingConfig {
            max_word_bytes: nz(16),
            allow_dictionary_write: true,
        })
        .unwrap();
    writable.advance();
    assert!(
        matches!(writable.plan(&context(),206,writable.opening()),Ok(Action::Add(word)) if word=="mispeling")
    );
    let mut mismatch = context();
    mismatch.selection = "different".into();
    assert!(matches!(
        writable.plan(&mismatch, 206, writable.opening()),
        Err(SpellingFailure::SelectionMismatch)
    ));
    // Pure planning, no CEF/native dictionary invocation or completion claim.
    assert_eq!(s.status().native_issued, 0);
    assert!(!writable.status().effect_verified);
    assert!(!format!("{:?}", s.status()).contains("mispeling"));
}
#[test]
fn newer_native_opening_and_counter_overflow_never_authorize_an_old_spelling_context() {
    let s = state();
    let old = s.opening();
    assert!(s.plan(&context(), 200, old).is_ok());
    s.advance();
    assert!(matches!(
        s.plan(&context(), 200, old),
        Err(SpellingFailure::StaleOpening)
    ));
    assert!(s.plan(&context(), 200, s.opening()).is_ok());
    assert_eq!(
        s.configure(SpellingConfig {
            max_word_bytes: nz(16),
            allow_dictionary_write: true
        }),
        Err(EngineError::InvalidRequest)
    );
    s.opening.store(u64::MAX, Ordering::Release);
    s.advance();
    assert!(!s.current(u64::MAX));
    assert!(!s.configured(200));
    assert_eq!(s.opening(), u64::MAX);
    let counters = state();
    let mut max = u64::MAX;
    assert!(!counters.increment(&mut max));
    assert_eq!(max, u64::MAX);
    assert!(!counters.configured(200));
}
#[test]
fn whole_model_payload_and_capability_checks_do_not_hide_dictionary_or_other_required_actions() {
    let s = state();
    let c = context();
    let m = model(&[200]);
    assert_eq!(
        crate::menu_bridge::unsupported_capabilities(&m, false, false, false, false, |id| s
            .configured(id)),
        None
    );
    assert!(s.validate_model(&c, &m).is_ok());
    let m = model(&[200, 206]);
    assert_eq!(
        crate::menu_bridge::unsupported_capabilities(&m, false, false, false, false, |id| s
            .configured(id)),
        Some((206, true))
    );
    let m = model(&[200, 131]);
    assert_eq!(
        crate::menu_bridge::unsupported_capabilities(&m, true, true, false, false, |id| s
            .configured(id)),
        Some((131, true))
    );
    let mut short = c.clone();
    short.dictionary_suggestions.truncate(1);
    assert_eq!(
        s.validate_model(&short, &model(&[204])),
        Err(SpellingFailure::MissingSuggestion)
    );
}
