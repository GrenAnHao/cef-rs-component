//! Pure-std bounded AXTree model, incremental applier, and coalescing slot.
//!
//! Structural reference (`D:\System Dir\Downloads\CefSharp`, rev `41919703`):
//! - `CefSharp.Wpf/Experimental/Accessibility/AccessibilityHandler.cs:17-69`
//! - `CefSharp.Wpf/Experimental/Accessibility/AccessibilityTree.cs:12-82`
//! - `CefSharp.Wpf/Experimental/Accessibility/AccessibilityNode.cs:17-71, 160-258`

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};

use crate::contracts::truncate_utf8_bytes;

pub const MAX_AX_NODES: usize = 2048;
pub const MAX_AX_TEXT_BYTES: usize = 1024;
pub const MAX_AX_CHILDREN_PER_NODE: usize = 256;
pub const MAX_AX_ANCESTOR_HOPS: usize = 64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum AxRole {
    RootWebArea,
    WebArea,
    Document,
    Heading,
    Link,
    Button,
    TextField,
    StaticText,
    CheckBox,
    RadioButton,
    ComboBox,
    List,
    ListItem,
    Image,
    Paragraph,
    Region,
    Navigation,
    Main,
    Banner,
    ContentInfo,
    Form,
    Table,
    Row,
    Cell,
    Dialog,
    Alert,
    Tab,
    TabList,
    TabPanel,
    Menu,
    MenuItem,
    GenericContainer,
    #[default]
    Unknown,
}

impl AxRole {
    pub fn from_cef_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "rootwebarea" | "root_web_area" => Self::RootWebArea,
            "webarea" | "web_area" => Self::WebArea,
            "document" => Self::Document,
            "heading" => Self::Heading,
            "link" => Self::Link,
            "button" | "togglebutton" | "popupbutton" => Self::Button,
            "textfield" | "text_field" | "searchbox" | "spinbutton" => Self::TextField,
            "statictext" | "static_text" | "inlinetextbox" => Self::StaticText,
            "checkbox" | "check_box" | "switch" => Self::CheckBox,
            "radiobutton" | "radio_button" => Self::RadioButton,
            "combobox" | "comboboxgrouping" | "comboboxmenubutton" | "textfieldwithcombobox" => {
                Self::ComboBox
            }
            "list" => Self::List,
            "listitem" | "list_item" => Self::ListItem,
            "image" | "imagemap" | "svgroot" => Self::Image,
            "paragraph" => Self::Paragraph,
            "region" | "group" => Self::Region,
            "navigation" => Self::Navigation,
            "main" => Self::Main,
            "banner" => Self::Banner,
            "contentinfo" => Self::ContentInfo,
            "form" => Self::Form,
            "table" | "grid" | "treegrid" => Self::Table,
            "row" => Self::Row,
            "cell" | "gridcell" | "columnheader" | "rowheader" => Self::Cell,
            "dialog" | "alertdialog" => Self::Dialog,
            "alert" => Self::Alert,
            "tab" => Self::Tab,
            "tablist" => Self::TabList,
            "tabpanel" => Self::TabPanel,
            "menu" | "menubar" => Self::Menu,
            "menuitem" | "menuitemcheckbox" | "menuitemradio" => Self::MenuItem,
            "genericcontainer" | "generic_container" | "section" | "article" => {
                Self::GenericContainer
            }
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AxBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl AxBounds {
    pub fn is_finite(self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.width >= 0.0
            && self.height >= 0.0
    }
}

/// Owned accessibility node snapshot. `Debug` omits node text strings.
#[derive(Clone, PartialEq)]
pub struct AxNode {
    pub id: i32,
    pub role: AxRole,
    pub name: String,
    pub value: String,
    pub description: String,
    pub bounds: AxBounds,
    pub offset_container_id: Option<i32>,
    pub child_ids: Vec<i32>,
    pub focusable: bool,
    pub ignored: bool,
    pub checked: Option<bool>,
}

impl fmt::Debug for AxNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AxNode")
            .field("id", &self.id)
            .field("role", &self.role)
            .field("name_bytes", &self.name.len())
            .field("value_bytes", &self.value.len())
            .field("description_bytes", &self.description.len())
            .field("bounds", &self.bounds)
            .field("offset_container_id", &self.offset_container_id)
            .field("child_count", &self.child_ids.len())
            .field("focusable", &self.focusable)
            .field("ignored", &self.ignored)
            .field("checked", &self.checked)
            .finish()
    }
}

#[derive(Clone, PartialEq)]
pub struct AxNodeUpdate {
    pub id: i32,
    pub role: AxRole,
    pub name: String,
    pub value: String,
    pub description: String,
    pub bounds: AxBounds,
    pub offset_container_id: Option<i32>,
    pub child_ids: Vec<i32>,
    pub focusable: bool,
    pub ignored: bool,
    pub checked: Option<bool>,
}

