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
    /// Up (-1) or Down (+1) with no modifier: the palette's selection while
    /// it is open, else the pipeline cursor of a CI list in front. Emitted
    /// for every such key, since the subscription cannot see either.
    KeyStep(isize),
    /// A letter for the CI list in front: `r`, `l`, Shift+`c`. Emitted for
    /// every such key no widget took; the CI view decides whether it is in
    /// front.
    #[cfg(not(target_arch = "wasm32"))]
    CiShortcut(CiShortcut),
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
    // A manual trigger node was pressed. Sent to the runner so the
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
    // The editor's clock: notices a changed runner list, commits settings
    // that have gone quiet and ages the hint.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    SyncPoll,
    // The window was asked to close, by the system or by the close button
    // in the editor's own titlebar. Handled rather than obeyed, because a
    // settings edit held back for the debounce would otherwise be lost
    // before it reached the runner: the editor flushes and then ends the runtime itself.
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
    // Constructed by the restore path, which brings the local document back.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    GraphLoaded(zeughaus_core::GraphDocument),
    // A file the user picked to import: its nodes and edges get fresh ids and
    // join the graphs of the section in front. Constructed by the native
    // load dialog only.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    GraphImported(zeughaus_core::GraphDocument),
    // A runner's graph link reported its document, a change, a refusal or its
    // end, on the link of that epoch: a task that has been replaced may still
    // have events queued, and a document from a link this editor no longer
    // holds must not be applied.
    #[cfg(not(target_arch = "wasm32"))]
    Graph(
        crate::workspace::RunnerKey,
        u64,
        crate::graph_link::GraphEvent,
    ),
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
    /// The answer to an image fetch: the epoch of the runner link, the
    /// terminal and the image it asked for.
    #[cfg(not(target_arch = "wasm32"))]
    Resource(
        u64,
        zeughaus_mux::TerminalId,
        zeughaus_mux::ImageKey,
        Result<zeughaus_mux::Resource, String>,
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
    // Bring the tab that shows one of a runner's terminals to the front, or
    // attach it in a new tab when no tab shows it.
    #[cfg(not(target_arch = "wasm32"))]
    RevealTerminal(crate::workspace::RunnerKey, zeughaus_mux::TerminalId),
    // Hold a runner, which starts no new run and lets the live ones finish,
    // or release it again.
    HoldRunner(crate::workspace::RunnerKey, bool),
    // The notice center: the pull tab opens or closes it, a click outside or
    // Escape closes it, a notice's x or Clear all dismisses, a click on a
    // notice's title opens what it is about.
    ToggleNoticeCenter,
    CloseNoticeCenter,
    DismissNotice(u64),
    DismissAllNotices,
    OpenNotice(u64),
    // The center's animation frame or its clock, which expires transient
    // notices.
    NoticeTick,
    // What the runner answered: the state it is in now and how many runs are
    // still alive.
    #[cfg(not(target_arch = "wasm32"))]
    HoldReplied(Result<zeughaus_link::HoldReply, String>),
    // Open a runner's archived run (a closed terminal's output) read-only in
    // a tab of its own.
    #[cfg(not(target_arch = "wasm32"))]
    ReplayRun(crate::workspace::RunnerKey, u64),
    // What a runner's terminals are doing, as `/activity` answered.
    #[cfg(not(target_arch = "wasm32"))]
    Activity(
        crate::workspace::RunnerKey,
        Result<Vec<zeughaus_link::ActivityEntry>, String>,
    ),
    // What a CI answered one of this editor's CI views, or what the user did
    // in one. The key is the CI's link. Native only: the browser editor
    // reaches no runner.
    #[cfg(not(target_arch = "wasm32"))]
    Ci(crate::workspace::RunnerKey, CiMsg),
    // The clock that keeps every CI view current: asks each CI runner for
    // its overview. Emitted by a subscription that exists while a CI view
    // does; a subscription's map cannot capture a runner, so it names none.
    #[cfg(not(target_arch = "wasm32"))]
    CiPoll,
    // The CI page: the titlebar chip or the palette opens and closes it, the
    // sidebar shows a view, the arrow keys and j/k move its cursor, Enter
    // acts on the row under it, a click on a row acts on that row, a click
    // on a row of Sessions or Runners puts the cursor on it.
    #[cfg(not(target_arch = "wasm32"))]
    TogglePage,
    #[cfg(not(target_arch = "wasm32"))]
    PageShow(crate::app::PageView),
    #[cfg(not(target_arch = "wasm32"))]
    PageStep(i32),
    #[cfg(not(target_arch = "wasm32"))]
    PageEnter,
    #[cfg(not(target_arch = "wasm32"))]
    PageOpen(crate::app::AttentionTarget),
    #[cfg(not(target_arch = "wasm32"))]
    PageCursor(usize),
    // A runner of a CI's overview, whose activity the Overview shows.
    #[cfg(not(target_arch = "wasm32"))]
    PageMachine(crate::workspace::RunnerKey, usize),
    // A pipeline dot of a Board's rail: its channel with the cursor on it.
    #[cfg(not(target_arch = "wasm32"))]
    PagePipeline(crate::workspace::RunnerKey, u64, u64),
    // What a terminal the page docks reported the user did.
    #[cfg(not(target_arch = "wasm32"))]
    PageTerminal(
        crate::workspace::RunnerKey,
        zeughaus_mux::TerminalId,
        iced_terminal::Action,
    ),
    /// Ctrl+PageDown and Ctrl+PageUp: the next (`1`) or previous (`-1`) tab
    /// in the bar's order, the Projects section's included.
    CycleTab(isize),
    // The Projects section of the bar: a press on a tab the editor made
    // there (a Board, the button that starts a work shell, a channel view),
    // opening a project with a shell or only its Board, closing a project's
    // group, a channel badge of its header.
    #[cfg(not(target_arch = "wasm32"))]
    ProjectTab(zeughaus_mux::TabId),
    #[cfg(not(target_arch = "wasm32"))]
    OpenProject(String),
    #[cfg(not(target_arch = "wasm32"))]
    ShowProject(String),
    #[cfg(not(target_arch = "wasm32"))]
    DismissProject(String),
    #[cfg(not(target_arch = "wasm32"))]
    OpenChannelView(String, String),
    // The Overview's project list: the section's `+` focuses its search,
    // typing filters it, `Refresh` asks the forges again, and the local
    // runner's answer.
    #[cfg(not(target_arch = "wasm32"))]
    OpenProjectSearch,
    #[cfg(not(target_arch = "wasm32"))]
    ProjectSearch(String),
    #[cfg(not(target_arch = "wasm32"))]
    RefreshProjects,
    #[cfg(not(target_arch = "wasm32"))]
    ProjectsArrived(Result<zeughaus_link::ProjectsReply, String>),
    // A press on a card of a Board: its column and row.
    #[cfg(not(target_arch = "wasm32"))]
    BoardSelect(usize, usize),
    // What `zeughaus-runner claude login` said: the account line, or why not.
    #[cfg(not(target_arch = "wasm32"))]
    ClaudeSignedIn(Result<String, String>),
    // The Board of a project no CI builds, by its origin: its advice and
    // the onboarding request it files.
    #[cfg(not(target_arch = "wasm32"))]
    Onboard(String, OnboardMsg),
    // The ticket popout of a Board.
    #[cfg(not(target_arch = "wasm32"))]
    Ticket(TicketMsg),
}

