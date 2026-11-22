//! Projection from bounded `AxTreeSnapshot` nodes to `gpui::accesskit` synthetic UIA children.
//!
//! Structural reference (`D:\System Dir\Downloads\CefSharp`, rev `41919703`):
//! - `CefSharp.Wpf/Experimental/Accessibility/AccessibilityTree.cs:12-82`
//! - `CefSharp.Wpf/Experimental/Accessibility/AccessibilityNode.cs:17-71, 160-258`
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::collections::BTreeSet;

use gpui::accesskit::{Action, Node, Rect, Role, Toggled};
use rust_cef_core::accessibility::{AxBounds, AxNode, AxRole, AxTreeSnapshot, MAX_AX_NODES};

/// Maps every `AxRole` variant explicitly to its `accesskit::Role`.
pub fn map_ax_role(role: AxRole) -> Role {
    match role {
        AxRole::RootWebArea => Role::RootWebArea,
        AxRole::WebArea | AxRole::Document => Role::Document,
        AxRole::Heading => Role::Heading,
        AxRole::Link => Role::Link,
        AxRole::Button => Role::Button,
        AxRole::TextField => Role::TextInput,
        AxRole::StaticText => Role::Label,
        AxRole::CheckBox => Role::CheckBox,
        AxRole::RadioButton => Role::RadioButton,
        AxRole::ComboBox => Role::ComboBox,
        AxRole::List => Role::List,
        AxRole::ListItem => Role::ListItem,
        AxRole::Image => Role::Image,
        AxRole::Paragraph => Role::Paragraph,
        AxRole::Region => Role::Region,
        AxRole::Navigation => Role::Navigation,
        AxRole::Main => Role::Main,
        AxRole::Banner => Role::Banner,
        AxRole::ContentInfo => Role::ContentInfo,
        AxRole::Form => Role::Form,
        AxRole::Table => Role::Table,
        AxRole::Row => Role::Row,
        AxRole::Cell => Role::Cell,
        AxRole::Dialog => Role::Dialog,
        AxRole::Alert => Role::Alert,
        AxRole::Tab => Role::Tab,
        AxRole::TabList => Role::TabList,
        AxRole::TabPanel => Role::TabPanel,
        AxRole::Menu => Role::Menu,
        AxRole::MenuItem => Role::MenuItem,
        AxRole::GenericContainer => Role::GenericContainer,
        AxRole::Unknown => Role::Unknown,
    }
}

/// Converts view-relative DIP bounds into physical window coordinates expected by GPUI AccessKit.
pub fn translate_ax_bounds(
    bounds: AxBounds,
    view_origin_dip: (f32, f32),
    scale: f32,
) -> Option<Rect> {
    let (ox, oy) = view_origin_dip;
    if !ox.is_finite()
        || !oy.is_finite()
        || !scale.is_finite()
        || scale <= 0.0
        || !bounds.is_finite()
        || bounds.width <= 0.0
        || bounds.height <= 0.0
    {
        return None;
    }
    let x0 = ((ox + bounds.x) * scale) as f64;
    let y0 = ((oy + bounds.y) * scale) as f64;
    let x1 = ((ox + bounds.x + bounds.width) * scale) as f64;
    let y1 = ((oy + bounds.y + bounds.height) * scale) as f64;
    Some(Rect { x0, y0, x1, y1 })
}

/// Builds a standalone leaf `accesskit::Node` from one `AxNode` without parent-owned child IDs.
pub fn build_accesskit_node(
    snapshot: &AxTreeSnapshot,
    ax_node: &AxNode,
    view_origin_dip: (f32, f32),
    scale: f32,
) -> Node {
    let mut node = Node::new(map_ax_role(ax_node.role));
    if !ax_node.name.is_empty() {
        node.set_label(ax_node.name.clone());
    }
    if !ax_node.value.is_empty() {
        node.set_value(ax_node.value.clone());
    }
    if !ax_node.description.is_empty() {
        node.set_description(ax_node.description.clone());
    }
    if let Some(resolved) = snapshot.resolved_bounds(ax_node.id)
        && let Some(rect) = translate_ax_bounds(resolved, view_origin_dip, scale)
    {
        node.set_bounds(rect);
    }
    if ax_node.focusable {
        node.add_action(Action::Focus);
    }
    if let Some(checked) = ax_node.checked {
        node.set_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        });
    }
    node
}