impl AxNodeUpdate {
    pub fn into_bounded_node(mut self) -> AxNode {
        self.child_ids.truncate(MAX_AX_CHILDREN_PER_NODE);
        let bounds = if self.bounds.is_finite() {
            self.bounds
        } else {
            AxBounds::default()
        };
        AxNode {
            id: self.id,
            role: self.role,
            name: truncate_utf8_bytes(&self.name, MAX_AX_TEXT_BYTES),
            value: truncate_utf8_bytes(&self.value, MAX_AX_TEXT_BYTES),
            description: truncate_utf8_bytes(&self.description, MAX_AX_TEXT_BYTES),
            bounds,
            offset_container_id: self.offset_container_id,
            child_ids: self.child_ids,
            focusable: self.focusable,
            ignored: self.ignored,
            checked: self.checked,
        }
    }
}

impl fmt::Debug for AxNodeUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AxNodeUpdate")
            .field("id", &self.id)
            .field("role", &self.role)
            .field("name_bytes", &self.name.len())
            .field("value_bytes", &self.value.len())
            .field("description_bytes", &self.description.len())
            .field("bounds", &self.bounds)
            .field("child_count", &self.child_ids.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AxLocationChange {
    pub id: i32,
    pub offset_container_id: Option<i32>,
    pub bounds: AxBounds,
}

#[derive(Clone, Default, PartialEq)]
pub struct AxTreeDelta {
    pub tree_id: String,
    pub node_id_to_clear: Option<i32>,
    pub root_id: Option<i32>,
    pub focus_id: Option<i32>,
    pub nodes: Vec<AxNodeUpdate>,
}

impl fmt::Debug for AxTreeDelta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AxTreeDelta")
            .field("tree_id_bytes", &self.tree_id.len())
            .field("node_id_to_clear", &self.node_id_to_clear)
            .field("root_id", &self.root_id)
            .field("focus_id", &self.focus_id)
            .field("node_updates", &self.nodes.len())
            .finish()
    }
}

#[derive(Clone, Default, PartialEq)]
pub struct AxTreeSnapshot {
    pub generation: u64,
    pub tree_id: String,
    pub root_id: Option<i32>,
    pub focus_id: Option<i32>,
    pub nodes: BTreeMap<i32, AxNode>,
    pub truncated: bool,
}

impl AxTreeSnapshot {
    /// Resolves `id`'s bounds relative to the browser viewport root by summing
    /// finite ancestor offsets up to `MAX_AX_ANCESTOR_HOPS` with cycle protection.
    pub fn resolved_bounds(&self, id: i32) -> Option<AxBounds> {
        let node = self.nodes.get(&id)?;
        if !node.bounds.is_finite() {
            return None;
        }
        let mut x = node.bounds.x;
        let mut y = node.bounds.y;
        let mut visited = BTreeSet::new();
        visited.insert(id);

        let mut cursor = node.offset_container_id;
        let mut hops = 0usize;
        while let Some(parent_id) = cursor {
            if hops >= MAX_AX_ANCESTOR_HOPS || !visited.insert(parent_id) {
                break;
            }
            let Some(parent) = self.nodes.get(&parent_id) else {
                break;
            };
            if parent.bounds.x.is_finite() && parent.bounds.y.is_finite() {
                x += parent.bounds.x;
                y += parent.bounds.y;
            }
            cursor = parent.offset_container_id;
            hops += 1;
        }

        let resolved = AxBounds {
            x,
            y,
            width: node.bounds.width,
            height: node.bounds.height,
        };
        resolved.is_finite().then_some(resolved)
    }
}

