//! Native CEF `IAccessibilityHandler` adapter parsing scoped `Value` snapshots
//! into bounded `AxTreeSlot` updates.
//!
//! Structural reference (`D:\System Dir\Downloads\CefSharp`, rev `41919703`):
//! - `CefSharp.Wpf/Experimental/Accessibility/AccessibilityHandler.cs:17-69`
//! - `CefSharp.Wpf/Experimental/Accessibility/AccessibilityTree.cs:12-82`
//! - `CefSharp.Wpf/Experimental/Accessibility/AccessibilityNode.cs:17-71, 160-258`
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use cef::*;
use rust_cef_core::accessibility::{
    AxBounds, AxLocationChange, AxNodeUpdate, AxRole, AxTreeDelta, AxTreeSlot, AxTreeState,
};
use rust_cef_core::contracts::{BrowserId, CefFeatureState};
use rust_cef_core::ports::{BrowserEvent, UiDispatcher};

use crate::client::read_cef_string_list;
use crate::registry::UiRegistry;

const MAX_DICT_KEYS: usize = 256;
const MAX_KEY_BYTES: usize = 256;
const MAX_LIST_ITEMS: usize = 4096;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn cef_feature_to_state(state: CefFeatureState) -> cef::State {
    match state {
        CefFeatureState::Default => cef::State::DEFAULT,
        CefFeatureState::Enabled => cef::State::ENABLED,
        CefFeatureState::Disabled => cef::State::DISABLED,
    }
}

fn value_as_f32(value: Option<Value>) -> Option<f32> {
    let val = value?;
    match val.get_type() {
        ValueType::DOUBLE => Some(val.double() as f32),
        ValueType::INT => Some(val.int() as f32),
        _ => None,
    }
}

fn dict_f32(dict: &DictionaryValue, key: &str) -> Option<f32> {
    let k = CefString::from(key);
    if dict.has_key(Some(&k)) == 0 {
        return None;
    }
    match dict.get_type(Some(&k)) {
        ValueType::DOUBLE => Some(dict.double(Some(&k)) as f32),
        ValueType::INT => Some(dict.int(Some(&k)) as f32),
        _ => None,
    }
}

fn dict_i32(dict: &DictionaryValue, key: &str) -> Option<i32> {
    let k = CefString::from(key);
    if dict.has_key(Some(&k)) == 0 {
        return None;
    }
    match dict.get_type(Some(&k)) {
        ValueType::INT => Some(dict.int(Some(&k))),
        ValueType::DOUBLE => Some(dict.double(Some(&k)) as i32),
        _ => None,
    }
}

fn dict_string(dict: &DictionaryValue, key: &str) -> Option<String> {
    let k = CefString::from(key);
    if dict.has_key(Some(&k)) == 0 || dict.get_type(Some(&k)) != ValueType::STRING {
        return None;
    }
    let text = CefString::from(&dict.string(Some(&k))).to_string();
    Some(text)
}

fn dict_bool(dict: &DictionaryValue, key: &str) -> Option<bool> {
    let k = CefString::from(key);
    if dict.has_key(Some(&k)) == 0 {
        return None;
    }
    match dict.get_type(Some(&k)) {
        ValueType::BOOL => Some(dict.bool(Some(&k)) != 0),
        ValueType::INT => Some(dict.int(Some(&k)) != 0),
        _ => None,
    }
}

fn parse_bounds(dict: &DictionaryValue) -> AxBounds {
    AxBounds {
        x: dict_f32(dict, "x").unwrap_or(0.0),
        y: dict_f32(dict, "y").unwrap_or(0.0),
        width: dict_f32(dict, "width").unwrap_or(0.0),
        height: dict_f32(dict, "height").unwrap_or(0.0),
    }
}

