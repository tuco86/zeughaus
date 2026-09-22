//! The workspace as this window shows it: the runner's tabs and split trees,
//! projected onto iced's pane grid.
//!
//! Two vocabularies meet here and must not be mixed up. A [`TabId`],
//! [`PaneId`] or [`SplitId`] is the runner's: it survives a reconnect, it is
//! the same number in every editor attached to that runner, and it is what a
//! [`TopologyCommand`] names. An iced `Pane` or `Split` is widget state,
//! minted afresh every time the grid is rebuilt from a snapshot and never
//! leaving this process. The maps in [`TabView`] are the whole translation,
//! and they are rebuilt with the grid so a stale one cannot exist.
//!
//! What is local stays local: which tab is active, which pane has focus and
//! where the tab bar sits are this window's business and travel nowhere.
//! Everything that changes the shared structure leaves as a
//! [`TopologyCommand`] and comes back as the next [`WorkspaceSnapshot`] -- the
//! editor never edits the tree it draws, so two editors cannot disagree about
//! it.
//!
//! With no runner attached the workspace is one tab showing the graph, and
//! every structural command is refused with a sentence rather than applied
//! locally: a split the runner never heard of would be gone at the next
//! snapshot, and a terminal needs a process to run in.

use std::collections::HashMap;
use std::time::Duration;

use iced::Color;
use iced::time::Instant;
use iced::widget::pane_grid::{self, Configuration, Node, Pane, ResizeEvent, Split};
use iced_tabs::Placement;
use zeughaus_mux::workspace::ProfileId;
use zeughaus_mux::{
    Axis, DetachedTerminal, PaneId, PaneNode, RunnerIncarnation, SplitId, TabId, TabSnapshot,
    TopologyCommand, WorkspaceSnapshot,
};

/// What a pane shows. The wire type itself: a mirrored copy would only add a
/// conversion that can disagree with the snapshot it came from.
pub use zeughaus_mux::SurfaceRef as Surface;

/// The pane title bar's word for a surface. A terminal's own title comes from
/// its [`zeughaus_mux::view::TerminalView`], which this module does not hold.
pub fn surface_title(surface: Surface) -> &'static str {
    match surface {
        Surface::Graph => "Graph",
        Surface::Empty => "Empty",
        Surface::Terminal(_) => "Terminal",
    }
}

/// Shortest gap between two [`TopologyCommand::ResizeSplit`] commands for one
/// drag. A resize is reported per cursor move, which is a command per frame at
/// 120 Hz for a structure change the runner broadcasts to every client; the
/// drag stays smooth locally either way, because the ratio is applied here
/// first and the runner's snapshot only confirms it.
const RESIZE_INTERVAL: Duration = Duration::from_millis(100);

/// The tab a detached editor shows. Fixed rather than counted: while no runner
/// is attached nothing can create a second tab or a second pane, and the first
/// snapshot replaces this wholesale.
const LOCAL_TAB: TabId = TabId(1);
const LOCAL_PANE: PaneId = PaneId(1);

/// One tab's pane grid, plus the translation between the runner's ids and
/// iced's.
#[derive(Debug)]
pub struct TabView {
    pub id: TabId,
    pub panes: pane_grid::State<Surface>,
    ids_by_pane: HashMap<Pane, PaneId>,
    ids_by_split: HashMap<Split, SplitId>,
}

impl TabView {
    /// Builds the grid for one tab and pairs the two trees.
    ///
    /// `with_configuration` mints its ids while it walks the configuration,
    /// so the resulting `Node` tree has exactly the shape of the `PaneNode`
    /// it was built from. Walking both together is therefore an exact
    /// pairing, and the only way to learn the `Split` ids at all: they are
    /// opaque and appear nowhere else.
    fn build(tab: &TabSnapshot) -> TabView {
        let panes = pane_grid::State::with_configuration(configuration(&tab.root));
        let mut ids_by_pane = HashMap::new();
        let mut ids_by_split = HashMap::new();
        pair(
            &tab.root,
            panes.layout(),
            &mut ids_by_pane,
            &mut ids_by_split,
        );
        TabView {
            id: tab.id,
            panes,
            ids_by_pane,
            ids_by_split,
        }
    }

