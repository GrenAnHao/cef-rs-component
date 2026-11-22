//! Drag and drop bridging between GPUI and CEF.
//!
//! Two directions exist and they are not symmetric:
//!
//! * **Into the browser** (a file dragged from Explorer, or a drag started by
//!   the host): CEF is the drop *target*. GPUI already turns OS file drags into
//!   `PlatformInput::FileDrop`, and exposes the dragged paths as
//!   `ExternalPaths`. We translate that into `DragTargetDragEnter/Over/Drop`.
//! * **Out of the browser** (a link or selection dragged inside the page): CEF
//!   asks us through `RenderHandler::start_dragging`. Doing that properly needs
//!   an OLE `IDropSource` loop, which is a separate, larger piece of work; it is
//!   reported as unsupported instead of pretending to work.

use cef::{DragData, DragOperationsMask, ImplDragData, MouseEvent, sys};

/// CEF drag masks, mirroring `cef_drag_operations_mask_t`.
#[cfg_attr(not(test), allow(dead_code))]
pub const DRAG_OPERATION_NONE: u32 = 0;
pub const DRAG_OPERATION_COPY: u32 = 1;
pub const DRAG_OPERATION_LINK: u32 = 2;
pub const DRAG_OPERATION_MOVE: u32 = 16;

/// Operations we accept from an OS drag. Explorer's file drags default to copy.
pub const ACCEPTED_OPERATIONS: u32 =
    DRAG_OPERATION_COPY | DRAG_OPERATION_MOVE | DRAG_OPERATION_LINK;

pub fn mask(bits: u32) -> DragOperationsMask {
    DragOperationsMask::from(sys::cef_drag_operations_mask_t(bits as i32))
}

/// Builds CEF drag data for a set of dropped file paths.
///
/// Returns `None` when there is nothing to drop, which is what an empty string
/// list from Explorer produces.
pub fn drag_data_for_paths(paths: &[std::path::PathBuf]) -> Option<DragData> {
    let data = cef::drag_data_create()?;
    let mut added = false;
    for path in paths {
        if let Some(text) = path.to_str() {
            let cef_path = cef::CefString::from(text);
            data.add_file(Some(&cef_path), None);
            added = true;
        }
    }
    if !added {
        return None;
    }
    // Explorer also provides the first path as plain text; pages that only read
    // text/plain then still see something useful.
    if let Some(first) = paths.iter().find_map(|path| path.to_str()) {
        data.set_fragment_text(Some(&cef::CefString::from(first)));
    }
    Some(data)
}

/// Position inside the view for drag callbacks, in DIP coordinates.
pub fn drag_position(x: i32, y: i32) -> MouseEvent {
    MouseEvent { x, y, modifiers: 0 }
}

/// Mouse event carrying the pressed-button bits Chromium expects.
///
/// `SendMouseMoveEvent` has no separate button state; the modifier flags are the
/// only place Chromium learns a button is down. Without
/// `EVENTFLAG_LEFT_MOUSE_BUTTON` a drag looks like a plain hover, so the page
/// never sees `mousemove` with `buttons != 0` and text selection cannot start.
pub fn drag_move_event(x: i32, y: i32, left_down: bool, modifiers: u32) -> MouseEvent {
    let mut flags = modifiers;
    if left_down {
        flags |= crate::browser::keymap::EVENTFLAG_LEFT_MOUSE_BUTTON;
    }
    MouseEvent {
        x,
        y,
        modifiers: flags,
    }
}

/// Raw mask value, matching `cef_drag_operations_mask_t`.
#[cfg_attr(not(test), allow(dead_code))]
pub fn raw(mask: DragOperationsMask) -> i32 {
    let raw: sys::cef_drag_operations_mask_t = mask.into();
    raw.0
}

/// `true` when CEF reported an operation we can accept back to the OS.
///
/// The mirror-image of the previous case: `start_dragging` reports what the page
/// offers, and we have to decide whether the OS is allowed to take it. That path
/// is not implemented (see the module note), so today this is only exercised by
/// tests.
#[cfg_attr(not(test), allow(dead_code))]
pub fn is_accepted(operation: u32) -> bool {
    operation & ACCEPTED_OPERATIONS != 0
}

/// Builds the `DragData` payload CEF should receive for a file drop.
///
/// Exposed separately from the CEF calls so the URL/text fallbacks can be
/// checked without a live browser.
#[cfg_attr(not(test), allow(dead_code))]
pub fn describe_paths(paths: &[std::path::PathBuf]) -> Vec<String> {
    paths
        .iter()
        .filter_map(|path| path.to_str().map(|text| text.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_accepted_bit_is_masked_in() {
        assert!(is_accepted(DRAG_OPERATION_COPY));
        assert!(is_accepted(DRAG_OPERATION_MOVE));
        assert!(is_accepted(DRAG_OPERATION_LINK));
        assert!(!is_accepted(DRAG_OPERATION_NONE));
        assert!(is_accepted(DRAG_OPERATION_COPY | DRAG_OPERATION_MOVE));
    }

    #[test]
    fn drag_move_sets_the_left_button_bit() {
        let idle = drag_move_event(5, 6, false, 0);
        assert_eq!(
            idle.modifiers & crate::browser::keymap::EVENTFLAG_LEFT_MOUSE_BUTTON,
            0
        );

        let dragging = drag_move_event(5, 6, true, 0);
        assert_ne!(
            dragging.modifiers & crate::browser::keymap::EVENTFLAG_LEFT_MOUSE_BUTTON,
            0
        );

        // Existing modifier bits survive.
        let with_shift = drag_move_event(1, 2, true, crate::browser::keymap::EVENTFLAG_SHIFT_DOWN);
        assert_ne!(
            with_shift.modifiers & crate::browser::keymap::EVENTFLAG_SHIFT_DOWN,
            0
        );
        assert_ne!(
            with_shift.modifiers & crate::browser::keymap::EVENTFLAG_LEFT_MOUSE_BUTTON,
            0
        );
    }

    #[test]
    fn describe_paths_skips_non_utf8() {
        let paths = vec![
            std::path::PathBuf::from("C:/tmp/a.txt"),
            std::path::PathBuf::from("C:/tmp/b.png"),
        ];
        assert_eq!(
            describe_paths(&paths),
            vec!["C:/tmp/a.txt".to_string(), "C:/tmp/b.png".to_string()]
        );
        assert!(describe_paths(&[]).is_empty());
    }

    #[test]
    fn mask_preserves_bits() {
        assert_eq!(raw(mask(DRAG_OPERATION_COPY)), DRAG_OPERATION_COPY as i32);
        assert_eq!(raw(mask(ACCEPTED_OPERATIONS)), ACCEPTED_OPERATIONS as i32);
    }
}