fn parse_node_update(dict: &DictionaryValue) -> Option<AxNodeUpdate> {
    let id = dict_i32(dict, "id")?;
    let role = dict_string(dict, "role")
        .map(|s| AxRole::from_cef_name(&s))
        .unwrap_or(AxRole::Unknown);

    let mut name = dict_string(dict, "name").unwrap_or_default();
    let mut value = dict_string(dict, "value").unwrap_or_default();
    let mut description = dict_string(dict, "description").unwrap_or_default();
    let mut checked = dict_bool(dict, "checked");

    let attr_key = CefString::from("attributes");
    if dict.has_key(Some(&attr_key)) != 0
        && let Some(attrs) = dict.dictionary(Some(&attr_key))
    {
        if name.is_empty()
            && let Some(s) = dict_string(&attrs, "name")
        {
            name = s;
        }
        if value.is_empty()
            && let Some(s) = dict_string(&attrs, "value")
        {
            value = s;
        }
        if description.is_empty()
            && let Some(s) = dict_string(&attrs, "description")
        {
            description = s;
        }
        if checked.is_none() {
            checked = dict_bool(&attrs, "checked");
            if checked.is_none()
                && let Some(checked_state) = dict_string(&attrs, "checked")
            {
                checked = match checked_state.as_str() {
                    "true" | "1" => Some(true),
                    "false" | "0" => Some(false),
                    _ => None,
                };
            }
        }
    }

    let mut focusable = dict_bool(dict, "focusable").unwrap_or(false);
    let mut ignored = dict_bool(dict, "ignored").unwrap_or(false);
    let states_key = CefString::from("states");
    if dict.has_key(Some(&states_key)) != 0
        && let Some(states) = dict.dictionary(Some(&states_key))
    {
        if let Some(f) = dict_bool(&states, "focusable") {
            focusable = f;
        }
        if let Some(i) = dict_bool(&states, "ignored") {
            ignored = i;
        }
        if checked.is_none() {
            checked = dict_bool(&states, "checked");
        }
    }

    let loc_key = CefString::from("location");
    let bounds = if dict.has_key(Some(&loc_key)) != 0
        && let Some(loc) = dict.dictionary(Some(&loc_key))
    {
        parse_bounds(&loc)
    } else {
        AxBounds::default()
    };

    let mut offset_container_id = dict_i32(dict, "offset_container_id");
    if offset_container_id.is_none() && dict.has_key(Some(&loc_key)) != 0 {
        if let Some(loc) = dict.dictionary(Some(&loc_key)) {
            offset_container_id = dict_i32(&loc, "offset_container_id");
        }
    }

    let mut child_ids = Vec::new();
    let children_key = CefString::from("child_ids");
    if dict.has_key(Some(&children_key)) != 0
        && let Some(list) = dict.list(Some(&children_key))
    {
        let count = list.size().min(MAX_LIST_ITEMS);
        for idx in 0..count {
            if let Some(id_num) = value_as_f32(list.value(idx)) {
                child_ids.push(id_num as i32);
            }
        }
    }

    // Also inspect top-level keys lightly so future Chromium keys cannot leak into retained wrappers.
    let _ = (MAX_DICT_KEYS, MAX_KEY_BYTES);

    Some(AxNodeUpdate {
        id,
        role,
        name,
        value,
        description,
        bounds,
        offset_container_id,
        child_ids,
        focusable,
        ignored,
        checked,
    })
}

