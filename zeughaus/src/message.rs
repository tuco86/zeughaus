use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use iced::{Point, Vector};
use iced_nodegraph::{Ids, PinRef};

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

/// The id vocabulary of the editor's graph widget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GraphIds;

impl Ids for GraphIds {
    type NodeId = u64;
    type PinId = PinLabel;
    type EdgeId = zeughaus_core::EdgeId;
    type AnchorId = u64;
    type Payload = crate::app::PinVisual;
}

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
    // Workspace chrome and pane layout. Graph messages remain flat below;
    // surfaces own their interaction vocabulary while the shell owns placement.
    Workspace(crate::workspace::Message),
    // Graph events (u64 IDs from iced_nodegraph)
    EdgeConnected {
        from: PinRef<GraphIds>,
        to: PinRef<GraphIds>,
    },
    EdgeDisconnected {
        from: PinRef<GraphIds>,
        to: PinRef<GraphIds>,
    },
    // A cable was released over a pin that is not an accepted target. The
    // widget reports the pair in drag order and gives no reason; the editor
    // re-runs its own rules to say which one turned it down.
    ConnectRefused {
        from: PinRef<GraphIds>,
        to: PinRef<GraphIds>,
    },
    // Show the contents of a container node, or the root graph for id 0. The
    // one navigation the editor has: a subgraph is drawn nowhere else.
    EnterGraph(u64),
    // Arrange the current graph's nodes in columns by depth. A layout is a
    // shared edit like any other move: it changes node positions.
    AutoLayout,
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
    SpawnNode {
        type_id: String,
    },
    // Node parameter editing
    ConstValueChanged {
        node_id: u64,
        value: String,
    },
    // In-node text settings (e.g. LLM base_url/model/prompt). `key` names the
    // setting, matching the node's SettingDef and set_parameter key.
    NodeSettingChanged {
        node_id: u64,
        key: String,
        value: String,
    },
    // A manual trigger node was pressed. Recorded in the shared store so the
    // one process that executes the graph fires the node once -- the
    // hand-driven counterpart to a timer, and it works from any window.
    NodeTriggered {
        node_id: u64,
    },
    // A node was resized by dragging its corner grip. The widget reports the
    // size the host should give the node's content; it does not own node size.
    NodeResized {
        node_id: u64,
        size: iced::Size,
    },
    // The window changed size. Kept because the palette places a new node in
    // the middle of what the user is looking at, and nothing else in the
    // editor knows how big that is.
    WindowResized {
        size: iced::Size,
    },
    // Periodic redraw tick while a node is in error, to animate its border. The
    // wasm editor has no timer subscription, so nothing emits it there.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    Tick,
    // Drain queued remote sync events from the SpacetimeDB subscription and
    // apply them to the editor. Only active while connected.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    SyncPoll,
    // Copy the current collaboration session id to the clipboard (palette).
    CopySessionId,
    // The window was asked to close. Handled rather than obeyed, because a
    // settings edit held back for the debounce would otherwise be lost from
    // the store: the editor flushes and then ends the runtime itself.
    CloseRequested,
    // The transport is closed; now the process may end.
    #[cfg(not(target_arch = "wasm32"))]
    Exit,
    // File operations
    SaveGraph,
    LoadGraph,
    // Constructed by the native load dialog; on wasm, loading arrives via the
    // SpacetimeDB store (later phase).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    GraphLoaded(zeughaus_core::GraphDocument),
    // A frame arrived on a Display node's feed. Native-only: the wasm editor
    // has no sync layer, so it never learns where frames come from and nothing
    // can emit this.
    #[cfg(not(target_arch = "wasm32"))]
    FeedFrame(crate::feed::Frame),
    // The runtime reported a value, a cleared pin or an edge it delivered
    // across, on the subscription of that epoch: a task that has been replaced
    // may still have messages queued, and its state must not be applied.
    // Native-only for the same reason as `FeedFrame`.
    #[cfg(not(target_arch = "wasm32"))]
    Traffic(u64, crate::feed::Traffic),
    // The runner's mux said something on the control exchange of that epoch.
    // Same guard as `Traffic`: a control task that has been replaced may
    // still have events queued, and a workspace from a runner this editor no
    // longer talks to must not replace the one on screen.
    #[cfg(not(target_arch = "wasm32"))]
    Mux(u64, crate::mux::MuxEvent),
    // One terminal's stream reported a head, a delta or its end.
    #[cfg(not(target_arch = "wasm32"))]
    Terminal(u64, zeughaus_mux::TerminalId, crate::mux::TerminalEvent),
    // A scrollback page the client scrolled to, or why it did not arrive.
    #[cfg(not(target_arch = "wasm32"))]
    RowPage(
        u64,
        zeughaus_mux::TerminalId,
        Result<zeughaus_mux::RowPage, String>,
    ),
    // A terminal pane reported what the user did in it. Carries the pane
    // rather than the terminal: focus is a property of the pane, and the
    // terminal it shows is one lookup away in the workspace.
    #[cfg(not(target_arch = "wasm32"))]
    TerminalAction(zeughaus_mux::PaneId, iced_terminal::Action),
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