/// What a Board of a project no CI builds says and is told.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
pub enum OnboardMsg {
    /// What the local runner found out about the project.
    Facts(Result<zeughaus_link::ProjectsReply, String>),
    /// The CI's advice on it.
    Advice(Result<Box<zeughaus_link::AdviceView>, String>),
    /// Ask the runner and the CI again.
    Refresh,
    /// Choose or drop a card, by its place.
    Card(usize),
    /// Switch a card's ticket between running through and waiting for
    /// review, by the card's place.
    Mode(usize),
    /// File the onboarding with the chosen cards' tickets.
    Onboard,
    /// The request's number and the tickets', or why not.
    Onboarded(Result<(u64, Vec<u64>), String>),
}

/// What a letter asks of the CI page in front.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiShortcut {
    /// `r`: rerun a pipeline, reject a plan.
    Rerun,
    Cancel,
    /// The log of the first failed job, else of the first job.
    OpenLog,
    /// `j` and `k`: the cursor down and up.
    Next,
    Previous,
    /// `a`: approve the selected plan.
    Approve,
    /// `p`: plan the selected proposal.
    Plan,
    /// `h` and Left, Right: a Board's column.
    Left,
    Right,
}

/// What a click on a notice opens.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub enum NoticeTarget {
    /// The tab that shows a runner's terminal, attached when none does.
    Terminal(crate::workspace::RunnerKey, zeughaus_mux::TerminalId),
    /// A view of the CI page.
    #[cfg(not(target_arch = "wasm32"))]
    CiPage(crate::app::PageView),
}

