//! Pure mapping tables shared by the Windows CEF glue.
//!
//! Kept free of `gpui` and `cef` types so the std-only test target
//! (`rustc --test src/lib.rs`) can exercise it.

/// CEF `cef_event_flags_t` bits. These are part of the frozen C ABI.
pub const EVENTFLAG_CAPS_LOCK_ON: u32 = 1 << 0;
pub const EVENTFLAG_SHIFT_DOWN: u32 = 1 << 1;
pub const EVENTFLAG_CONTROL_DOWN: u32 = 1 << 2;
pub const EVENTFLAG_ALT_DOWN: u32 = 1 << 3;
pub const EVENTFLAG_LEFT_MOUSE_BUTTON: u32 = 1 << 4;
pub const EVENTFLAG_MIDDLE_MOUSE_BUTTON: u32 = 1 << 5;
pub const EVENTFLAG_RIGHT_MOUSE_BUTTON: u32 = 1 << 6;
pub const EVENTFLAG_COMMAND_DOWN: u32 = 1 << 7;
pub const EVENTFLAG_IS_REPEAT: u32 = 1 << 13;
pub const EVENTFLAG_PRECISION_SCROLLING_DELTA: u32 = 1 << 14;
pub const EVENTFLAG_SCROLL_BY_PAGE: u32 = 1 << 15;

/// Keyboard modifier bits for a CEF mouse/key event.
pub fn modifier_flags(shift: bool, control: bool, alt: bool, platform: bool) -> u32 {
    let mut flags = 0;
    if shift {
        flags |= EVENTFLAG_SHIFT_DOWN;
    }
    if control {
        flags |= EVENTFLAG_CONTROL_DOWN;
    }
    if alt {
        flags |= EVENTFLAG_ALT_DOWN;
    }
    if platform {
        flags |= EVENTFLAG_COMMAND_DOWN;
    }
    flags
}

/// Maps a GPUI keystroke `key` to a Windows virtual-key code.
///
/// GPUI names printable keys by the character printed on the key
/// (`a`, `1`, `[`, `;`) and special keys by name (`enter`, `pageup`, `f5`).
pub fn windows_key_code(key: &str) -> Option<i32> {
    match key {
        "space" => return Some(0x20),
        "backspace" => return Some(0x08),
        "enter" => return Some(0x0D),
        "tab" => return Some(0x09),
        "up" => return Some(0x26),
        "down" => return Some(0x28),
        "left" => return Some(0x25),
        "right" => return Some(0x27),
        "home" => return Some(0x24),
        "end" => return Some(0x23),
        "pageup" => return Some(0x21),
        "pagedown" => return Some(0x22),
        "escape" => return Some(0x1B),
        "insert" => return Some(0x2D),
        "delete" => return Some(0x2E),
        "menu" => return Some(0x5D),
        "back" => return Some(0xA6),
        "forward" => return Some(0xA7),
        "shift" => return Some(0x10),
        "control" => return Some(0x11),
        "alt" => return Some(0x12),
        "capslock" => return Some(0x14),
        _ => {}
    }

    if let Some(rest) = key.strip_prefix('f')
        && let Ok(number) = rest.parse::<i32>()
        && (1..=24).contains(&number)
    {
        return Some(0x70 + number - 1);
    }

    let mut chars = key.chars();
    let (first, rest_empty) = (chars.next()?, chars.next().is_none());
    if !rest_empty {
        return None;
    }
    // VK codes for printable keys are the uppercase ASCII character.
    Some(match first {
        'a'..='z' => (first as i32) - ('a' as i32) + 0x41,
        'A'..='Z' => (first as i32) - ('A' as i32) + 0x41,
        '0'..='9' => (first as i32) - ('0' as i32) + 0x30,
        ';' => 0xBA,
        '=' => 0xBB,
        ',' => 0xBC,
        '-' => 0xBD,
        '.' => 0xBE,
        '/' => 0xBF,
        '`' => 0xC0,
        '[' => 0xDB,
        '\\' => 0xDC,
        ']' => 0xDD,
        '\'' => 0xDE,
        // Shifted variants reported when shift is folded into the key name.
        ':' => 0xBA,
        '+' => 0xBB,
        '<' => 0xBC,
        '_' => 0xBD,
        '>' => 0xBE,
        '?' => 0xBF,
        '~' => 0xC0,
        '{' => 0xDB,
        '|' => 0xDC,
        '}' => 0xDD,
        '"' => 0xDE,
        ' ' => 0x20,
        _ => return None,
    })
}

/// Translates a window-space pointer position into CEF view (DIP) coordinates.
///
/// Returns `None` for a zero-sized surface, and clamps nothing: CEF itself
/// ignores coordinates outside the view, but we keep the raw value so drags
/// that leave the surface still report sensible positions.
pub fn to_view_point(
    position: (f32, f32),
    origin: (f32, f32),
    size: (f32, f32),
) -> Option<(i32, i32)> {
    if size.0 <= 0.0 || size.1 <= 0.0 {
        return None;
    }
    Some((
        (position.0 - origin.0).round() as i32,
        (position.1 - origin.1).round() as i32,
    ))
}