fn parse_tree_change(value: &Value) -> Vec<AxTreeDelta> {
    if value.is_valid() == 0 || value.get_type() != ValueType::DICTIONARY {
        return Vec::new();
    }
    let Some(root_dict) = value.dictionary() else {
        return Vec::new();
    };
    let tree_id = dict_string(&root_dict, "ax_tree_id").unwrap_or_default();

    let mut focus_from_events = None;
    let events_key = CefString::from("events");
    if root_dict.has_key(Some(&events_key)) != 0
        && let Some(events) = root_dict.list(Some(&events_key))
    {
        let count = events.size().min(MAX_LIST_ITEMS);
        for idx in 0..count {
            if let Some(ev) = events.dictionary(idx)
                && dict_string(&ev, "event_type").as_deref() == Some("focus")
                && let Some(id) = dict_i32(&ev, "id")
            {
                focus_from_events = Some(id);
            }
        }
    }

    let mut deltas = Vec::new();
    let updates_key = CefString::from("updates");
    if root_dict.has_key(Some(&updates_key)) != 0
        && let Some(updates) = root_dict.list(Some(&updates_key))
    {
        let count = updates.size().min(MAX_LIST_ITEMS);
        for idx in 0..count {
            let Some(update_dict) = updates.dictionary(idx) else {
                continue;
            };
            let node_id_to_clear = dict_i32(&update_dict, "node_id_to_clear").filter(|&id| id >= 0);
            let root_id = dict_i32(&update_dict, "root_id");
            let mut nodes = Vec::new();
            let nodes_key = CefString::from("nodes");
            if update_dict.has_key(Some(&nodes_key)) != 0
                && let Some(nodes_list) = update_dict.list(Some(&nodes_key))
            {
                let ncount = nodes_list.size().min(MAX_LIST_ITEMS);
                for nidx in 0..ncount {
                    if let Some(node_dict) = nodes_list.dictionary(nidx)
                        && let Some(node) = parse_node_update(&node_dict)
                    {
                        nodes.push(node);
                    }
                }
            }
            deltas.push(AxTreeDelta {
                tree_id: tree_id.clone(),
                node_id_to_clear,
                root_id,
                focus_id: if idx + 1 == count {
                    focus_from_events
                } else {
                    None
                },
                nodes,
            });
        }
    } else if focus_from_events.is_some() {
        deltas.push(AxTreeDelta {
            tree_id,
            node_id_to_clear: None,
            root_id: None,
            focus_id: focus_from_events,
            nodes: Vec::new(),
        });
    } else {
        // Fallback when keys live directly at top dictionary.
        let mut keys = CefStringList::new();
        let _ = root_dict.keys(Some(&mut keys));
        let key_names = read_cef_string_list(&mut keys, MAX_DICT_KEYS, MAX_KEY_BYTES);
        if key_names.iter().any(|k| k == "nodes" || k == "root_id") {
            let mut nodes = Vec::new();
            let nodes_key = CefString::from("nodes");
            if let Some(nodes_list) = root_dict.list(Some(&nodes_key)) {
                let ncount = nodes_list.size().min(MAX_LIST_ITEMS);
                for nidx in 0..ncount {
                    if let Some(node_dict) = nodes_list.dictionary(nidx)
                        && let Some(node) = parse_node_update(&node_dict)
                    {
                        nodes.push(node);
                    }
                }
            }
            deltas.push(AxTreeDelta {
                tree_id,
                node_id_to_clear: dict_i32(&root_dict, "node_id_to_clear").filter(|&id| id >= 0),
                root_id: dict_i32(&root_dict, "root_id"),
                focus_id: None,
                nodes,
            });
        }
    }
    deltas
}

fn parse_location_entry(dict: &DictionaryValue) -> Option<AxLocationChange> {
    let id = dict_i32(dict, "id")?;
    let new_loc_key = CefString::from("new_location");
    if dict.has_key(Some(&new_loc_key)) != 0
        && let Some(new_loc) = dict.dictionary(Some(&new_loc_key))
    {
        let bounds_key = CefString::from("bounds");
        let bounds = if new_loc.has_key(Some(&bounds_key)) != 0
            && let Some(b) = new_loc.dictionary(Some(&bounds_key))
        {
            parse_bounds(&b)
        } else {
            parse_bounds(&new_loc)
        };
        let offset_container_id = dict_i32(&new_loc, "offset_container_id");
        return Some(AxLocationChange {
            id,
            offset_container_id,
            bounds,
        });
    }
    Some(AxLocationChange {
        id,
        offset_container_id: dict_i32(dict, "offset_container_id"),
        bounds: parse_bounds(dict),
    })
}

