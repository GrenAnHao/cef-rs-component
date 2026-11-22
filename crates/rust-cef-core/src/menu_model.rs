//! UI-independent menu model for the approved top level plus one child level.
//!
//! Child rows cannot contain a submenu, so traversal and destruction need no
//! recursive walk or unbounded depth. The native copier must report deeper
//! structures rather than flattening or silently dropping them. Limits are
//! logical row/UTF-8 lengths, not allocation capacity or process RSS.
use std::{collections::BTreeSet, fmt};

use crate::menu::{ContextMenu, MenuEntry, MenuLimits};

/// New-model errors do not extend the legacy flat model's exhaustive enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuTreeError {
    Empty,
    TooManyEntries,
    EmptyLabel,
    LabelTooLong,
    LabelsTooLarge,
    InvalidCommandId,
    DuplicateCommandId,
    EmptySubmenu,
    EmptyVisibleSubmenu,
    NoVisibleCommands,
    InvalidRadioState,
}

#[derive(Clone, PartialEq, Eq)]
pub struct MenuCommand {
    pub command_id: i32,
    pub label: String,
    pub enabled: bool,
    pub visible: bool,
    /// None is not checkable; Some(false) is checkable but currently unchecked.
    pub checked: Option<bool>,
    /// Some identifies a radio item; checked must also be Some. The opaque CEF
    /// group ID is scoped to its containing menu, never global across submenus.
    pub radio_group: Option<i32>,
}

impl fmt::Debug for MenuCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MenuCommand")
            .field("command_id", &self.command_id)
            .field("label_bytes", &self.label.len())
            .field("enabled", &self.enabled)
            .field("visible", &self.visible)
            .field("checked", &self.checked)
            .field("radio_group", &self.radio_group)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuChild {
    Command(MenuCommand),
    Separator { visible: bool },
}

#[derive(Clone, PartialEq, Eq)]
pub struct MenuSubmenu {
    pub label: String,
    pub enabled: bool,
    pub visible: bool,
    pub entries: Vec<MenuChild>,
}

