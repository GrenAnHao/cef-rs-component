//! Pure description of the interaction self-test.
//!
//! No `gpui` or `cef` types, so the std-only test target can check the plan and
//! the pass/fail rules without building a browser.

/// Where the scripted pointer clicks, in CEF view (DIP) coordinates.
pub const CLICK_X: i32 = 120;
pub const CLICK_Y: i32 = 90;

/// One step of the direct-to-CEF phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    PointerMove,
    MouseDown,
    MouseUp,
    WheelDown,
    Key(char),
    StartComposition,
    CommitComposition,
}

/// Pointer and keyboard events only.
///
/// IME composition is deliberately absent: on a machine whose active keyboard
/// layout is an IME (this one has only `中文(简体) - 美式键盘`), the system IME
/// owns the composition state, and synthetic `ImeSetComposition`/`ImeCommitText`
/// calls race with it — the page ends up reporting real pinyin conversions such
/// as `compositionupdate:擦` instead of the scripted text. That made this check
/// fail for reasons unrelated to the code. Composition now has its own mode.
pub fn steps() -> Vec<Step> {
    vec![
        Step::PointerMove,
        Step::MouseDown,
        Step::MouseUp,
        Step::WheelDown,
        Step::Key('a'),
        Step::Key('b'),
    ]
}

/// Composition steps, driven only by `POC_SELF_TEST=6`. Requires an environment
/// with no IME stealing the composition state; the real IME path is best
/// verified by hand.
pub fn composition_steps() -> Vec<Step> {
    vec![
        Step::PointerMove,
        Step::MouseDown,
        Step::MouseUp,
        Step::StartComposition,
        Step::CommitComposition,
    ]
}

/// Markers the pointer/keyboard run must report.
pub fn expected_title_markers() -> Vec<&'static str> {
    vec![
        "mousedown",
        "mouseup",
        // Chromium converts the -120 wheel units into roughly +100px of scroll,
        // so only the presence of the event is asserted.
        "wheel:",
        "keydown:a",
        "input:a",
        "keydown:b",
        "input:ab",
    ]
}

/// Markers the composition run must report.
pub fn expected_composition_markers() -> Vec<&'static str> {
    vec!["compositionstart", "compositionend"]
}

/// Markers a drag run has to produce: a pointer move with a held button is what
/// text selection and in-page dragging produce inside the page.
pub fn expected_drag_markers() -> Vec<&'static str> {
    vec!["mousedown", "drag:", "mouseup"]
}

/// Markers a file drop has to produce, written by the page's `drop` listener.
pub fn expected_drop_markers() -> Vec<&'static str> {
    vec!["drop:"]
}

/// Markers an in-page HTML5 drag has to produce: `dragstart` comes from the
/// draggable element, `htmldrop` from the zone that accepted it.
pub fn expected_html5_drag_markers() -> Vec<&'static str> {
    vec!["dragstart", "htmldrop:"]
}

/// Markers an OS-level input run has to produce. The typed text is chosen by
/// the injector, so only the event classes are checked.
pub fn expected_os_input_markers() -> Vec<&'static str> {
    vec!["mousedown", "mouseup", "wheel:", "input:"]
}

fn missing(title: &str, markers: Vec<&'static str>) -> Vec<&'static str> {
    markers
        .into_iter()
        .filter(|marker| !title.contains(marker))
        .collect()
}

/// Reports which expected markers did not show up in the page title.
pub fn evaluate(title: &str) -> Vec<&'static str> {
    missing(title, expected_title_markers())
}

pub fn evaluate_composition(title: &str) -> Vec<&'static str> {
    missing(title, expected_composition_markers())
}

pub fn evaluate_os_input(title: &str) -> Vec<&'static str> {
    missing(title, expected_os_input_markers())
}

pub fn evaluate_drag(title: &str) -> Vec<&'static str> {
    missing(title, expected_drag_markers())
}

pub fn evaluate_drop(title: &str) -> Vec<&'static str> {
    missing(title, expected_drop_markers())
}

pub fn evaluate_html5_drag(title: &str) -> Vec<&'static str> {
    missing(title, expected_html5_drag_markers())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_and_key_steps_avoid_composition() {
        let pointer = steps();
        assert_eq!(pointer.len(), 6);
        assert!(pointer.contains(&Step::WheelDown));
        // Composition is a separate run: an active system IME otherwise owns the
        // composition state and these synthetic calls race with it.
        assert!(!pointer.contains(&Step::StartComposition));
        assert!(!pointer.contains(&Step::CommitComposition));
    }

    #[test]
    fn composition_steps_are_self_contained() {
        let composition = composition_steps();
        assert!(composition.contains(&Step::StartComposition));
        assert!(composition.contains(&Step::CommitComposition));
        assert!(!composition.contains(&Step::Key('a')));
    }

    #[test]
    fn evaluation_lists_only_missing_markers() {
        let full = expected_title_markers().join("|");
        assert!(evaluate(&full).is_empty());

        let partial = "mousedown:1,2|mouseup:1,2";
        let missing = evaluate(partial);
        assert!(missing.contains(&"wheel:"));
        assert!(missing.contains(&"input:ab"));
        assert!(!missing.contains(&"mousedown"));

        // Composition is judged by its own rule set.
        assert!(evaluate_composition("compositionstart").contains(&"compositionend"));
        assert!(evaluate_composition("compositionstart|compositionend").is_empty());
    }

    #[test]
    fn drag_and_drop_rules_are_independent() {
        let dragged = "mousedown:1,2|drag:5,6|mouseup:5,6";
        assert!(evaluate_drag(dragged).is_empty());
        assert_eq!(evaluate_drop(dragged), vec!["drop:"]);

        let dropped = "drop:a.txt,b.png";
        assert!(evaluate_drop(dropped).is_empty());
        assert_eq!(
            evaluate_drag(dropped),
            vec!["mousedown", "drag:", "mouseup"]
        );
    }

    #[test]
    fn os_input_evaluation_ignores_specific_typed_text() {
        let title = "mousedown:1,2|mouseup:1,2|wheel:300|input:xy";
        assert!(evaluate_os_input(title).is_empty());
        assert!(evaluate_os_input("mousedown:1,2").contains(&"wheel:"));
    }
}