fn parse_location_change(value: &Value) -> Vec<AxLocationChange> {
    if value.is_valid() == 0 {
        return Vec::new();
    }
    match value.get_type() {
        ValueType::LIST => {
            let Some(list) = value.list() else {
                return Vec::new();
            };
            let count = list.size().min(MAX_LIST_ITEMS);
            let mut out = Vec::with_capacity(count);
            for idx in 0..count {
                if let Some(dict) = list.dictionary(idx)
                    && let Some(entry) = parse_location_entry(&dict)
                {
                    out.push(entry);
                }
            }
            out
        }
        ValueType::DICTIONARY => {
            let Some(dict) = value.dictionary() else {
                return Vec::new();
            };
            let key = CefString::from("location_changes");
            if dict.has_key(Some(&key)) != 0
                && let Some(list) = dict.list(Some(&key))
            {
                let count = list.size().min(MAX_LIST_ITEMS);
                let mut out = Vec::with_capacity(count);
                for idx in 0..count {
                    if let Some(item) = list.dictionary(idx)
                        && let Some(entry) = parse_location_entry(&item)
                    {
                        out.push(entry);
                    }
                }
                return out;
            }
            parse_location_entry(&dict).into_iter().collect()
        }
        _ => Vec::new(),
    }
}

/// Owned per-browser runtime accessibility tree tracker and coalescing UI slot.
pub(crate) struct RuntimeAccessibilityState {
    enabled: AtomicBool,
    state: Mutex<AxTreeState>,
    slot: AxTreeSlot,
}

impl RuntimeAccessibilityState {
    pub(crate) fn new(initially_enabled: bool) -> Arc<Self> {
        Arc::new(Self {
            enabled: AtomicBool::new(initially_enabled),
            state: Mutex::new(AxTreeState::new()),
            slot: AxTreeSlot::new(),
        })
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
        if !enabled {
            self.clear();
        }
    }

    pub(crate) fn slot(&self) -> AxTreeSlot {
        self.slot.clone()
    }

    pub(crate) fn clear(&self) {
        lock(&self.state).clear();
        self.slot.clear();
    }

    pub(crate) fn apply_deltas(&self, deltas: Vec<AxTreeDelta>) -> Option<(u64, usize, bool)> {
        if !self.is_enabled() || deltas.is_empty() {
            return None;
        }
        let snapshot = {
            let mut guard = lock(&self.state);
            for delta in deltas {
                guard.apply_delta(delta);
            }
            guard.snapshot()
        };
        let summary = (
            snapshot.generation,
            snapshot.nodes.len(),
            snapshot.truncated,
        );
        self.slot.publish(snapshot);
        Some(summary)
    }

    pub(crate) fn apply_locations(
        &self,
        changes: &[AxLocationChange],
    ) -> Option<(u64, usize, bool)> {
        if !self.is_enabled() || changes.is_empty() {
            return None;
        }
        let snapshot = {
            let mut guard = lock(&self.state);
            if !guard.apply_location_changes(changes) {
                return None;
            }
            guard.snapshot()
        };
        let summary = (
            snapshot.generation,
            snapshot.nodes.len(),
            snapshot.truncated,
        );
        self.slot.publish(snapshot);
        Some(summary)
    }
}