impl fmt::Debug for AxTreeSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AxTreeSnapshot")
            .field("generation", &self.generation)
            .field("tree_id_bytes", &self.tree_id.len())
            .field("root_id", &self.root_id)
            .field("focus_id", &self.focus_id)
            .field("node_count", &self.nodes.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

#[derive(Default)]
pub struct AxTreeState {
    snapshot: AxTreeSnapshot,
}

impl AxTreeState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_delta(&mut self, delta: AxTreeDelta) -> &AxTreeSnapshot {
        if let Some(clear_id) = delta.node_id_to_clear {
            if self.snapshot.root_id == Some(clear_id) {
                self.snapshot.nodes.clear();
                self.snapshot.root_id = None;
                self.snapshot.focus_id = None;
                self.snapshot.truncated = false;
            } else {
                self.clear_subtree(clear_id);
            }
        }

        if !delta.tree_id.is_empty() {
            self.snapshot.tree_id = truncate_utf8_bytes(&delta.tree_id, MAX_AX_TEXT_BYTES);
        }
        if delta.root_id.is_some() {
            self.snapshot.root_id = delta.root_id;
        }
        if delta.focus_id.is_some() {
            self.snapshot.focus_id = delta.focus_id;
        }

        for update in delta.nodes {
            let bounded = update.into_bounded_node();
            let id = bounded.id;
            if self.snapshot.nodes.contains_key(&id) {
                self.snapshot.nodes.insert(id, bounded);
            } else if self.snapshot.nodes.len() < MAX_AX_NODES {
                self.snapshot.nodes.insert(id, bounded);
            } else {
                self.snapshot.truncated = true;
            }
        }

        self.snapshot.generation = self.snapshot.generation.saturating_add(1);
        &self.snapshot
    }

    pub fn apply_location_changes(&mut self, changes: &[AxLocationChange]) -> bool {
        let mut updated = false;
        for change in changes {
            if !change.bounds.is_finite() {
                continue;
            }
            if let Some(node) = self.snapshot.nodes.get_mut(&change.id) {
                node.bounds = change.bounds;
                if change.offset_container_id.is_some() {
                    node.offset_container_id = change.offset_container_id;
                }
                updated = true;
            }
        }
        if updated {
            self.snapshot.generation = self.snapshot.generation.saturating_add(1);
        }
        updated
    }

    pub fn clear(&mut self) {
        self.snapshot.nodes.clear();
        self.snapshot.root_id = None;
        self.snapshot.focus_id = None;
        self.snapshot.tree_id.clear();
        self.snapshot.truncated = false;
        self.snapshot.generation = self.snapshot.generation.saturating_add(1);
    }

    pub fn snapshot(&self) -> AxTreeSnapshot {
        self.snapshot.clone()
    }

    fn clear_subtree(&mut self, root_id: i32) {
        let mut stack = vec![root_id];
        let mut visited = BTreeSet::new();
        while let Some(id) = stack.pop() {
            if !visited.insert(id) {
                continue;
            }
            if let Some(removed) = self.snapshot.nodes.remove(&id) {
                for child_id in removed.child_ids {
                    if !visited.contains(&child_id) {
                        stack.push(child_id);
                    }
                }
            }
            if self.snapshot.root_id == Some(id) {
                self.snapshot.root_id = None;
            }
            if self.snapshot.focus_id == Some(id) {
                self.snapshot.focus_id = None;
            }
        }
    }
}

/// Coalescing single-slot latest AXTree handoff between runtime and UI.
#[derive(Clone, Default)]
pub struct AxTreeSlot {
    inner: Arc<Mutex<Option<AxTreeSnapshot>>>,
}

