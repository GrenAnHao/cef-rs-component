//! Compatibility exports and embedded offline test pages used by the demo.
pub use rust_cef_core::menu as context_menu;
pub use rust_cef_core::{frame_store, keymap, resize_recovery};

/// Self-test plan and pass/fail rules, testable without a browser.
#[cfg_attr(target_os = "windows", allow(dead_code))]
#[path = "browser/plan.rs"]
pub mod plan;

/// Frame-format math for accelerated (shared texture) OSR frames.
#[cfg_attr(target_os = "windows", allow(dead_code))]
#[path = "browser/shared_texture.rs"]
pub mod shared_texture;

/// A self-contained page avoids requiring internet access for the first run.
pub fn demo_url() -> String {
    data_url(include_str!("../assets/demo.html"))
}

/// Editable page used by the menu probes. The offline demo page has no editable
/// element, so a right click there can never produce the edit menu - the menu
/// fixture has to be loaded explicitly.
pub fn menu_probe_url() -> String {
    data_url(
        "<body style='margin:0;background:rgb(26,76,126)'><div id='e' contenteditable='true' spellcheck='false' style='height:100px;width:280px;font:24px Arial'>menu text</div><script>let n=0;const e=document.getElementById('e');document.title='menu-default-ready';document.addEventListener('selectionchange',()=>{if(getSelection().toString()==='menu text'){n++;document.title='menu-default-selected:'+n}});new MutationObserver(()=>{if(e.textContent===''){document.title='menu-default-deleted'}}).observe(e,{childList:true,characterData:true,subtree:true})</script></body>",
    )
}

/// Page used by the interaction self-tests; it reports received input through
/// its title.
///
/// `tab` picks which section of the single-page probe opens first: the drag
/// probes drive fixed view coordinates, which only exist while their tab is
/// visible.
pub fn interaction_test_url() -> String {
    interaction_test_url_for("typing")
}

pub fn interaction_test_url_for(tab: &str) -> String {
    let mut url = data_url(include_str!("../assets/interaction-test.html"));
    if tab != "typing" {
        url.push_str("#");
        url.push_str(tab);
    }
    url
}

fn data_url(html: &str) -> String {
    let mut url = String::from("data:text/html;charset=utf-8,");
    use std::fmt::Write as _;
    for byte in html.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            url.push(byte as char);
        } else {
            write!(&mut url, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    url
}

#[cfg(test)]
mod tests {
    #[test]
    fn compatibility_exports_use_the_actual_core_types() {
        let store: rust_cef_core::frame_store::FrameStore =
            super::frame_store::FrameStore::default();
        assert!(store.publish_bgra(1, 1, &[0, 0, 255, 255]));
        assert_eq!(store.latest().unwrap().bgra.as_slice(), &[0, 0, 255, 255]);
        let format: rust_cef_core::frame_layout::FrameFormat =
            super::shared_texture::FrameFormat::Bgra8;
        assert!(format.matches_render_image());

        let mut menu: rust_cef_core::menu::ContextMenu = super::context_menu::ContextMenu {
            entries: vec![
                super::context_menu::MenuEntry::Separator,
                super::context_menu::MenuEntry::Command {
                    command_id: 1,
                    label: "Copy".to_string(),
                    enabled: true,
                    checked: false,
                },
            ],
            x: 0,
            y: 0,
        };
        menu.normalize();
        assert_eq!(menu.entries.len(), 1);

        let now = std::time::Instant::now();
        let mut recovery: rust_cef_core::resize_recovery::ResizeRecovery =
            super::resize_recovery::ResizeRecovery::default();
        recovery.resized(4, 4, 1.0, now);
        assert!(recovery.take_request(now));
        recovery.frame_received(4, 4);
        assert!(!recovery.take_request(now + std::time::Duration::from_secs(1)));
        assert_eq!(super::keymap::windows_key_code("a"), Some(0x41));
    }

    #[test]
    fn demo_is_embedded_and_percent_encoded() {
        let url = super::demo_url();
        assert!(url.starts_with("data:text/html;charset=utf-8,%3C"));
        assert!(!url.contains('<'));
        assert!(!url.contains('\n'));
        assert!(url.contains("GPUI"));
    }

    #[test]
    fn interaction_page_is_embedded() {
        let url = super::interaction_test_url();
        assert!(url.starts_with("data:text/html;charset=utf-8,%3C"));
        assert!(url.contains("mousedown"));
    }
}