/// What one runner's CI views say.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
pub enum CiMsg {
    /// The channels and machines the runner reported.
    Overview(Result<zeughaus_link::CiOverview, String>),
    /// A channel's pipelines, newest first: the first page, or the next
    /// older one.
    Pipelines {
        view: u64,
        older: bool,
        result: Result<Vec<zeughaus_link::PipelineView>, String>,
    },
    /// One pipeline after a change; `None` when the runner no longer has it.
    /// Boxed: a pipeline is large, and a message is moved by value through
    /// every queue the editor has.
    Pipeline {
        repo: String,
        number: u64,
        result: Result<Option<Box<zeughaus_link::PipelineView>>, String>,
    },
    /// The user pressed a job: its transcript in a tab of its own.
    OpenJob { view: u64, number: u64, job: String },
    /// The terminal the runner replays a job's output in.
    Transcript {
        view: u64,
        number: u64,
        job: String,
        result: Result<u64, String>,
    },
    /// The job's tab was closed.
    CloseTranscript { view: u64, number: u64, job: String },
    /// The user asked for the pipelines older than those shown.
    LoadOlder { view: u64 },
    /// The user opened or closed a pipeline's row.
    Toggle { view: u64, number: u64 },
    /// The user asked the CI to run a pipeline's commit again.
    Rerun { view: u64, number: u64 },
    /// The user asked the CI to stop a running pipeline.
    Cancel { view: u64, number: u64 },
    /// What the CI answered a rerun or a cancel.
    Acted {
        view: u64,
        number: u64,
        cancel: bool,
        result: Result<(), String>,
    },
    /// A repository's tickets, texts empty, for its Board.
    Docs {
        view: u64,
        result: Result<Vec<zeughaus_link::DocView>, String>,
    },
    /// One document with its texts; `None` when the CI no longer has it.
    Shown {
        view: u64,
        number: u64,
        result: Result<Option<Box<zeughaus_link::DocView>>, String>,
    },
    /// The user asked the CI something its answer to is `Accepted`: a
    /// `DocState`, `DocDelete`, `DocRunsOn`, `DocEdit`, `DocAutoApprove`,
    /// `DocRefine`, `QueuePaused` or `CloseSession`.
    DocAct {
        view: u64,
        request: zeughaus_link::CiRequest,
    },
    /// What the CI answered it.
    DocActed {
        view: u64,
        result: Result<(), String>,
    },
    /// The user edits the `Runs on` of a document.
    RunsOnEdited {
        view: u64,
        number: u64,
        text: String,
    },
    /// The user asked the CI for a plan session of the Board's repository
    /// without a ticket; its plan becomes a new ticket.
    OpenSession { view: u64 },
    /// What the CI answered.
    SessionOpened(Result<zeughaus_link::SessionView, String>),
    /// The user pressed a link in a document.
    OpenLink(String),
}

/// Which part of a ticket its popout shows.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketTab {
    Proposal,
    Plan,
}

/// What the ticket popout over the workspace says; `ci` and `view` name the
/// CI and its Board the ticket belongs to.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
pub enum TicketMsg {
    Open {
        ci: crate::workspace::RunnerKey,
        view: u64,
        number: u64,
    },
    /// A ticket written in the popout before it exists.
    New {
        ci: crate::workspace::RunnerKey,
        view: u64,
    },
    Close,
    Tab(TicketTab),
    /// The text of a ticket that is not finished becomes editable.
    Edit,
    EditBody(iced::widget::text_editor::Action),
    EditSave,
    EditCancel,
    /// Pre-approves the ticket's plan, or takes that back.
    PreApprove(bool),
    /// The new ticket goes to the CI, and its answer.
    Create,
    Created(Result<u64, String>),
    /// The plan review: a section folded, deleted or restored, its note
    /// opened or typed in, the overall feedback, and Refine.
    Collapse(usize),
    Delete(usize),
    Annotate(usize),
    Note(usize, iced::widget::text_editor::Action),
    Overall(iced::widget::text_editor::Action),
    Refine,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Save/load and the runner's graph protocol round-trip pin names through `String`:
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