fn should_skip_emit(ax_node: &AxNode) -> bool {
    ax_node.ignored
        || (ax_node.role == AxRole::GenericContainer
            && ax_node.name.is_empty()
            && ax_node.value.is_empty()
            && ax_node.description.is_empty())
}

fn visit_pre_order(
    id: i32,
    snapshot: &AxTreeSnapshot,
    view_origin_dip: (f32, f32),
    scale: f32,
    visited: &mut BTreeSet<i32>,
    out: &mut Vec<(i32, Node)>,
) {
    if out.len() >= MAX_AX_NODES || !visited.insert(id) {
        return;
    }
    let Some(ax_node) = snapshot.nodes.get(&id) else {
        return;
    };
    if !should_skip_emit(ax_node) {
        out.push((
            id,
            build_accesskit_node(snapshot, ax_node, view_origin_dip, scale),
        ));
    }
    for &child_id in &ax_node.child_ids {
        if out.len() >= MAX_AX_NODES {
            break;
        }
        visit_pre_order(child_id, snapshot, view_origin_dip, scale, visited, out);
    }
}

/// Collects deterministic, cycle-safe AccessKit leaf nodes bounded by `MAX_AX_NODES`.
pub fn collect_projected_nodes(
    snapshot: &AxTreeSnapshot,
    view_origin_dip: (f32, f32),
    scale: f32,
) -> Vec<(i32, Node)> {
    let mut visited = BTreeSet::new();
    let mut out = Vec::new();
    if let Some(root_id) = snapshot.root_id
        && snapshot.nodes.contains_key(&root_id)
    {
        visit_pre_order(
            root_id,
            snapshot,
            view_origin_dip,
            scale,
            &mut visited,
            &mut out,
        );
    } else {
        for &id in snapshot.nodes.keys() {
            if out.len() >= MAX_AX_NODES {
                break;
            }
            visit_pre_order(id, snapshot, view_origin_dip, scale, &mut visited, &mut out);
        }
    }
    out
}

