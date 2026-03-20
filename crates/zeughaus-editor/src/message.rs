use iced::Point;
use iced_nodegraph::PinRef;

pub type PinLabel = &'static str;

/// Messages use raw u64 IDs (matching the NodeGraph widget's ID type).
/// Conversion to/from zeughaus_core::NodeId happens in App::update().
#[derive(Debug, Clone)]
pub enum Message {
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
    SelectionChanged(Vec<u64>),
    DeleteNodes(Vec<u64>),
    CameraChanged {
        position: Point,
        zoom: f32,
    },
}
