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
pub const EVENTFLAG_NUM_LOCK_ON: u32 = 1 << 8;
pub const EVENTFLAG_IS_KEY_PAD: u32 = 1 << 9;
pub const EVENTFLAG_IS_LEFT: u32 = 1 << 10;
pub const EVENTFLAG_IS_RIGHT: u32 = 1 << 11;
pub const EVENTFLAG_IS_REPEAT: u32 = 1 << 13;
pub const EVENTFLAG_PRECISION_SCROLLING_DELTA: u32 = 1 << 14;
pub const EVENTFLAG_SCROLL_BY_PAGE: u32 = 1 << 15;

/// Derives CEF key-location modifier flags (`IS_KEY_PAD`, `IS_LEFT`, `IS_RIGHT`)
/// from a Windows virtual-key code and `WM_KEYDOWN`/`WM_KEYUP` `lParam`, matching
/// `CefBrowserHostWrapper::GetCefKeyboardModifiers` (`CefBrowserHostWrapper.cpp:717-780`).
pub fn windows_key_location_flags(vk: i32, lparam: i32) -> u32 {
    let extended = ((lparam as u32 >> 16) & 0x0100) != 0;
    let scancode = (lparam as u32 >> 16) & 0x00FF;
    match vk {
        // VK_RETURN: extended means Numpad Enter.
        0x0D if extended => EVENTFLAG_IS_KEY_PAD,
        // Navigation/editing keys without KF_EXTENDED come from the numpad.
        0x21..=0x28 | 0x2D | 0x2E if !extended => EVENTFLAG_IS_KEY_PAD,
        // VK_CLEAR, VK_NUMPAD0..=VK_NUMPAD9, VK_MULTIPLY..=VK_DIVIDE, VK_NUMLOCK.
        0x0C | 0x60..=0x6F | 0x90 => EVENTFLAG_IS_KEY_PAD,
        // VK_CONTROL (0x11) and VK_MENU (0x12): KF_EXTENDED distinguishes right vs left.
        0x11 | 0x12 => {
            if extended {
                EVENTFLAG_IS_RIGHT
            } else {
                EVENTFLAG_IS_LEFT
            }
        }
        // VK_SHIFT (0x10): standard AT scancode 0x36 is Right Shift, 0x2A is Left Shift.
        0x10 => match scancode {
            0x36 => EVENTFLAG_IS_RIGHT,
            0x2A => EVENTFLAG_IS_LEFT,
            _ => 0,
        },
        // VK_LWIN (0x5B) / VK_RWIN (0x5C), plus explicit L/R modifier VKs.
        0x5B | 0xA0 | 0xA2 | 0xA4 => EVENTFLAG_IS_LEFT,
        0x5C | 0xA1 | 0xA3 | 0xA5 => EVENTFLAG_IS_RIGHT,
        _ => 0,
    }
}

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
        ')' => 0x30,
        '!' => 0x31,
        '@' => 0x32,
        '#' => 0x33,
        '$' => 0x34,
        '%' => 0x35,
        '^' => 0x36,
        '&' => 0x37,
        '*' => 0x38,
        '(' => 0x39,
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

pub const CHROMIUM_PIXELS_PER_LINE: f32 = 100.0 / 3.0;
pub const DEFAULT_WHEEL_SCROLL_LINES: f32 = 3.0;

/// Accumulates sub-pixel scroll deltas across high-resolution wheel and touchpad
/// events without truncating slow movements to zero.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScrollWheelAccumulator {
    rem_x: f64,
    rem_y: f64,
}

impl ScrollWheelAccumulator {
    pub fn reset(&mut self) {
        self.rem_x = 0.0;
        self.rem_y = 0.0;
    }

    pub fn accumulate_lines(&mut self, lines: (f32, f32)) -> (i32, i32) {
        let clamp_lines = |v: f32| {
            if v.is_finite() {
                f64::from(v.clamp(-120.0, 120.0))
            } else {
                0.0
            }
        };
        let px_x = (clamp_lines(lines.0) * 100.0) / 3.0;
        let px_y = (clamp_lines(lines.1) * 100.0) / 3.0;
        (
            accumulate_axis(&mut self.rem_x, px_x),
            accumulate_axis(&mut self.rem_y, px_y),
        )
    }

    pub fn accumulate_pixels(&mut self, pixels: (f32, f32)) -> (i32, i32) {
        let step_x = accumulate_axis(&mut self.rem_x, f64::from(pixels.0));
        let step_y = accumulate_axis(&mut self.rem_y, f64::from(pixels.1));
        (step_x, step_y)
    }
}

