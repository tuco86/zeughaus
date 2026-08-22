use std::collections::HashMap;
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use iced::{Point, Vector};
use iced_nodegraph::{PinId, PinRef};
use zeughaus_core::Value;

/// A pin name as it travels through the node graph widget.
///
/// Pin names are runtime data: a variadic node grows them while the graph is
/// edited and a loaded document brings them in as plain strings, so this wraps
/// a shared `Arc<str>` instead of a compile-time `&'static str`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PinLabel(pub Arc<str>);

impl PinLabel {
    /// The pin name as a plain string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl PinId for PinLabel {}

impl Deref for PinLabel {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for PinLabel {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<&str> for PinLabel {
    fn from(name: &str) -> Self {
        Self(Arc::from(name))
    }
}

impl From<Arc<str>> for PinLabel {
    fn from(name: Arc<str>) -> Self {
        Self(name)
    }
}

impl fmt::Display for PinLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    // Graph events (u64 IDs from iced_nodegraph)
    EdgeConnected {
        from: PinRef<u64, PinLabel>,
        to: PinRef<u64, PinLabel>,
    },
    EdgeDisconnected {
        from: PinRef<u64, PinLabel>,
        to: PinRef<u64, PinLabel>,
    },
    GroupMoved {
        node_ids: Vec<u64>,
        delta: Vector,
    },
    SelectionChanged(Vec<u64>),
    CloneNodes(Vec<u64>),
    DeleteNodes(Vec<u64>),
    CameraChanged {
        position: Point,
        zoom: f32,
    },
    // Command palette
    TogglePalette,
    PaletteInput(String),
    PaletteSelect(usize),
    PaletteConfirm,
    PaletteCancel,
    PaletteNavigate(usize),
    // Spawning
    SpawnNode { type_id: String },
    // Node parameter editing
    ConstValueChanged { node_id: u64, value: String },
    // In-node text settings (e.g. LLM base_url/model/prompt). `key` names the
    // setting, matching the node's SettingDef and set_parameter key.
    NodeSettingChanged { node_id: u64, key: String, value: String },
    // Periodic redraw tick while nodes are working, to animate node borders.
    Tick,
    // Drain queued remote sync events from the SpacetimeDB subscription and
    // apply them to the editor. Only active while connected.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    SyncPoll,
    // Copy the current collaboration session id to the clipboard (palette).
    CopySessionId,
    // A node's deferred async work finished. Carries the output pin values, or
    // an error message. Delivered back into the executor to resume downstream.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    AsyncNodeDone {
        node_id: u64,
        result: Result<HashMap<String, Value>, String>,
    },
    // File operations
    SaveGraph,
    LoadGraph,
    // Constructed by the native load dialog; on wasm, loading arrives via the
    // SpacetimeDB store (later phase).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    GraphLoaded(zeughaus_core::GraphDocument),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Save/load and SpacetimeDB sync round-trip pin names through `String`:
    /// the label must survive as the plain pin name in both directions. This is
    /// what replaced the old `leak_string`, which fabricated `&'static str` by
    /// leaking one allocation per loaded edge.
    #[test]
    fn label_round_trips_through_a_plain_string() {
        let label = PinLabel::from("model");
        let serialized = label.to_string();
        assert_eq!(serialized, "model");
        assert_eq!(PinLabel::from(serialized.as_str()), label);
    }

    /// A label built from a pin definition shares that allocation instead of
    /// copying the name on every view pass.
    #[test]
    fn label_shares_the_pin_definition_allocation() {
        let name: Arc<str> = Arc::from("out");
        let label = PinLabel::from(Arc::clone(&name));
        assert!(Arc::ptr_eq(&name, &label.0));
        assert_eq!(label.as_str(), "out");
    }

    /// Pin lookups compare a label against a `PinDefinition::name`, so equality
    /// must follow the name, not the allocation.
    #[test]
    fn equality_follows_the_name_not_the_allocation() {
        assert_eq!(PinLabel::from("a"), PinLabel::from("a"));
        assert_ne!(PinLabel::from("a"), PinLabel::from("b"));
        assert_eq!(&*PinLabel::from("a"), "a");
    }
}