    /// The runner's id for a pane the widget reported. The only direction
    /// anything needs: every id a message carries came out of the widget.
    pub fn pane_id(&self, pane: Pane) -> Option<PaneId> {
        self.ids_by_pane.get(&pane).copied()
    }
}

/// The pane grid configuration one shared tree means.
fn configuration(node: &PaneNode) -> Configuration<Surface> {
    match node {
        PaneNode::Split {
            axis,
            ratio,
            first,
            second,
            ..
        } => Configuration::Split {
            axis: iced_axis(*axis),
            ratio: *ratio,
            a: Box::new(configuration(first)),
            b: Box::new(configuration(second)),
        },
        PaneNode::Leaf { surface, .. } => Configuration::Pane(*surface),
    }
}

/// The two trees, walked together.
///
/// A shape mismatch is impossible -- the `Node` was just built from this
/// `PaneNode` -- and silently ignoring one keeps the function total, which
/// matters because it runs on a snapshot a peer sent.
fn pair(
    stable: &PaneNode,
    node: &Node,
    ids_by_pane: &mut HashMap<Pane, PaneId>,
    ids_by_split: &mut HashMap<Split, SplitId>,
) {
    match (stable, node) {
        (
            PaneNode::Split {
                id, first, second, ..
            },
            Node::Split {
                id: split, a, b, ..
            },
        ) => {
            ids_by_split.insert(*split, *id);
            pair(first, a, ids_by_pane, ids_by_split);
            pair(second, b, ids_by_pane, ids_by_split);
        }
        (PaneNode::Leaf { pane_id, .. }, Node::Pane(pane)) => {
            ids_by_pane.insert(*pane, *pane_id);
        }
        _ => {}
    }
}

/// The two axis vocabularies. The runner says which way the children sit,
/// iced names the divider: children side by side are separated by a vertical
/// line.
fn iced_axis(axis: Axis) -> pane_grid::Axis {
    match axis {
        Axis::Horizontal => pane_grid::Axis::Vertical,
        Axis::Vertical => pane_grid::Axis::Horizontal,
    }
}

/// What this window shows of the shared workspace.
#[derive(Debug)]
pub struct Workspace {
    snapshot: WorkspaceSnapshot,
    /// Whether `snapshot` came from a runner. While false it is the local
    /// graph-only default and no structural command may be sent.
    attached: bool,
    tabs: Vec<TabView>,
    active_tab: Option<TabId>,
    focused_pane: Option<PaneId>,
    pub placement: Placement,
    resize: Coalescer,
}

impl Workspace {
    /// The workspace of an editor with no runner: one tab, one graph pane.
    pub fn new() -> Self {
        let mut workspace = Workspace {
            snapshot: local_snapshot(),
            attached: false,
            tabs: Vec::new(),
            active_tab: None,
            focused_pane: None,
            placement: Placement::Top,
            resize: Coalescer::default(),
        };
        workspace.rebuild();
        workspace
    }