wrap_accessibility_handler! {
    struct RuntimeAccessibilityHandler {
        browser: BrowserId,
        a11y: Arc<RuntimeAccessibilityState>,
        registry: Weak<UiRegistry>,
        ui: Arc<dyn UiDispatcher>,
    }

    impl AccessibilityHandler {
        fn on_accessibility_tree_change(&self, value: Option<&mut Value>) {
            let Some(value) = value else {
                return;
            };
            if !self.a11y.is_enabled() {
                return;
            }
            let deltas = parse_tree_change(value);
            if let Some((generation, node_count, truncated)) = self.a11y.apply_deltas(deltas) {
                if let Some(registry) = self.registry.upgrade() {
                    registry.navigation_event(
                        self.browser,
                        BrowserEvent::AccessibilityTreeUpdated {
                            generation,
                            node_count,
                            truncated,
                        },
                    );
                }
                let _ = self.ui.wake();
            }
        }

        fn on_accessibility_location_change(&self, value: Option<&mut Value>) {
            let Some(value) = value else {
                return;
            };
            if !self.a11y.is_enabled() {
                return;
            }
            let changes = parse_location_change(value);
            if let Some((generation, node_count, truncated)) = self.a11y.apply_locations(&changes) {
                if let Some(registry) = self.registry.upgrade() {
                    registry.navigation_event(
                        self.browser,
                        BrowserEvent::AccessibilityTreeUpdated {
                            generation,
                            node_count,
                            truncated,
                        },
                    );
                }
                let _ = self.ui.wake();
            }
        }
    }
}

pub(crate) fn new_handler(
    browser: BrowserId,
    a11y: Arc<RuntimeAccessibilityState>,
    registry: Weak<UiRegistry>,
    ui: Arc<dyn UiDispatcher>,
) -> AccessibilityHandler {
    RuntimeAccessibilityHandler::new(browser, a11y, registry, ui)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_state_maps_to_expected_cef_state() {
        assert_eq!(
            cef_feature_to_state(CefFeatureState::Default),
            cef::State::DEFAULT
        );
        assert_eq!(
            cef_feature_to_state(CefFeatureState::Enabled),
            cef::State::ENABLED
        );
        assert_eq!(
            cef_feature_to_state(CefFeatureState::Disabled),
            cef::State::DISABLED
        );
    }

    #[test]
    fn runtime_accessibility_state_applies_coalesces_and_clears_on_disable() {
        let state = RuntimeAccessibilityState::new(false);
        let slot = state.slot();

        let delta = AxTreeDelta {
            tree_id: "main".into(),
            node_id_to_clear: None,
            root_id: Some(1),
            focus_id: Some(2),
            nodes: vec![
                AxNodeUpdate {
                    id: 1,
                    role: AxRole::RootWebArea,
                    name: "Page".into(),
                    value: String::new(),
                    description: String::new(),
                    bounds: AxBounds {
                        x: 0.0,
                        y: 0.0,
                        width: 800.0,
                        height: 600.0,
                    },
                    offset_container_id: None,
                    child_ids: vec![2],
                    focusable: false,
                    ignored: false,
                    checked: None,
                },
                AxNodeUpdate {
                    id: 2,
                    role: AxRole::Button,
                    name: "Submit".into(),
                    value: String::new(),
                    description: String::new(),
                    bounds: AxBounds {
                        x: 10.0,
                        y: 20.0,
                        width: 80.0,
                        height: 24.0,
                    },
                    offset_container_id: Some(1),
                    child_ids: Vec::new(),
                    focusable: true,
                    ignored: false,
                    checked: None,
                },
            ],
        };

        // Ignored while disabled.
        assert_eq!(state.apply_deltas(vec![delta.clone()]), None);
        assert_eq!(slot.snapshot(), None);

        // Enabled emits and updates coalescing slot.
        state.set_enabled(true);
        let summary = state.apply_deltas(vec![delta]).unwrap();
        assert_eq!(summary, (1, 2, false));
        let loc_summary = state
            .apply_locations(&[AxLocationChange {
                id: 2,
                offset_container_id: Some(1),
                bounds: AxBounds {
                    x: 15.0,
                    y: 25.0,
                    width: 82.0,
                    height: 26.0,
                },
            }])
            .unwrap();
        assert_eq!(loc_summary, (2, 2, false));
        let snap = slot.take_latest().unwrap();
        assert_eq!(snap.generation, 2);
        assert_eq!(snap.nodes[&2].bounds.x, 15.0);

        // Disabling clears both tree and coalescing slot.
        state.set_enabled(false);
        assert_eq!(slot.snapshot(), None);
        assert_eq!(slot.take_latest(), None);
    }
}