impl fmt::Debug for MenuSubmenu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MenuSubmenu")
            .field("label_bytes", &self.label.len())
            .field("enabled", &self.enabled)
            .field("visible", &self.visible)
            .field("entries", &self.entries)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuItem {
    Command(MenuCommand),
    Separator {
        visible: bool,
    },
    /// A group, never a selectable command ID.
    Submenu(MenuSubmenu),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MenuModel {
    pub entries: Vec<MenuItem>,
    /// Anchor in original CEF view DIP, not window or screen physical pixels.
    pub x: i32,
    pub y: i32,
}

struct Validation {
    limits: MenuLimits,
    ids: BTreeSet<i32>,
    label_bytes: usize,
}

impl Validation {
    fn label(&mut self, label: &str) -> Result<(), MenuTreeError> {
        if label.is_empty() {
            return Err(MenuTreeError::EmptyLabel);
        }
        if label.len() > self.limits.max_label_bytes.get() {
            return Err(MenuTreeError::LabelTooLong);
        }
        self.label_bytes = self
            .label_bytes
            .checked_add(label.len())
            .ok_or(MenuTreeError::LabelsTooLarge)?;
        if self.label_bytes > self.limits.max_total_label_bytes.get() {
            return Err(MenuTreeError::LabelsTooLarge);
        }
        Ok(())
    }

    fn command(&mut self, command: &MenuCommand) -> Result<(), MenuTreeError> {
        if command.command_id <= 0 {
            return Err(MenuTreeError::InvalidCommandId);
        }
        if !self.ids.insert(command.command_id) {
            return Err(MenuTreeError::DuplicateCommandId);
        }
        if command.radio_group.is_some() && command.checked.is_none() {
            return Err(MenuTreeError::InvalidRadioState);
        }
        self.label(&command.label)
    }
}

impl MenuModel {
    /// Counts all raw parent, child and separator rows before normalization.
    /// Duplicate leaf IDs across different groups are ambiguous and refused.
    pub fn validate(&self, limits: MenuLimits) -> Result<(), MenuTreeError> {
        let mut count = self.entries.len();
        if count > limits.max_entries.get() {
            return Err(MenuTreeError::TooManyEntries);
        }
        for item in &self.entries {
            if let MenuItem::Submenu(group) = item {
                count = count
                    .checked_add(group.entries.len())
                    .ok_or(MenuTreeError::TooManyEntries)?;
                if count > limits.max_entries.get() {
                    return Err(MenuTreeError::TooManyEntries);
                }
            }
        }
        let mut validation = Validation {
            limits,
            ids: BTreeSet::new(),
            label_bytes: 0,
        };
        let mut visible_command = false;
        for item in &self.entries {
            match item {
                MenuItem::Command(command) => {
                    validation.command(command)?;
                    visible_command |= command.visible;
                }
                MenuItem::Separator { .. } => {}
                MenuItem::Submenu(group) => {
                    validation.label(&group.label)?;
                    let mut has_command = false;
                    let mut visible_child = false;
                    for child in &group.entries {
                        if let MenuChild::Command(command) = child {
                            has_command = true;
                            visible_child |= command.visible;
                            validation.command(command)?;
                        }
                    }
                    if !has_command {
                        return Err(MenuTreeError::EmptySubmenu);
                    }
                    if group.visible && !visible_child {
                        return Err(MenuTreeError::EmptyVisibleSubmenu);
                    }
                    visible_command |= group.visible && visible_child;
                }
            }
        }
        if !visible_command && !validation.ids.is_empty() {
            return Err(MenuTreeError::NoVisibleCommands);
        }
        if validation.ids.is_empty() {
            return Err(MenuTreeError::Empty);
        }
        Ok(())
    }

    fn visit_commands(&self, mut visit: impl FnMut(&MenuCommand, bool)) {
        for item in &self.entries {
            match item {
                MenuItem::Command(command) => visit(command, command.visible && command.enabled),
                MenuItem::Separator { .. } => {}
                MenuItem::Submenu(group) => {
                    for child in &group.entries {
                        if let MenuChild::Command(command) = child {
                            visit(
                                command,
                                group.visible
                                    && group.enabled
                                    && command.visible
                                    && command.enabled,
                            );
                        }
                    }
                }
            }
        }
    }

    /// A disabled parent disables every child; check state never authorizes it.
    /// Even an unvalidated model must not authorize a duplicate matching ID.
    pub fn allows_selection(&self, command_id: i32) -> bool {
        if command_id <= 0 {
            return false;
        }
        let mut found = false;
        let mut allowed = false;
        let mut duplicate = false;
        self.visit_commands(|command, enabled| {
            if command.command_id == command_id {
                if found {
                    duplicate = true;
                }
                found = true;
                allowed = enabled;
            }
        });
        found && allowed && !duplicate
    }

    /// Only validated models may feed this private request-table metadata path.
    pub(crate) fn command_metadata(&self) -> Vec<(i32, bool)> {
        let mut commands = Vec::new();
        self.visit_commands(|command, enabled| commands.push((command.command_id, enabled)));
        commands
    }

    /// Cleans each level separately; never moves a child separator to the root.
    /// Admission must validate raw limits before calling this operation.
    pub fn normalize(&mut self) {
        for item in &mut self.entries {
            if let MenuItem::Submenu(group) = item {
                // A hidden parent makes its entire subtree effectively hidden.
                // Preserve that subtree verbatim, including child separators.
                if !group.visible {
                    continue;
                }
                normalize_rows(
                    &mut group.entries,
                    |row| matches!(row, MenuChild::Separator { .. }),
                    |row| match row {
                        MenuChild::Command(c) => c.visible,
                        MenuChild::Separator { visible } => *visible,
                    },
                );
            }
        }
        normalize_rows(
            &mut self.entries,
            |row| matches!(row, MenuItem::Separator { .. }),
            |row| match row {
                MenuItem::Command(c) => c.visible,
                MenuItem::Submenu(g) => g.visible,
                MenuItem::Separator { visible } => *visible,
            },
        );
    }
}

fn normalize_rows<T>(
    rows: &mut Vec<T>,
    separator: impl Fn(&T) -> bool,
    visible: impl Fn(&T) -> bool,
) {
    let mut cleaned = Vec::with_capacity(rows.len());
    let mut last_visible_separator = true;
    for row in rows.drain(..) {
        if visible(&row) {
            let is_separator = separator(&row);
            if is_separator && last_visible_separator {
                continue;
            }
            last_visible_separator = is_separator;
        }
        // Hidden rows keep their original data but do not interrupt visible
        // separator adjacency. Admission already charged them to raw limits.
        cleaned.push(row);
    }
    if let Some(index) = cleaned.iter().rposition(visible) {
        if separator(&cleaned[index]) {
            cleaned.remove(index);
        }
    }
    *rows = cleaned;
}

/// Explicit legacy conversion preserves the old flat model and PoC API. The
/// legacy bool cannot express checkable-but-unchecked; false remains ordinary.
impl From<ContextMenu> for MenuModel {
    fn from(menu: ContextMenu) -> Self {
        Self {
            entries: menu
                .entries
                .into_iter()
                .map(|entry| match entry {
                    MenuEntry::Separator => MenuItem::Separator { visible: true },
                    MenuEntry::Command {
                        command_id,
                        label,
                        enabled,
                        checked,
                    } => MenuItem::Command(MenuCommand {
                        command_id,
                        label,
                        enabled,
                        visible: true,
                        checked: checked.then_some(true),
                        radio_group: None,
                    }),
                })
                .collect(),
            x: menu.x,
            y: menu.y,
        }
    }
}