/// Converts a GPUI scroll delta into CEF's Windows wheel units.
/// A line is one notch (WHEEL_DELTA = 120); pixel deltas are scaled so a
/// typical 3-line notch still lands near 120.
pub fn wheel_units(lines: (f32, f32)) -> (i32, i32) {
    (
        (lines.0 * 120.0).round() as i32,
        (lines.1 * 120.0).round() as i32,
    )
}

pub fn wheel_units_from_pixels(delta: (f32, f32)) -> (i32, i32) {
    (
        (delta.0 / 2.0).round() as i32,
        (delta.1 / 2.0).round() as i32,
    )
}

/// Returns the `character` field for a `KEYEVENT_CHAR` event, if this key
/// produces one on Windows without an IME.
pub fn char_for_key(key: &str) -> Option<u16> {
    match key {
        "enter" => Some(0x0D),
        "tab" => Some(0x09),
        _ => {
            let mut chars = key.chars();
            let ch = chars.next()?;
            if chars.next().is_some() || !ch.is_ascii_graphic() && ch != ' ' {
                return None;
            }
            let mut units = [0u16; 2];
            ch.encode_utf16(&mut units).first().copied()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_keys_map_to_virtual_codes() {
        assert_eq!(windows_key_code("enter"), Some(0x0D));
        assert_eq!(windows_key_code("space"), Some(0x20));
        assert_eq!(windows_key_code("pageup"), Some(0x21));
        assert_eq!(windows_key_code("escape"), Some(0x1B));
        assert_eq!(windows_key_code("f1"), Some(0x70));
        assert_eq!(windows_key_code("f24"), Some(0x87));
        assert_eq!(windows_key_code("f25"), None);
    }

    #[test]
    fn printable_keys_map_to_ascii_virtual_codes() {
        assert_eq!(windows_key_code("a"), Some(0x41));
        assert_eq!(windows_key_code("z"), Some(0x5A));
        assert_eq!(windows_key_code("0"), Some(0x30));
        assert_eq!(windows_key_code("["), Some(0xDB));
        assert_eq!(windows_key_code("'"), Some(0xDE));
        assert_eq!(windows_key_code("/"), Some(0xBF));
    }

    #[test]
    fn shifted_punctuation_reuses_the_base_virtual_code() {
        for (shifted, base) in [("~", "`"), ("{", "["), ("}", "]"), ("|", "\\"), ("\"", "'")] {
            assert_eq!(windows_key_code(shifted), windows_key_code(base));
        }
    }

    #[test]
    fn multi_character_and_unknown_keys_are_rejected() {
        assert_eq!(windows_key_code(""), None);
        assert_eq!(windows_key_code("ab"), None);
        assert_eq!(windows_key_code("不"), None);
        assert_eq!(windows_key_code("unknown-key"), None);
    }

    #[test]
    fn modifier_flags_combine() {
        assert_eq!(modifier_flags(false, false, false, false), 0);
        assert_eq!(
            modifier_flags(true, false, false, false),
            EVENTFLAG_SHIFT_DOWN
        );
        assert_eq!(
            modifier_flags(true, true, false, true),
            EVENTFLAG_SHIFT_DOWN | EVENTFLAG_CONTROL_DOWN | EVENTFLAG_COMMAND_DOWN
        );
    }

    #[test]
    fn pointer_positions_are_relative_to_the_surface() {
        assert_eq!(
            to_view_point((120.0, 90.0), (16.0, 48.0), (800.0, 600.0)),
            Some((104, 42))
        );
        assert_eq!(to_view_point((10.0, 10.0), (16.0, 48.0), (0.0, 0.0)), None);
        // Negative remains negative so a CEF drag can leave the view.
        assert_eq!(
            to_view_point((0.0, 0.0), (16.0, 48.0), (800.0, 600.0)),
            Some((-16, -48))
        );
    }

    #[test]
    fn wheel_deltas_use_windows_notch_units() {
        assert_eq!(wheel_units((0.0, 1.0)), (0, 120));
        assert_eq!(wheel_units((0.0, -3.0)), (0, -360));
        assert_eq!(wheel_units_from_pixels((0.0, 240.0)), (0, 120));
    }

    #[test]
    fn char_keys_cover_enter_tab_and_printables() {
        assert_eq!(char_for_key("enter"), Some(0x0D));
        assert_eq!(char_for_key("tab"), Some(0x09));
        assert_eq!(char_for_key("a"), Some(0x61));
        assert_eq!(char_for_key(" "), Some(0x20));
        assert_eq!(char_for_key("f5"), None);
        assert_eq!(char_for_key("up"), None);
    }
}