    /// Adopts the runner's workspace. The authoritative structure replaces
    /// whatever was shown; the active tab and focused pane are kept when the
    /// ids they name survived, and otherwise fall back to the graph.
    ///
    /// Nothing reaches the browser editor with a snapshot in hand: it has no
    /// sync layer and therefore never learns of a runner.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn apply_snapshot(&mut self, snapshot: WorkspaceSnapshot) {
        let unchanged = self.attached
            && self.snapshot.incarnation == snapshot.incarnation
            && self.snapshot.revision == snapshot.revision;
        if unchanged {
            return;
        }
        self.snapshot = snapshot;
        self.attached = true;
        self.rebuild();
    }

    /// Back to the local graph-only workspace: the runner this was attached to
    /// is gone for good, and its tabs name terminals that no longer exist.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn detach(&mut self) {
        if !self.attached {
            return;
        }
        self.snapshot = local_snapshot();
        self.attached = false;
        self.resize = Coalescer::default();
        self.rebuild();
    }

    pub fn attached(&self) -> bool {
        self.attached
    }

    pub fn tabs(&self) -> &[TabSnapshot] {
        &self.snapshot.tabs
    }

    /// Terminals the runner owns that no pane shows -- a job's, until
    /// somebody attaches it. Empty while no runner is attached.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn detached(&self) -> &[DetachedTerminal] {
        &self.snapshot.detached
    }

    /// The active tab's id, or the first tab's: a workspace always has one
    /// tab, and the tab bar needs an id to mark.
    pub fn active_tab(&self) -> TabId {
        self.active_tab
            .or_else(|| self.snapshot.tabs.first().map(|tab| tab.id))
            .unwrap_or(LOCAL_TAB)
    }

    pub fn active(&self) -> Option<&TabView> {
        let active = self.active_tab();
        self.tabs.iter().find(|tab| tab.id == active)
    }

    pub fn focused_pane(&self) -> Option<PaneId> {
        self.focused_pane
    }

    /// What a pane shows, in any tab.
    pub fn surface_of(&self, pane: PaneId) -> Option<Surface> {
        self.snapshot
            .tabs
            .iter()
            .find_map(|tab| match tab.root.find(pane) {
                Some(PaneNode::Leaf { surface, .. }) => Some(*surface),
                _ => None,
            })
    }

    /// Gives a pane the keyboard focus, if it exists.
    pub fn focus(&mut self, pane: PaneId) {
        if self.surface_of(pane).is_some() {
            self.focused_pane = Some(pane);
        }
    }

    /// Whether a drag has ratios that have not been sent yet, so the caller
    /// knows whether it needs a clock.
    ///
    /// The browser editor has no timer subscription, so it never asks.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn resize_pending(&self) -> bool {
        self.resize.pending()
    }

    pub fn update(&mut self, message: Message) -> Update {
        match message {
            Message::ActivateTab(id) => {
                if self.snapshot.tabs.iter().any(|tab| tab.id == id) {
                    self.active_tab = Some(id);
                    self.focused_pane = self.default_pane(id);
                }
                Update::none()
            }
            // Reported in iced's vocabulary because that is what the widget
            // knows; translated here, where the maps are, so no caller has to
            // hold both id spaces at once.
            Message::ActivatePane(pane) => {
                if let Some(id) = self.stable(pane) {
                    self.focus(id);
                }
                Update::none()
            }
            Message::TogglePlacement => {
                self.placement = match self.placement {
                    Placement::Top => Placement::Left,
                    Placement::Left => Placement::Top,
                };
                Update::none()
            }
            Message::NewTab => self.remote(TopologyCommand::NewTerminalTab {
                profile: ProfileId::DEFAULT,
            }),
            Message::Split { pane, axis } => match self.stable(pane) {
                None => Update::none(),
                Some(pane) => self.remote(TopologyCommand::SplitWithTerminal {
                    pane,
                    axis,
                    profile: ProfileId::DEFAULT,
                }),
            },
            Message::ClosePane(pane) => match self.stable(pane) {
                None => Update::none(),
                Some(pane) => self.remote(TopologyCommand::ClosePane { pane }),
            },
            Message::CloseTab(tab) => self.remote(TopologyCommand::CloseTab { tab }),
            Message::Resize(event) => self.resized(event),
            Message::FlushResize => Update {
                commands: self.resize.take_if_due(Instant::now()),
                hint: None,
            },
        }
    }

    /// The runner's id for a pane the widget reported, in the active tab.
    fn stable(&self, pane: Pane) -> Option<PaneId> {
        self.active()?.pane_id(pane)
    }

    /// A drag of a split divider: applied here at once so the pane follows
    /// the cursor, and coalesced on its way to the runner.
    fn resized(&mut self, event: ResizeEvent) -> Update {
        let split = {
            let Some(tab) = self.active_mut() else {
                return Update::none();
            };
            let split = tab.ids_by_split.get(&event.split).copied();
            tab.panes.resize(event.split, event.ratio);
            split
        };
        let Some(split) = split else {
            return Update::none();
        };
        if !self.attached {
            return Update::none();
        }
        self.resize.record(split, event.ratio);
        Update {
            commands: self.resize.take_if_due(Instant::now()),
            hint: None,
        }
    }

    /// A structural change: the runner's to make, or refused while there is
    /// no runner to make it.
    fn remote(&self, command: TopologyCommand) -> Update {
        if !self.attached {
            return Update {
                commands: Vec::new(),
                hint: Some("no runner: the shared workspace cannot be changed"),
            };
        }
        Update {
            commands: vec![command],
            hint: None,
        }
    }

    fn active_mut(&mut self) -> Option<&mut TabView> {
        let active = self.active_tab();
        self.tabs.iter_mut().find(|tab| tab.id == active)
    }

    /// Rebuilds every tab's grid from the snapshot and repairs the local
    /// selection.
    fn rebuild(&mut self) {
        self.tabs = self.snapshot.tabs.iter().map(TabView::build).collect();
        let active = self
            .active_tab
            .filter(|id| self.snapshot.tabs.iter().any(|tab| tab.id == *id))
            .or_else(|| self.graph_tab())
            .or_else(|| self.snapshot.tabs.first().map(|tab| tab.id));
        self.active_tab = active;
        let focus_survived = self
            .focused_pane
            .is_some_and(|pane| self.surface_of(pane).is_some());
        if !focus_survived {
            self.focused_pane = active.and_then(|id| self.default_pane(id));
        }
    }

    /// The tab holding the graph pane: where a lost selection lands, because
    /// it is the one surface that always exists.
    fn graph_tab(&self) -> Option<TabId> {
        self.snapshot
            .tabs
            .iter()
            .find(|tab| {
                tab.root
                    .leaves()
                    .iter()
                    .any(|(_, surface)| *surface == Surface::Graph)
            })
            .map(|tab| tab.id)
    }

    /// Which pane of a tab gets the focus when nothing else says: the graph
    /// if it is there, else the first leaf.
    fn default_pane(&self, tab: TabId) -> Option<PaneId> {
        let tab = self.snapshot.tabs.iter().find(|t| t.id == tab)?;
        let leaves = tab.root.leaves();
        leaves
            .iter()
            .find(|(_, surface)| *surface == Surface::Graph)
            .or_else(|| leaves.first())
            .map(|(pane, _)| *pane)
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Workspace::new()
    }
}

