//! WindowInfo and browser settings for a host-initiated managed DevTools
//! browser. Pure data: this module never calls Show, creates a native window,
//! activates anything or holds a Browser/Client reference.
use cef::{BrowserSettings, RuntimeStyle, WindowInfo};

/// Windowed Chrome-style DevTools popup owned by the original parent window.
/// The component Browser itself is windowless OSR, so the auxiliary surface
/// must be requested as a real popup with that parent as its owner.
pub(crate) fn window_info(parent: usize, title: &str) -> WindowInfo {
    let mut info = WindowInfo::default();
    info.windowless_rendering_enabled = 0;
    info.runtime_style = RuntimeStyle::CHROME;
    info.set_as_popup(cef::sys::HWND(parent as *mut cef::sys::HWND__), title)
}

/// Default settings with an explicit zero windowless frame rate: the auxiliary
/// browser is windowed and must not inherit OSR frame pacing.
pub(crate) fn settings() -> BrowserSettings {
    let mut settings = BrowserSettings::default();
    settings.windowless_frame_rate = 0;
    settings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devtools_window_is_windowed_chrome_popup() {
        let info = window_info(0, "GPUI CEF DevTools");
        assert_eq!(info.windowless_rendering_enabled, 0);
        assert!(matches!(info.runtime_style, RuntimeStyle::CHROME));
        assert_eq!(info.window_name.to_string(), "GPUI CEF DevTools");
        assert_eq!(settings().windowless_frame_rate, 0);
    }
}