/// Emits coalesced `AxTreeSnapshot` nodes into a GPUI element's `A11ySubtreeBuilder`.
pub fn project_ax_snapshot(
    snapshot: &AxTreeSnapshot,
    view_origin_dip: (f32, f32),
    scale: f32,
    builder: &mut gpui::A11ySubtreeBuilder<'_>,
) -> usize {
    if snapshot.nodes.is_empty() {
        return 0;
    }
    if let Some(root) = snapshot.root_id.and_then(|id| snapshot.nodes.get(&id))
        && !root.name.is_empty()
    {
        builder.parent_node().set_label(root.name.clone());
    }
    let mut pushed = 0usize;
    for (ax_id, node) in collect_projected_nodes(snapshot, view_origin_dip, scale) {
        let id = builder.synthetic_node_id(("cef-ax", ax_id));
        if builder.push_child(id, node) {
            pushed += 1;
        }
    }
    pushed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn node(id: i32, role: AxRole) -> AxNode {
        AxNode {
            id,
            role,
            name: String::new(),
            value: String::new(),
            description: String::new(),
            bounds: AxBounds {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 24.0,
            },
            offset_container_id: None,
            child_ids: Vec::new(),
            focusable: false,
            ignored: false,
            checked: None,
        }
    }

    #[test]
    fn maps_every_ax_role_variant() {
        let roles = [
            (AxRole::RootWebArea, Role::RootWebArea),
            (AxRole::WebArea, Role::Document),
            (AxRole::Document, Role::Document),
            (AxRole::Heading, Role::Heading),
            (AxRole::Link, Role::Link),
            (AxRole::Button, Role::Button),
            (AxRole::TextField, Role::TextInput),
            (AxRole::StaticText, Role::Label),
            (AxRole::CheckBox, Role::CheckBox),
            (AxRole::RadioButton, Role::RadioButton),
            (AxRole::ComboBox, Role::ComboBox),
            (AxRole::List, Role::List),
            (AxRole::ListItem, Role::ListItem),
            (AxRole::Image, Role::Image),
            (AxRole::Paragraph, Role::Paragraph),
            (AxRole::Region, Role::Region),
            (AxRole::Navigation, Role::Navigation),
            (AxRole::Main, Role::Main),
            (AxRole::Banner, Role::Banner),
            (AxRole::ContentInfo, Role::ContentInfo),
            (AxRole::Form, Role::Form),
            (AxRole::Table, Role::Table),
            (AxRole::Row, Role::Row),
            (AxRole::Cell, Role::Cell),
            (AxRole::Dialog, Role::Dialog),
            (AxRole::Alert, Role::Alert),
            (AxRole::Tab, Role::Tab),
            (AxRole::TabList, Role::TabList),
            (AxRole::TabPanel, Role::TabPanel),
            (AxRole::Menu, Role::Menu),
            (AxRole::MenuItem, Role::MenuItem),
            (AxRole::GenericContainer, Role::GenericContainer),
            (AxRole::Unknown, Role::Unknown),
        ];
        for (ax, ak) in roles {
            assert_eq!(map_ax_role(ax), ak);
        }
    }

    #[test]
    fn translates_bounds_with_origin_and_hidpi_scale_and_rejects_invalid() {
        let rect = translate_ax_bounds(
            AxBounds {
                x: 10.0,
                y: 20.0,
                width: 80.0,
                height: 30.0,
            },
            (100.0, 50.0),
            1.5,
        )
        .unwrap();
        assert_eq!(
            rect,
            Rect {
                x0: 165.0,
                y0: 105.0,
                x1: 285.0,
                y1: 150.0,
            }
        );

        assert_eq!(
            translate_ax_bounds(
                AxBounds {
                    x: 0.0,
                    y: 0.0,
                    width: 0.0,
                    height: 10.0,
                },
                (0.0, 0.0),
                1.0,
            ),
            None
        );
        assert_eq!(
            translate_ax_bounds(
                AxBounds {
                    x: f32::NAN,
                    y: 0.0,
                    width: 10.0,
                    height: 10.0,
                },
                (0.0, 0.0),
                1.0,
            ),
            None
        );
        assert_eq!(
            translate_ax_bounds(
                AxBounds {
                    x: 1.0,
                    y: 1.0,
                    width: 10.0,
                    height: 10.0,
                },
                (0.0, 0.0),
                0.0,
            ),
            None
        );
    }

    #[test]
    fn flattens_ignored_and_empty_generic_containers_with_cycle_protection() {
        let mut nodes = BTreeMap::new();
        let mut root = node(1, AxRole::RootWebArea);
        root.name = "Example Page".into();
        root.child_ids = vec![2, 3];

        let mut container = node(2, AxRole::GenericContainer);
        container.child_ids = vec![4, 1]; // Cycle back to root

        let mut ignored = node(3, AxRole::Region);
        ignored.ignored = true;
        ignored.child_ids = vec![5];

        let mut heading = node(4, AxRole::Heading);
        heading.name = "Welcome".into();
        heading.offset_container_id = Some(2);
        heading.bounds = AxBounds {
            x: 10.0,
            y: 5.0,
            width: 50.0,
            height: 20.0,
        };

        let mut check = node(5, AxRole::CheckBox);
        check.name = "Accept".into();
        check.value = "on".into();
        check.description = "Terms".into();
        check.focusable = true;
        check.checked = Some(true);

        nodes.insert(1, root);
        nodes.insert(2, container);
        nodes.insert(3, ignored);
        nodes.insert(4, heading);
        nodes.insert(5, check);

        let snapshot = AxTreeSnapshot {
            generation: 1,
            tree_id: "t".into(),
            root_id: Some(1),
            focus_id: Some(5),
            nodes,
            truncated: false,
        };

        let projected = collect_projected_nodes(&snapshot, (100.0, 200.0), 2.0);
        let ids: Vec<i32> = projected.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![1, 4, 5]);

        let cb_node = &projected[2].1;
        assert_eq!(cb_node.role(), Role::CheckBox);
        assert_eq!(cb_node.label(), Some("Accept"));
        assert_eq!(cb_node.value(), Some("on"));
        assert_eq!(cb_node.description(), Some("Terms"));
        assert_eq!(cb_node.toggled(), Some(Toggled::True));
        assert!(cb_node.supports_action(Action::Focus));
    }

    #[test]
    fn empty_snapshot_yields_no_projected_nodes() {
        let snapshot = AxTreeSnapshot::default();
        assert!(collect_projected_nodes(&snapshot, (0.0, 0.0), 1.0).is_empty());
    }
}