/// The workspace of an editor with no runner.
fn local_snapshot() -> WorkspaceSnapshot {
    WorkspaceSnapshot {
        // Not a runner's: nothing was attached, so no cache belongs to it.
        incarnation: RunnerIncarnation::from_bytes([0; 16]),
        revision: 0,
        tabs: vec![TabSnapshot {
            id: LOCAL_TAB,
            title: "Graph".to_owned(),
            group: None,
            accent_rgba: None,
            root: PaneNode::Leaf {
                pane_id: LOCAL_PANE,
                surface: Surface::Graph,
            },
        }],
        detached: Vec::new(),
    }
}

/// A tab's accent as a colour, or the theme's default.
pub fn accent(tab: &TabSnapshot) -> Option<Color> {
    tab.accent_rgba
        .map(|[r, g, b, a]| Color::from_rgba8(r, g, b, f32::from(a) / u8::MAX as f32))
}

/// The newest ratio per split, sent at most once per [`RESIZE_INTERVAL`].
///
/// A drag reports a ratio per cursor move and only the last one is worth
/// anything: the runner broadcasts each applied command to every client, so
/// forwarding the whole drag would multiply one gesture into a snapshot storm.
#[derive(Debug, Default)]
struct Coalescer {
    pending: HashMap<SplitId, f32>,
    sent: Option<Instant>,
}

impl Coalescer {
    fn record(&mut self, split: SplitId, ratio: f32) {
        self.pending.insert(split, ratio);
    }

    fn pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Everything held, if enough time has passed since the last batch.
    /// Ordered by split so two runs of one drag produce the same commands.
    fn take_if_due(&mut self, now: Instant) -> Vec<TopologyCommand> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        if self
            .sent
            .is_some_and(|last| now.duration_since(last) < RESIZE_INTERVAL)
        {
            return Vec::new();
        }
        self.sent = Some(now);
        let mut ratios: Vec<(SplitId, f32)> = self.pending.drain().collect();
        ratios.sort_by_key(|(split, _)| split.0);
        ratios
            .into_iter()
            .map(|(split, ratio)| TopologyCommand::ResizeSplit { split, ratio })
            .collect()
    }
}

/// What handling a workspace message produced: commands for the runner, and a
/// sentence for the status bar when there was no runner to take them.
#[derive(Debug, Default)]
pub struct Update {
    pub commands: Vec<TopologyCommand>,
    pub hint: Option<&'static str>,
}

