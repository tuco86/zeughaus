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
    NodeMoved {
        node_id: u64,
        position: Point,
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
}
