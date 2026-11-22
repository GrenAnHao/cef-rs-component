//! Scripted input used by `POC_SELF_TEST=1`.
//!
//! Each step is a real `BrowserHost` call in view (DIP) coordinates, so the test
//! exercises the same path a human would: CEF ->
//! renderer -> page. The page reports what it saw through its title.

use cef::{BrowserHost, CefString, ImplBrowserHost, KeyEvent, KeyEventType, MouseButtonType};

use super::keymap;

pub use super::plan::{CLICK_X, CLICK_Y, Step, composition_steps, steps};

pub fn apply(step: Step, host: &BrowserHost) {
    let mouse = cef::MouseEvent {
        x: CLICK_X,
        y: CLICK_Y,
        modifiers: 0,
    };
    match step {
        Step::PointerMove => host.send_mouse_move_event(Some(&mouse), 0),
        Step::MouseDown => host.send_mouse_click_event(Some(&mouse), MouseButtonType::LEFT, 0, 1),
        Step::MouseUp => host.send_mouse_click_event(Some(&mouse), MouseButtonType::LEFT, 1, 1),
        Step::WheelDown => host.send_mouse_wheel_event(Some(&mouse), 0, -120),
        Step::Key(key) => {
            let key = key.to_string();
            let mut down = KeyEvent::default();
            down.type_ = KeyEventType::RAWKEYDOWN;
            down.windows_key_code = keymap::windows_key_code(&key).unwrap_or(0);
            host.send_key_event(Some(&down));

            let mut ch = KeyEvent::default();
            ch.type_ = KeyEventType::CHAR;
            let unit = keymap::char_for_key(&key).unwrap_or(0);
            ch.character = unit;
            ch.unmodified_character = unit;
            ch.windows_key_code = unit as i32;
            host.send_key_event(Some(&ch));

            let mut up = KeyEvent::default();
            up.type_ = KeyEventType::KEYUP;
            up.windows_key_code = down.windows_key_code;
            host.send_key_event(Some(&up));
        }
        Step::StartComposition => {
            let composing = CefString::from("ni");
            let range = cef::Range { from: 0, to: 2 };
            let mut underline = cef::CompositionUnderline::default();
            underline.range = range.clone();
            underline.color = 0xff00_0000;
            underline.thick = 1;
            let underlines = [underline];
            // cefclient passes CefRange::InvalidRange() (-1,-1) here. The Rust
            // binding exposes u32 fields, so -1 becomes u32::MAX bit for bit.
            let invalid = cef::Range {
                from: u32::MAX,
                to: u32::MAX,
            };
            host.ime_set_composition(
                Some(&composing),
                Some(&underlines),
                Some(&invalid),
                Some(&range),
            );
        }
        Step::CommitComposition => {
            // cefclient calls ImeCommitText with an invalid replacement range;
            // the u32 fields carry the -1 bit pattern.
            let committed = CefString::from("你");
            let invalid = cef::Range {
                from: u32::MAX,
                to: u32::MAX,
            };
            host.ime_commit_text(Some(&committed), Some(&invalid), 0);
        }
    }
}

/// The same interaction, but expressed as GPUI platform input so it can be
/// dispatched through the view's listeners. This checks the GPUI -> CEF
/// translation (coordinate math, key naming, wheel units), not just CEF itself.
pub fn gpui_steps() -> Vec<gpui::PlatformInput> {
    use gpui::{
        KeyDownEvent, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
        MouseUpEvent, PlatformInput, ScrollDelta, ScrollEvent, TouchPhase, point, px,
    };
    let at = point(px(CLICK_X as f32 + 30.0), px(CLICK_Y as f32 + 20.0));
    let modifiers = Modifiers::default();
    vec![
        PlatformInput::MouseMove(MouseMoveEvent {
            position: at,
            pressed_button: None,
            modifiers,
        }),
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: at,
            modifiers,
            click_count: 1,
            first_mouse: false,
        }),
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: at,
            modifiers,
            click_count: 1,
        }),
        PlatformInput::Scroll(ScrollEvent {
            position: at,
            delta: ScrollDelta::Lines(point(0.0, -1.0)),
            modifiers,
            touch_phase: TouchPhase::Moved,
            source: gpui::ScrollSource::Pointer,
        }),
        PlatformInput::KeyDown(KeyDownEvent {
            keystroke: Keystroke {
                modifiers,
                key: "a".to_string(),
                key_char: Some("a".to_string()),
            },
            is_held: false,
            prefer_character_input: false,
        }),
        PlatformInput::KeyUp(gpui::KeyUpEvent {
            keystroke: Keystroke {
                modifiers,
                key: "a".to_string(),
                key_char: Some("a".to_string()),
            },
        }),
    ]
}

pub use super::plan::{
    evaluate, evaluate_composition, evaluate_drag, evaluate_drop, evaluate_html5_drag,
    evaluate_os_input,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpui_steps_cover_every_listener_path() {
        use gpui::PlatformInput;
        let steps = gpui_steps();
        assert_eq!(steps.len(), 6);
        assert!(matches!(steps[0], PlatformInput::MouseMove(_)));
        assert!(matches!(steps[1], PlatformInput::MouseDown(_)));
        assert!(matches!(steps[2], PlatformInput::MouseUp(_)));
        assert!(matches!(steps[3], PlatformInput::Scroll(_)));
        assert!(matches!(steps[4], PlatformInput::KeyDown(_)));
        assert!(matches!(steps[5], PlatformInput::KeyUp(_)));
    }
}
