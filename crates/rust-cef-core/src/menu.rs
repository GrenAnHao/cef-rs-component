//! Legacy flat menu compatibility and shared menu identities/limits.
//!
//! The old PoC keeps this flat representation. New presenter delivery uses
//! [`crate::menu_model::MenuModel`] and [`crate::menu_context::MenuPresentation`]
//! for one submenu layer, checkability and owned context. Never silently flatten
//! new structures into this compatibility type or retain native model pointers.

use std::{
    collections::BTreeSet,
    num::NonZeroUsize,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use crate::contracts::BrowserId;

fn native_menu_epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn monotonic_ms_nonzero() -> u64 {
    let elapsed = native_menu_epoch().elapsed().as_millis();
    let ms = u64::try_from(elapsed).unwrap_or(u64::MAX - 1);
    ms.saturating_add(1)
}

#[derive(Debug, Default)]
struct NativeContextMenuState {
    open: AtomicBool,
    last_dismissed_ms: AtomicU64,
}

/// Lock-free native CEF context-menu lifecycle slot shared between CEF UI
/// callbacks (`run_context_menu` / `on_context_menu_dismissed`) and the GPUI view.
#[derive(Clone, Debug, Default)]
pub struct NativeContextMenuSlot {
    inner: Arc<NativeContextMenuState>,
}

impl NativeContextMenuSlot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mark_opened(&self) {
        self.inner.open.store(true, Ordering::Release);
    }

    pub fn mark_dismissed(&self) {
        // Store the dismissal timestamp before clearing `open` so concurrent UI
        // readers never see `open == false && last_dismissed_ms == 0`.
        self.inner
            .last_dismissed_ms
            .store(monotonic_ms_nonzero(), Ordering::Release);
        self.inner.open.store(false, Ordering::Release);
    }

    pub fn is_open(&self) -> bool {
        self.inner.open.load(Ordering::Acquire)
    }

    pub fn was_active_within(&self, window: Duration) -> bool {
        if self.is_open() {
            return true;
        }
        let dismissed_ms = self.inner.last_dismissed_ms.load(Ordering::Acquire);
        if dismissed_ms == 0 {
            return false;
        }
        let now_ms = monotonic_ms_nonzero();
        let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
        now_ms.saturating_sub(dismissed_ms) <= window_ms
    }

    pub fn clear(&self) {
        self.inner.open.store(false, Ordering::Release);
        self.inner.last_dismissed_ms.store(0, Ordering::Release);
    }
}

/// A menu opening, distinct from beforeunload and other continuation domains.
/// The full Browser identity includes the Runtime generation; sequence zero is
/// never allocated. One Runtime-owned menu request book allocates these IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextMenuRequestId {
    pub browser: BrowserId,
    pub sequence: u64,
}

/// A proposed terminal response, not proof that native CEF has applied it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextMenuResponse {
    Select { command_id: i32 },
    Cancel,
}

/// Explicit logical model limits chosen by the embedding composition root,
/// applied before separator normalization. These bound entry counts and label
/// UTF-8 lengths, not Vec/String capacities, allocator overhead, opaque payloads
/// or RSS. The native adapter must separately control copying and allocations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MenuLimits {
    pub max_entries: NonZeroUsize,
    pub max_label_bytes: NonZeroUsize,
    pub max_total_label_bytes: NonZeroUsize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuModelError {
    Empty,
    TooManyEntries,
    EmptyLabel,
    LabelTooLong,
    LabelsTooLarge,
    InvalidCommandId,
    DuplicateCommandId,
}

/// One entry of the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuEntry {
    /// A clickable item. `command_id` is what has to be sent back to CEF.
    Command {
        command_id: i32,
        label: String,
        enabled: bool,
        /// `true` for check/radio items that are currently on.
        checked: bool,
    },
    /// A non-clickable divider.
    Separator,
}

/// The menu as the UI should show it, anchored at a view-space point.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContextMenu {
    pub entries: Vec<MenuEntry>,
    /// Position in CEF view (DIP) coordinates, which is where the user clicked.
    pub x: i32,
    pub y: i32,
}

impl ContextMenu {
    /// Validates logical flat-model limits without changing it or silently dropping
    /// unsupported entries. A disabled command still counts as displayable.
    pub fn validate(&self, limits: MenuLimits) -> Result<(), MenuModelError> {
        if self.entries.len() > limits.max_entries.get() {
            return Err(MenuModelError::TooManyEntries);
        }
        let mut ids = BTreeSet::new();
        let mut total = 0usize;
        for entry in &self.entries {
            let MenuEntry::Command {
                command_id, label, ..
            } = entry
            else {
                continue;
            };
            if *command_id <= 0 {
                return Err(MenuModelError::InvalidCommandId);
            }
            if !ids.insert(*command_id) {
                return Err(MenuModelError::DuplicateCommandId);
            }
            if label.is_empty() {
                return Err(MenuModelError::EmptyLabel);
            }
            if label.len() > limits.max_label_bytes.get() {
                return Err(MenuModelError::LabelTooLong);
            }
            total = total
                .checked_add(label.len())
                .ok_or(MenuModelError::LabelsTooLarge)?;
            if total > limits.max_total_label_bytes.get() {
                return Err(MenuModelError::LabelsTooLarge);
            }
        }
        if ids.is_empty() {
            return Err(MenuModelError::Empty);
        }
        Ok(())
    }

    /// Only one enabled matching command may be selected. This is a model
    /// capability check, not a navigation/security policy or native execution.
    pub fn allows_selection(&self, command_id: i32) -> bool {
        if command_id <= 0 {
            return false;
        }
        let mut matching = self.entries.iter().filter_map(|entry| match entry {
            MenuEntry::Command {
                command_id: id,
                enabled,
                ..
            } if *id == command_id => Some(*enabled),
            _ => None,
        });
        matching.next() == Some(true) && matching.next().is_none()
    }

