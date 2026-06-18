use iced::{Point, Vector};
use iced_nodegraph::PinRef;

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
    // File operations
    SaveGraph,
    LoadGraph,
    // Constructed by the native load dialog; on wasm, loading arrives via the
    // SpacetimeDB store (later phase).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    GraphLoaded(zeughaus_core::GraphDocument),
}
