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
    // Open a container node as a graph tab, or bring its tab to the front.
    // The one navigation the editor has: a subgraph is drawn nowhere else.
    OpenGraph(u64),
    // A new top-level graph executed by this section's runner (or local).
    NewGraph(crate::workspace::RunnerKey),
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
    // One graph pane's camera moved. Every pane showing that graph shares it.
    CameraChanged {
        graph: u64,
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
    // Draw with this theme, by name: a pack entry or a file the user dropped
    // into the state directory. The name travels rather than the theme
    // itself, for the same reason `SpawnNode` carries a type id -- a palette
    // command is data, and the window owns the list it is resolved against.
    SetTheme(String),
    // In-node settings: one message per edited field, whatever the node does
    // with the text. `key` names the setting, matching the node's SettingDef
    // and set_parameter key.
    NodeSettingChanged {
        node_id: u64,
        key: String,
        value: String,
    },
    // Renaming a node from its header: the edit button starts it with the
    // current name, the header's text field edits the draft, Enter or the edit
    // button again commits it.
    RenameStart(u64),
    RenameInput(String),
    RenameCommit,
    // Escape anywhere: closes the palette and abandons a rename.
    Escape,
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
    // The window opened: its size, and the id to ask for its scale factor.
    WindowOpened {
        id: iced::window::Id,
        size: iced::Size,
    },
    // Device pixels per logical pixel of the window. Terminal panes count
    // their grid with cells rounded to whole device pixels at this scale.
    WindowRescaled(f32),
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
    // The window was asked to close, by the system or by the close button
    // in the editor's own titlebar. Handled rather than obeyed, because a
    // settings edit held back for the debounce would otherwise be lost from
    // the store: the editor flushes and then ends the runtime itself.
    CloseRequested,
    // The undecorated window's own titlebar and edge grips: the moves a
    // system titlebar would have made. The grips and the minimize button
    // exist on native windows only; a browser tab has neither.
    //
    // A press on the titlebar only arms a drag; the drag starts once the
    // pointer moves away with the button held. Starting it on the press
    // would put the second click of a double-click inside a compositor move,
    // and a maximize that arrives during a move keeps the moved position.
    TitlebarPress,
    TitlebarMove(iced::Point),
    TitlebarRelease,
    TitlebarExit,
    #[cfg(not(target_arch = "wasm32"))]
    WindowResize(iced::window::Direction),
    #[cfg(not(target_arch = "wasm32"))]
    WindowMinimize,
    WindowMaximize,
    // Read from the window manager after opening or resizing, including
    // maximize/restore actions performed outside our titlebar.
    #[cfg(not(target_arch = "wasm32"))]
    WindowMaximized(bool),
    // The window gained or lost the keyboard focus. A terminal's
    // notification is shown on the desktop unless its pane is what the user
    // is looking at, and that needs both.
    #[cfg(not(target_arch = "wasm32"))]
    WindowFocused(bool),
    // The transport is closed; now the process may end.
    #[cfg(not(target_arch = "wasm32"))]
    Exit,
    // SIGUSR1: save what the window shows and replace the process with a
    // fresh build of itself.
    #[cfg(unix)]
    Restart,
    // The transport is closed and the restore file written; `exec` now.
    #[cfg(unix)]
    RestartExec(std::path::PathBuf),
    // File operations
    SaveGraph,
    LoadGraph,
    // Constructed by the native load dialog. The browser editor has no file
    // dialog, so nothing emits it there.
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
    TerminalAction(crate::workspace::PaneRef, iced_terminal::Action),
    // Show one of a runner's own terminals -- a job's -- in a new tab, or
    // kill it. Offered by the palette from the detached list of that
    // runner's last workspace snapshot; the runner answers with the next
    // snapshot. The browser editor never has a runner, so nothing emits
    // these there.
    AttachTerminal(crate::workspace::RunnerKey, zeughaus_mux::TerminalId),
    CloseTerminal(crate::workspace::RunnerKey, zeughaus_mux::TerminalId),
    // Hold a runner, which starts no new run and lets the live ones finish,
    // or release it again.
    HoldRunner(crate::workspace::RunnerKey, bool),
    // What the runner answered: the state it is in now and how many runs are
    // still alive.
    #[cfg(not(target_arch = "wasm32"))]
    HoldReplied(Result<zeughaus_link::HoldReply, String>),
    // Move a CI runner's busy mode to the next one: auto, busy, free. The
    // toggle next to its section header.
    CycleBusy(crate::workspace::RunnerKey),
    // What the runner answered: the machine's state after the change.
    #[cfg(not(target_arch = "wasm32"))]
    BusyReplied(
        crate::workspace::RunnerKey,
        Result<zeughaus_link::MachineState, String>,
    ),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Save/load and SpacetimeDB sync round-trip pin names through `String`:
    /// the label must survive as the plain pin name in both directions.
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