impl Update {
    fn none() -> Update {
        Update::default()
    }
}

/// What the workspace chrome reports.
///
/// The first three are this window's own presentation and are applied here.
/// The rest change the shared structure: they become [`TopologyCommand`]s and
/// take effect when the runner's next snapshot says so.
///
/// A pane is named the way the widget named it -- an iced `Pane` -- because
/// that is all a click knows; the translation to the runner's [`PaneId`]
/// happens where the maps are.
#[derive(Debug, Clone)]
pub enum Message {
    ActivateTab(TabId),
    ActivatePane(Pane),
    TogglePlacement,
    NewTab,
    Split {
        pane: Pane,
        axis: Axis,
    },
    ClosePane(Pane),
    CloseTab(TabId),
    Resize(ResizeEvent),
    /// The coalescing clock: sends whatever a drag left behind. Emitted by
    /// a timer subscription the browser editor does not have.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    FlushResize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeughaus_mux::{SurfaceRef, TerminalId};

    /// The reverse of [`TabView::pane_id`]. Only tests need it: every id a
    /// message carries came out of the widget, so the editor never looks a
    /// pane up by the runner's id.
    fn iced_pane(tab: &TabView, id: PaneId) -> Option<Pane> {
        tab.ids_by_pane
            .iter()
            .find(|(_, stable)| **stable == id)
            .map(|(pane, _)| *pane)
    }

    fn leaf(pane: u64, surface: SurfaceRef) -> PaneNode {
        PaneNode::Leaf {
            pane_id: PaneId(pane),
            surface,
        }
    }

    fn split(id: u64, axis: Axis, ratio: f32, first: PaneNode, second: PaneNode) -> PaneNode {
        PaneNode::Split {
            id: SplitId(id),
            axis,
            ratio,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    fn snapshot(root: PaneNode) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            incarnation: RunnerIncarnation::from_bytes([7; 16]),
            revision: 1,
            tabs: vec![TabSnapshot {
                id: TabId(9),
                title: "Shell".to_owned(),
                group: None,
                accent_rgba: None,
                root,
            }],
            detached: Vec::new(),
        }
    }

    /// The tree an iced grid means, read back through the maps the projection
    /// kept. Test-only: the editor never needs it, but it is what proves the
    /// projection lost nothing.
    fn recover(node: &Node, view: &TabView) -> PaneNode {
        match node {
            Node::Split {
                id,
                axis,
                ratio,
                a,
                b,
                ..
            } => PaneNode::Split {
                id: view.ids_by_split[id],
                axis: match axis {
                    pane_grid::Axis::Vertical => Axis::Horizontal,
                    pane_grid::Axis::Horizontal => Axis::Vertical,
                },
                ratio: *ratio,
                first: Box::new(recover(a, view)),
                second: Box::new(recover(b, view)),
            },
            Node::Pane(pane) => PaneNode::Leaf {
                pane_id: view.ids_by_pane[pane],
                surface: *view.panes.get(*pane).expect("a built pane has a surface"),
            },
        }
    }

    #[test]
    fn a_pane_tree_survives_the_trip_through_iced() {
        let root = split(
            1,
            Axis::Horizontal,
            0.4,
            leaf(10, SurfaceRef::Graph),
            split(
                2,
                Axis::Vertical,
                0.75,
                leaf(11, SurfaceRef::Terminal(TerminalId(3))),
                leaf(12, SurfaceRef::Empty),
            ),
        );
        let mut workspace = Workspace::new();
        workspace.apply_snapshot(snapshot(root.clone()));

        let tab = workspace.active().expect("the snapshot has one tab");
        assert_eq!(recover(tab.panes.layout(), tab), root);
    }

    #[test]
    fn every_pane_id_maps_both_ways() {
        let root = split(
            1,
            Axis::Vertical,
            0.5,
            leaf(10, SurfaceRef::Graph),
            leaf(11, SurfaceRef::Terminal(TerminalId(1))),
        );
        let mut workspace = Workspace::new();
        workspace.apply_snapshot(snapshot(root));

        let tab = workspace.active().expect("the snapshot has one tab");
        for id in [PaneId(10), PaneId(11)] {
            let pane = iced_pane(tab, id).expect("a leaf has an iced pane");
            assert_eq!(tab.pane_id(pane), Some(id));
        }
    }

