use std::collections::HashMap;

use iced::{Point, Vector};
use iced_nodegraph::PinRef;
use zeughaus_core::Value;

pub type PinLabel = &'static str;

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