    /// Whether anything would actually be drawn.
    ///
    /// CEF is told about every right click, including ones on empty space where
    /// the model only has the standard editing items; an empty or all-separator
    /// menu is not worth a popup.
    pub fn is_empty(&self) -> bool {
        !self
            .entries
            .iter()
            .any(|entry| matches!(entry, MenuEntry::Command { .. }))
    }

    /// Label for `command_id`, or `None` when it is not in this menu.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn label_of(&self, command_id: i32) -> Option<&str> {
        self.entries.iter().find_map(|entry| match entry {
            MenuEntry::Command {
                command_id: id,
                label,
                ..
            } if *id == command_id => Some(label.as_str()),
            _ => None,
        })
    }

    /// One-line description used by tests and logs, e.g. `Back | Reload`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn describe(&self) -> String {
        self.entries
            .iter()
            .map(|entry| match entry {
                MenuEntry::Separator => "---".to_string(),
                MenuEntry::Command { label, enabled, .. } => {
                    if *enabled {
                        label.clone()
                    } else {
                        format!("({label})")
                    }
                }
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }

    /// Removes leading, trailing and duplicated separators.
    ///
    /// Chromium omits items per context (no "Paste" when the clipboard is
    /// empty), which leaves dividers next to each other; drawing those produces
    /// visibly broken gaps.
    pub fn normalize(&mut self) {
        let mut cleaned: Vec<MenuEntry> = Vec::with_capacity(self.entries.len());
        for entry in self.entries.drain(..) {
            if matches!(entry, MenuEntry::Separator) {
                match cleaned.last() {
                    None | Some(MenuEntry::Separator) => continue,
                    _ => {}
                }
            }
            cleaned.push(entry);
        }
        while matches!(cleaned.last(), Some(MenuEntry::Separator)) {
            cleaned.pop();
        }
        self.entries = cleaned;
    }
}

/// Chromium's item labels arrive with `&` before the accelerator character
/// (`&Copy`, `Cu&t`). Win32 menus render that as an underline; gpui has no
/// accelerator support, so the ampersand would show up literally.
pub fn strip_accelerator(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '&' {
            match chars.peek() {
                Some('&') => {
                    // `&&` is a literal ampersand.
                    chars.next();
                    out.push('&');
                }
                Some(_) => {
                    // Drop the marker, keep the character it marks.
                    continue;
                }
                None => out.push('&'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(id: i32, label: &str, enabled: bool) -> MenuEntry {
        MenuEntry::Command {
            command_id: id,
            label: label.to_string(),
            enabled,
            checked: false,
        }
    }

    #[test]
    fn separators_round_trip_is_idempotent() {
        let mut menu = ContextMenu {
            entries: vec![
                MenuEntry::Separator,
                command(1, "Back", true),
                MenuEntry::Separator,
                MenuEntry::Separator,
                command(2, "Reload", true),
                MenuEntry::Separator,
            ],
            x: 0,
            y: 0,
        };
        menu.normalize();
        assert_eq!(
            menu.entries,
            vec![
                command(1, "Back", true),
                MenuEntry::Separator,
                command(2, "Reload", true)
            ]
        );
        let once = menu.clone();
        menu.normalize();
        assert_eq!(menu, once, "normalize must be idempotent");
    }

    #[test]
    fn a_menu_without_commands_is_empty() {
        let mut menu = ContextMenu {
            entries: vec![MenuEntry::Separator],
            x: 0,
            y: 0,
        };
        menu.normalize();
        assert!(menu.is_empty());

        let only_disabled = ContextMenu {
            entries: vec![command(1, "Copy", false)],
            x: 0,
            y: 0,
        };
        // A disabled item is still worth showing, so it must not count as empty.
        assert!(!only_disabled.is_empty());
    }

    #[test]
    fn accelerators_are_stripped_not_shown() {
        assert_eq!(strip_accelerator("&Copy"), "Copy");
        assert_eq!(strip_accelerator("Cu&t"), "Cut");
        assert_eq!(strip_accelerator("&Back"), "Back");
        // A literal ampersand survives.
        assert_eq!(strip_accelerator("Tom && Jerry"), "Tom & Jerry");
        assert_eq!(strip_accelerator("Trailing&"), "Trailing&");
        assert_eq!(strip_accelerator(""), "");
        assert_eq!(strip_accelerator("&"), "&");
    }

    #[test]
    fn command_labels_are_lookupable() {
        let menu = ContextMenu {
            entries: vec![command(10, "Inspect", true), command(11, "Reload", false)],
            x: 5,
            y: 7,
        };
        assert_eq!(menu.label_of(10), Some("Inspect"));
        assert_eq!(menu.label_of(99), None);
    }

    #[test]
    fn native_context_menu_slot_tracks_open_dismiss_and_window() {
        let slot = NativeContextMenuSlot::new();
        assert!(!slot.is_open());
        assert!(!slot.was_active_within(Duration::from_millis(400)));

        slot.mark_opened();
        assert!(slot.is_open());
        assert!(slot.was_active_within(Duration::from_millis(0)));

        slot.mark_dismissed();
        assert!(!slot.is_open());
        assert!(slot.was_active_within(Duration::from_millis(400)));

        std::thread::sleep(Duration::from_millis(25));
        assert!(!slot.was_active_within(Duration::from_millis(5)));
        assert!(slot.was_active_within(Duration::from_millis(400)));

        slot.mark_opened();
        assert!(slot.is_open());
        slot.clear();
        assert!(!slot.is_open());
        assert!(!slot.was_active_within(Duration::from_millis(400)));
    }
}