    #[test]
    fn a_detached_workspace_shows_the_graph_and_refuses_structure() {
        let mut workspace = Workspace::new();

        assert!(!workspace.attached());
        assert_eq!(workspace.tabs().len(), 1);
        assert_eq!(workspace.surface_of(LOCAL_PANE), Some(Surface::Graph));

        let update = workspace.update(Message::NewTab);
        assert!(update.commands.is_empty());
        assert!(update.hint.is_some());
    }

    #[test]
    fn a_lost_runner_returns_the_graph_only_workspace() {
        let mut workspace = Workspace::new();
        workspace.apply_snapshot(snapshot(leaf(10, SurfaceRef::Graph)));
        assert!(workspace.attached());

        workspace.detach();

        assert!(!workspace.attached());
        assert_eq!(workspace.surface_of(LOCAL_PANE), Some(Surface::Graph));
        assert_eq!(workspace.focused_pane(), Some(LOCAL_PANE));
    }

    #[test]
    fn focus_falls_back_to_the_graph_when_its_pane_is_gone() {
        let mut workspace = Workspace::new();
        workspace.apply_snapshot(snapshot(split(
            1,
            Axis::Horizontal,
            0.5,
            leaf(10, SurfaceRef::Graph),
            leaf(11, SurfaceRef::Terminal(TerminalId(1))),
        )));
        workspace.focus(PaneId(11));
        assert_eq!(workspace.focused_pane(), Some(PaneId(11)));

        let mut next = snapshot(leaf(10, SurfaceRef::Graph));
        next.revision = 2;
        workspace.apply_snapshot(next);

        assert_eq!(workspace.focused_pane(), Some(PaneId(10)));
    }

    #[test]
    fn a_structural_command_needs_a_runner() {
        let mut workspace = Workspace::new();
        workspace.apply_snapshot(snapshot(leaf(10, SurfaceRef::Graph)));

        let pane = iced_pane(
            workspace.active().expect("the snapshot has one tab"),
            PaneId(10),
        )
        .expect("the graph leaf has an iced pane");
        let update = workspace.update(Message::Split {
            pane,
            axis: Axis::Horizontal,
        });

        assert_eq!(
            update.commands,
            vec![TopologyCommand::SplitWithTerminal {
                pane: PaneId(10),
                axis: Axis::Horizontal,
                profile: ProfileId::DEFAULT,
            }]
        );
        assert!(update.hint.is_none());
    }

    #[test]
    fn a_drag_sends_one_command_per_interval_with_the_newest_ratio() {
        let mut coalescer = Coalescer::default();
        let start = Instant::now();

        coalescer.record(SplitId(1), 0.3);
        assert_eq!(
            coalescer.take_if_due(start),
            vec![TopologyCommand::ResizeSplit {
                split: SplitId(1),
                ratio: 0.3
            }]
        );

        // Mid-drag: every ratio is recorded, none is sent.
        for step in 1u8..8 {
            coalescer.record(SplitId(1), 0.3 + 0.01 * f32::from(step));
            assert!(
                coalescer
                    .take_if_due(start + Duration::from_millis(u64::from(step) * 10))
                    .is_empty()
            );
        }
        assert!(coalescer.pending());

        assert_eq!(
            coalescer.take_if_due(start + RESIZE_INTERVAL),
            vec![TopologyCommand::ResizeSplit {
                split: SplitId(1),
                ratio: 0.37
            }]
        );
        assert!(!coalescer.pending());
    }

    #[test]
    fn a_drag_of_two_splits_keeps_both_ratios() {
        let mut coalescer = Coalescer::default();
        let start = Instant::now();
        coalescer.record(SplitId(2), 0.6);
        coalescer.record(SplitId(1), 0.2);
        coalescer.record(SplitId(1), 0.25);

        assert_eq!(
            coalescer.take_if_due(start),
            vec![
                TopologyCommand::ResizeSplit {
                    split: SplitId(1),
                    ratio: 0.25
                },
                TopologyCommand::ResizeSplit {
                    split: SplitId(2),
                    ratio: 0.6
                },
            ]
        );
    }
}