fn accumulate_axis(rem: &mut f64, delta: f64) -> i32 {
    if !delta.is_finite() || delta == 0.0 {
        return 0;
    }
    if (*rem > 0.0 && delta < 0.0) || (*rem < 0.0 && delta > 0.0) {
        *rem = 0.0;
    }
    let total = *rem + delta;
    if !total.is_finite() {
        *rem = 0.0;
        return 0;
    }
    let step = total.trunc() as i32;
    *rem = total - f64::from(step);
    step
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
        for (shifted, base) in [
            (")", "0"),
            ("!", "1"),
            ("@", "2"),
            ("#", "3"),
            ("$", "4"),
            ("%", "5"),
            ("^", "6"),
            ("&", "7"),
            ("*", "8"),
            ("(", "9"),
            ("~", "`"),
            ("{", "["),
            ("}", "]"),
            ("|", "\\"),
            ("\"", "'"),
        ] {
            assert_eq!(
                windows_key_code(shifted),
                windows_key_code(base),
                "{shifted}"
            );
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
    fn scroll_wheel_accumulator_matches_chromium_lines_and_preserves_subpixels() {
        let mut acc = ScrollWheelAccumulator::default();
        // Standard 3-line Windows notch = 100 CSS px (Edge/Chrome).
        assert_eq!(acc.accumulate_lines((0.0, 3.0)), (0, 100));
        // 1-line steps sum to exact 100 across 3 steps.
        let s1 = acc.accumulate_lines((0.0, 1.0)).1;
        let s2 = acc.accumulate_lines((0.0, 1.0)).1;
        let s3 = acc.accumulate_lines((0.0, 1.0)).1;
        assert_eq!((s1, s2, s3), (33, 33, 34));
        assert_eq!(s1 + s2 + s3, 100);

        // High-res 1/8 notch (0.375 lines = 12.5 px) alternates 12 and 13 cleanly.
        acc.reset();
        assert_eq!(acc.accumulate_lines((0.0, 0.375)), (0, 12));
        assert_eq!(acc.accumulate_lines((0.0, 0.375)), (0, 13));

        // Sub-pixel touchpad movements accumulate without dropping slow motion.
        acc.reset();
        assert_eq!(acc.accumulate_pixels((0.0, 0.4)), (0, 0));
        assert_eq!(acc.accumulate_pixels((0.0, 0.4)), (0, 0));
        assert_eq!(acc.accumulate_pixels((0.0, 0.4)), (0, 1));
        // Direction reversal resets stale remainder cleanly.
        assert_eq!(acc.accumulate_pixels((0.0, -0.6)), (0, 0));
        assert_eq!(acc.accumulate_pixels((0.0, -0.6)), (0, -1));

        // Non-finite and extreme (e.g. unguarded WHEEL_PAGESCROLL) line counts never overflow.
        acc.reset();
        assert_eq!(acc.accumulate_lines((f32::NAN, f32::INFINITY)), (0, 0));
        assert_eq!(acc.accumulate_lines((0.0, 1.0e9)), (0, 4000));
        assert_eq!(acc.accumulate_lines((0.0, -1.0e9)), (0, -4000));
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

    #[test]
    fn windows_key_location_flags_match_cefsharp_conventions() {
        let extended = 0x0100_0000i32;
        assert_eq!(windows_key_location_flags(0x0D, 0x001C_0001), 0);
        assert_eq!(
            windows_key_location_flags(0x0D, extended | 0x001C_0001),
            EVENTFLAG_IS_KEY_PAD
        );
        assert_eq!(
            windows_key_location_flags(0x26, 0x0048_0001),
            EVENTFLAG_IS_KEY_PAD
        );
        assert_eq!(windows_key_location_flags(0x26, extended | 0x0048_0001), 0);
        assert_eq!(
            windows_key_location_flags(0x65, 0x004C_0001),
            EVENTFLAG_IS_KEY_PAD
        );
        assert_eq!(
            windows_key_location_flags(0x11, 0x001D_0001),
            EVENTFLAG_IS_LEFT
        );
        assert_eq!(
            windows_key_location_flags(0x11, extended | 0x001D_0001),
            EVENTFLAG_IS_RIGHT
        );
        assert_eq!(
            windows_key_location_flags(0x10, 0x002A_0001),
            EVENTFLAG_IS_LEFT
        );
        assert_eq!(
            windows_key_location_flags(0x10, 0x0036_0001),
            EVENTFLAG_IS_RIGHT
        );
        assert_eq!(windows_key_location_flags(0x5B, 0), EVENTFLAG_IS_LEFT);
        assert_eq!(windows_key_location_flags(0x5C, 0), EVENTFLAG_IS_RIGHT);
    }
}