impl AxTreeSlot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn publish(&self, snapshot: AxTreeSnapshot) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(snapshot);
    }

    pub fn take_latest(&self) -> Option<AxTreeSnapshot> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.take()
    }

    pub fn snapshot(&self) -> Option<AxTreeSnapshot> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.clone()
    }

    pub fn clear(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: i32, role: AxRole, name: &str, child_ids: Vec<i32>) -> AxNodeUpdate {
        AxNodeUpdate {
            id,
            role,
            name: name.into(),
            value: String::new(),
            description: String::new(),
            bounds: AxBounds {
                x: 10.0,
                y: 20.0,
                width: 100.0,
                height: 30.0,
            },
            offset_container_id: None,
            child_ids,
            focusable: true,
            ignored: false,
            checked: None,
        }
    }

    #[test]
    fn cef_role_strings_map_accurately() {
        assert_eq!(AxRole::from_cef_name("rootWebArea"), AxRole::RootWebArea);
        assert_eq!(AxRole::from_cef_name("heading"), AxRole::Heading);
        assert_eq!(AxRole::from_cef_name("link"), AxRole::Link);
        assert_eq!(AxRole::from_cef_name("button"), AxRole::Button);
        assert_eq!(
            AxRole::from_cef_name("textFieldWithComboBox"),
            AxRole::ComboBox
        );
        assert_eq!(AxRole::from_cef_name("unknownCustom"), AxRole::Unknown);
    }

    #[test]
    fn delta_caps_text_children_and_max_nodes_and_clears_subtrees_safely() {
        let mut state = AxTreeState::new();
        let long_name = "字".repeat(MAX_AX_TEXT_BYTES);
        let too_many_children: Vec<i32> = (1..=300).collect();

        let mut updates = Vec::with_capacity(MAX_AX_NODES + 5);
        updates.push(AxNodeUpdate {
            name: long_name,
            child_ids: too_many_children,
            // Deliberate cycle check back to node 0 when clearing later.
            ..node(0, AxRole::RootWebArea, "", vec![1, 2])
        });
        for id in 1..(MAX_AX_NODES as i32 + 5) {
            updates.push(node(id, AxRole::Button, "btn", vec![0]));
        }

        let snap = state.apply_delta(AxTreeDelta {
            tree_id: "tree-1".into(),
            node_id_to_clear: None,
            root_id: Some(0),
            focus_id: Some(1),
            nodes: updates,
        });

        assert_eq!(snap.nodes.len(), MAX_AX_NODES);
        assert!(snap.truncated);
        assert!(snap.nodes[&0].name.len() <= MAX_AX_TEXT_BYTES);
        assert_eq!(snap.nodes[&0].child_ids.len(), MAX_AX_CHILDREN_PER_NODE);

        // Non-root cyclic subtree clear removes subtree safely without touching root.
        let mut branch_state = AxTreeState::new();
        branch_state.apply_delta(AxTreeDelta {
            tree_id: "branch".into(),
            node_id_to_clear: None,
            root_id: Some(1),
            focus_id: Some(3),
            nodes: vec![
                node(1, AxRole::RootWebArea, "root", vec![2, 4]),
                node(2, AxRole::GenericContainer, "sub", vec![3]),
                node(3, AxRole::Button, "cycle", vec![2]),
                node(4, AxRole::Link, "keep", Vec::new()),
            ],
        });
        let branch_after = branch_state.apply_delta(AxTreeDelta {
            tree_id: String::new(),
            node_id_to_clear: Some(2),
            root_id: None,
            focus_id: None,
            nodes: Vec::new(),
        });
        assert!(branch_after.nodes.contains_key(&1));
        assert!(branch_after.nodes.contains_key(&4));
        assert!(!branch_after.nodes.contains_key(&2));
        assert!(!branch_after.nodes.contains_key(&3));
        assert_eq!(branch_after.focus_id, None);

        // Clearing root rebuilds cleanly even after truncation.
        let cleared = state.apply_delta(AxTreeDelta {
            tree_id: String::new(),
            node_id_to_clear: Some(0),
            root_id: Some(99),
            focus_id: None,
            nodes: vec![node(99, AxRole::Document, "FRESH", Vec::new())],
        });
        assert!(!cleared.truncated);
        assert_eq!(cleared.nodes.len(), 1);
        assert_eq!(cleared.focus_id, None);
    }

    #[test]
    fn resolved_bounds_walks_offset_containers_with_cycle_protection_and_location_updates() {
        let mut state = AxTreeState::new();
        let mut parent = node(1, AxRole::GenericContainer, "parent", vec![2]);
        parent.bounds = AxBounds {
            x: 100.0,
            y: 50.0,
            width: 500.0,
            height: 400.0,
        };
        let mut child = node(2, AxRole::Button, "child", Vec::new());
        child.offset_container_id = Some(1);
        child.bounds = AxBounds {
            x: 12.0,
            y: 8.0,
            width: 80.0,
            height: 24.0,
        };

        state.apply_delta(AxTreeDelta {
            tree_id: "t".into(),
            node_id_to_clear: None,
            root_id: Some(1),
            focus_id: None,
            nodes: vec![parent, child],
        });

        let resolved = state.snapshot().resolved_bounds(2).unwrap();
        assert_eq!(resolved.x, 112.0);
        assert_eq!(resolved.y, 58.0);
        assert_eq!(resolved.width, 80.0);

        assert!(state.apply_location_changes(&[AxLocationChange {
            id: 2,
            offset_container_id: Some(1),
            bounds: AxBounds {
                x: 20.0,
                y: 10.0,
                width: 90.0,
                height: 28.0,
            },
        }]));
        let updated = state.snapshot().resolved_bounds(2).unwrap();
        assert_eq!(updated.x, 120.0);
        assert_eq!(updated.y, 60.0);
    }

    #[test]
    fn debug_redacts_node_text_and_slot_coalesces() {
        let mut state = AxTreeState::new();
        state.apply_delta(AxTreeDelta {
            tree_id: "secret-tree".into(),
            node_id_to_clear: None,
            root_id: Some(1),
            focus_id: Some(1),
            nodes: vec![AxNodeUpdate {
                value: "SECRET_PASSWORD_INPUT".into(),
                description: "SECRET_HINT".into(),
                ..node(1, AxRole::TextField, "SECRET_LABEL", Vec::new())
            }],
        });
        let snap = state.snapshot();
        let dbg = format!("{snap:?} {:?}", snap.nodes[&1]);
        assert!(!dbg.contains("SECRET_PASSWORD"));
        assert!(!dbg.contains("SECRET_LABEL"));
        assert!(!dbg.contains("SECRET_HINT"));

        let slot = AxTreeSlot::new();
        slot.publish(snap);
        state.apply_delta(AxTreeDelta::default());
        slot.publish(state.snapshot());
        let latest = slot.take_latest().unwrap();
        assert_eq!(latest.generation, 2);
        assert_eq!(slot.take_latest(), None);
    }
}
