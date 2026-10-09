//! The workspace as this window shows it: one section per runner, each the
//! runner's tabs, groups and split trees, projected onto iced's pane grid.
//!
//! Two vocabularies meet here and must not be mixed up. A [`TabId`],
//! [`PaneId`], [`SplitId`] or [`GroupId`] is a runner's: it survives a
//! reconnect, it is the same number in every editor attached to that runner,
//! and it is what a [`TopologyCommand`] names. It is unique within one runner
//! only, which is why every reference that leaves a section carries its
//! [`RunnerKey`]. An iced `Pane` or `Split` is widget state, minted afresh
//! every time a grid is rebuilt from a snapshot and never leaving this
//! process. The maps in [`TabView`] are the whole translation, and they are
//! rebuilt with the grid so a stale one cannot exist.
//!
//! What is local stays local: which tab is active, which pane has focus,
//! which sections and groups are collapsed, where the tab bar sits and what
//! is being dragged are this window's business and travel nowhere.
//! Everything that changes the shared structure leaves as a
//! [`TopologyCommand`] to the section's runner and comes back as its next
//! [`WorkspaceSnapshot`] -- the editor never edits the tree it draws, so two
//! editors cannot disagree about it. Tabs never move between sections: each
//! runner owns its own terminals and graphs.
//!
//! Two sections are not a runner's: `local` (no runner; every graph of the
//! scratch document) and `offline` (graphs whose runner is not connected).
//! The app builds their snapshots from the document; they take no drops and
//! send no commands.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use iced::time::Instant;
use iced::widget::pane_grid::{self, Configuration, Node, Pane, ResizeEvent, Split};
use iced::{Color, Point};
use iced_tabs::{Placement, Target};
use zeughaus_core::NodeId;
use zeughaus_mux::workspace::ProfileId;
use zeughaus_mux::{
    Axis, DetachedTerminal, GroupId, GroupSnapshot, PaneId, PaneNode, PaneTarget,
    RunnerIncarnation, Side, SplitId, TabId, TabSlot, TabSnapshot, TopologyCommand, WorkspaceItem,
    WorkspaceSnapshot,
};

/// What a pane shows. The wire type itself: a mirrored copy would only add a
/// conversion that can disagree with the snapshot it came from.
pub use zeughaus_mux::SurfaceRef as Surface;

/// Shortest gap between two [`TopologyCommand::ResizeSplit`] commands for one
/// drag. A resize is reported per cursor move, which is a command per frame at
/// 120 Hz for a structure change the runner broadcasts to every client; the
/// drag stays smooth locally either way, because the ratio is applied here
/// first and the runner's snapshot only confirms it.
const RESIZE_INTERVAL: Duration = Duration::from_millis(100);

/// How far the pointer moves with the button held before a press on a tab,
/// a group header or a pane grip becomes a drag. A release before that is a
/// click.
const DRAG_THRESHOLD: f32 = 4.0;

/// The colours a new group cycles through: distinct at 25 % alpha on dark and
/// light chrome alike.
pub const GROUP_COLORS: [[u8; 4]; 8] = [
    [0x4c, 0x8b, 0xf5, 0xff],
    [0x43, 0xa0, 0x47, 0xff],
    [0xef, 0x8f, 0x2d, 0xff],
    [0xab, 0x47, 0xbc, 0xff],
    [0xe5, 0x39, 0x35, 0xff],
    [0x00, 0xac, 0xc1, 0xff],
    [0xfd, 0xd8, 0x35, 0xff],
    [0x8d, 0x6e, 0x63, 0xff],
];

/// Which section something belongs to: a runner's fingerprint text, or one
/// of the two synthetic sections ([`RunnerKey::LOCAL`], [`RunnerKey::OFFLINE`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunnerKey(pub Arc<str>);

impl RunnerKey {
    /// The section of an editor without a runner: every graph it holds.
    pub const LOCAL: &'static str = "local";
    /// The section of graphs whose runner is not connected.
    pub const OFFLINE: &'static str = "offline";

    pub fn new(text: &str) -> RunnerKey {
        RunnerKey(Arc::from(text))
    }

    pub fn local() -> RunnerKey {
        RunnerKey::new(Self::LOCAL)
    }

    pub fn offline() -> RunnerKey {
        RunnerKey::new(Self::OFFLINE)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is one of the two sections no runner stands behind.
    pub fn is_synthetic(&self) -> bool {
        matches!(self.as_str(), Self::LOCAL | Self::OFFLINE)
    }
}

/// A tab, named across sections.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TabRef {
    pub runner: RunnerKey,
    pub tab: TabId,
}

/// A pane, named across sections.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PaneRef {
    pub runner: RunnerKey,
    pub pane: PaneId,
}

/// Something this window shows collapsed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CollapseKey {
    Section(RunnerKey),
    Group(RunnerKey, GroupId),
}

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

    /// How many panes the tab shows.
    pub fn pane_count(&self) -> usize {
        self.ids_by_pane.len()
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

/// One section of the tab bar: a runner's workspace, or a synthetic one.
#[derive(Debug)]
pub struct Section {
    pub key: RunnerKey,
    pub label: String,
    pub snapshot: WorkspaceSnapshot,
    /// Whether `snapshot` came from the runner's mux. While false a runner
    /// section shows what it last had (or nothing) and takes no commands.
    pub attached: bool,
    tabs: Vec<TabView>,
}

impl Section {
    fn new(key: RunnerKey, label: String) -> Section {
        Section {
            key,
            label,
            snapshot: empty_snapshot(),
            attached: false,
            tabs: Vec::new(),
        }
    }

    fn rebuild(&mut self) {
        self.tabs = self.snapshot.tabs().map(TabView::build).collect();
    }

    /// Whether this section is one no runner stands behind.
    pub fn synthetic(&self) -> bool {
        self.key.is_synthetic()
    }

    pub fn tab_view(&self, tab: TabId) -> Option<&TabView> {
        self.tabs.iter().find(|view| view.id == tab)
    }

    fn tab_view_mut(&mut self, tab: TabId) -> Option<&mut TabView> {
        self.tabs.iter_mut().find(|view| view.id == tab)
    }

    fn tab(&self, tab: TabId) -> Option<&TabSnapshot> {
        self.snapshot.tabs().find(|t| t.id == tab)
    }

    /// What a pane shows, in any tab of this section.
    pub fn surface_of(&self, pane: PaneId) -> Option<Surface> {
        self.snapshot
            .tabs()
            .find_map(|tab| match tab.root.find(pane) {
                Some(PaneNode::Leaf { surface, .. }) => Some(*surface),
                _ => None,
            })
    }

    /// Which pane of a tab gets the focus when nothing else says: a graph if
    /// one is there, else the first leaf.
    fn default_pane(&self, tab: TabId) -> Option<PaneId> {
        let leaves = self.tab(tab)?.root.leaves();
        leaves
            .iter()
            .find(|(_, surface)| matches!(surface, Surface::Graph(_)))
            .or_else(|| leaves.first())
            .map(|(pane, _)| *pane)
    }
}

/// A snapshot with nothing in it: a runner section before its mux attached.
fn empty_snapshot() -> WorkspaceSnapshot {
    WorkspaceSnapshot {
        // Not a runner's: nothing was attached, so no cache belongs to it.
        incarnation: RunnerIncarnation::from_bytes([0; 16]),
        revision: 0,
        items: Vec::new(),
        detached: Vec::new(),
    }
}

/// A synthetic section's snapshot: one tab per graph, the tab and its pane
/// numbered by the graph's node id, which no runner id space shares because
/// no runner stands behind the section.
pub fn graph_snapshot(graphs: impl IntoIterator<Item = (NodeId, String)>) -> WorkspaceSnapshot {
    WorkspaceSnapshot {
        items: graphs
            .into_iter()
            .map(|(graph, title)| {
                WorkspaceItem::Tab(TabSnapshot {
                    id: TabId(graph.0),
                    title,
                    accent_rgba: None,
                    root: PaneNode::Leaf {
                        pane_id: PaneId(graph.0),
                        surface: Surface::Graph(graph.0),
                    },
                })
            })
            .collect(),
        ..empty_snapshot()
    }
}

/// What is being dragged.
#[derive(Debug, Clone, PartialEq)]
pub enum DragSource {
    Tab(TabRef),
    Group(RunnerKey, GroupId),
    Pane(PaneRef),
}

impl DragSource {
    pub fn runner(&self) -> &RunnerKey {
        match self {
            DragSource::Tab(tab) => &tab.runner,
            DragSource::Group(runner, _) => runner,
            DragSource::Pane(pane) => &pane.runner,
        }
    }
}

/// Where a drag would land.
///
/// A slot's index is the position in the list as it is drawn, the dragged
/// tab still in it: the command adjusts it, because the runner counts after
/// taking the tab out.
#[derive(Debug, Clone, PartialEq)]
pub enum DropTarget {
    Slot(RunnerKey, TabSlot),
    /// The end of a group, collapsed or not.
    IntoGroup(RunnerKey, GroupId),
    /// Beside a pane, on one side.
    Pane(PaneRef, Side),
}

impl DropTarget {
    pub fn runner(&self) -> &RunnerKey {
        match self {
            DropTarget::Slot(runner, _) | DropTarget::IntoGroup(runner, _) => runner,
            DropTarget::Pane(pane, _) => &pane.runner,
        }
    }
}

/// A press on a tab, a group header or a pane grip, and what it became.
#[derive(Debug, Clone)]
pub struct Drag {
    pub source: DragSource,
    /// Where the pointer was first seen after the press. The press itself
    /// carries no position, so the first move is the reference.
    origin: Option<Point>,
    /// The pointer, in window coordinates.
    pub cursor: Point,
    /// Whether the pointer moved far enough to make this a drag.
    pub active: bool,
    /// Where a release would drop, if anywhere is allowed.
    pub target: Option<DropTarget>,
    /// The tab bar's insertion marker for `target`.
    pub marker: Option<Target<GroupId, RunnerKey>>,
}

/// What this window shows of every runner's workspace.
#[derive(Debug)]
pub struct Workspace {
    sections: Vec<Section>,
    active: Option<TabRef>,
    focused: Option<PaneRef>,
    pub collapsed: HashSet<CollapseKey>,
    pub placement: Placement,
    resize: Coalescer,
    pub drag: Option<Drag>,
    /// A group whose name is being edited: the section, the group and the
    /// draft.
    pub renaming_group: Option<(RunnerKey, GroupId, String)>,
    /// A tab whose name is being edited, and the draft.
    pub renaming_tab: Option<(TabRef, String)>,
    /// A tab this window asked a runner for: brought to the front when a
    /// snapshot of that runner shows a tab id not in the set.
    expected_tab: Option<(RunnerKey, HashSet<TabId>)>,
}

impl Workspace {
    pub fn new() -> Self {
        Workspace {
            sections: Vec::new(),
            active: None,
            focused: None,
            collapsed: HashSet::new(),
            placement: Placement::Top,
            resize: Coalescer::default(),
            drag: None,
            renaming_group: None,
            renaming_tab: None,
            expected_tab: None,
        }
    }

    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    pub fn section(&self, key: &RunnerKey) -> Option<&Section> {
        self.sections.iter().find(|s| &s.key == key)
    }

    fn section_mut(&mut self, key: &RunnerKey) -> Option<&mut Section> {
        self.sections.iter_mut().find(|s| &s.key == key)
    }

    /// Makes sure a runner has its section, labelled `label`.
    ///
    /// Runner sections are ordered by label (host, then fingerprint) and
    /// come before the synthetic ones: an order that no reconnect, restart
    /// or second editor changes, which the order of connecting would.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn ensure_runner(&mut self, key: RunnerKey, label: String) {
        match self.section_mut(&key) {
            Some(section) if section.label == label => return,
            Some(section) => section.label = label,
            None => self.sections.push(Section::new(key, label)),
        }
        self.sections.sort_by(|a, b| {
            (a.synthetic(), &a.label, &a.key).cmp(&(b.synthetic(), &b.label, &b.key))
        });
    }

    /// Drops a section: its runner is gone.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn remove_section(&mut self, key: &RunnerKey) {
        self.sections.retain(|s| &s.key != key);
        self.resize.forget(key);
        self.forget_expected(key);
        self.repair();
    }

    /// Adopts a runner's workspace. The authoritative structure replaces
    /// whatever was shown; the active tab and focused pane are kept when the
    /// ids they name survived.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn apply_snapshot(&mut self, key: &RunnerKey, snapshot: WorkspaceSnapshot) {
        let Some(section) = self.section_mut(key) else {
            return;
        };
        let unchanged = section.attached
            && section.snapshot.incarnation == snapshot.incarnation
            && section.snapshot.revision == snapshot.revision;
        if unchanged {
            return;
        }
        section.snapshot = snapshot;
        section.attached = true;
        section.rebuild();
        let arrived = match &self.expected_tab {
            Some((expected, known)) if expected == key => self.section(key).and_then(|s| {
                s.snapshot
                    .tabs()
                    .map(|t| t.id)
                    .find(|id| !known.contains(id))
            }),
            _ => None,
        };
        if let Some(tab) = arrived {
            self.expected_tab = None;
            self.activate(TabRef {
                runner: key.clone(),
                tab,
            });
        }
        self.repair();
    }

    /// Forgets a runner's workspace: its mux is gone for good, and its tabs
    /// name terminals that no longer exist.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn detach(&mut self, key: &RunnerKey) {
        let Some(section) = self.section_mut(key) else {
            return;
        };
        if !section.attached && section.snapshot.items.is_empty() {
            return;
        }
        section.snapshot = empty_snapshot();
        section.attached = false;
        section.rebuild();
        self.resize.forget(key);
        self.forget_expected(key);
        self.repair();
    }

    /// Sets a synthetic section's content, or removes it with `None`.
    /// Rebuilt only when the content changed: this runs after every update.
    pub fn set_synthetic(
        &mut self,
        key: RunnerKey,
        label: &str,
        snapshot: Option<WorkspaceSnapshot>,
    ) {
        let Some(snapshot) = snapshot else {
            if self.section(&key).is_some() {
                self.remove_section(&key);
            }
            return;
        };
        if self.section(&key).is_none() {
            self.sections
                .push(Section::new(key.clone(), label.to_owned()));
        }
        let Some(section) = self.section_mut(&key) else {
            return;
        };
        if section.label != label {
            label.clone_into(&mut section.label);
        }
        if section.snapshot.items == snapshot.items {
            return;
        }
        section.snapshot = snapshot;
        section.rebuild();
        self.repair();
    }

    /// Whether `key`'s runner takes structural commands.
    pub fn attached(&self, key: &RunnerKey) -> bool {
        self.section(key).is_some_and(|s| s.attached)
    }

    /// Remembers the tabs `key`'s runner shows now, so the first tab its
    /// next snapshots add comes to the front: the answer to a request from
    /// this window.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn expect_new_tab(&mut self, key: &RunnerKey) {
        let known = self
            .section(key)
            .map(|s| s.snapshot.tabs().map(|t| t.id).collect())
            .unwrap_or_default();
        self.expected_tab = Some((key.clone(), known));
    }

    /// Stops waiting for a tab from `key`'s runner: it refused the request
    /// or went away.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn forget_expected(&mut self, key: &RunnerKey) {
        if self.expected_tab.as_ref().is_some_and(|(k, _)| k == key) {
            self.expected_tab = None;
        }
    }

    /// Whether a runner's workspace shows `graph` in any pane.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn runner_shows(&self, graph: NodeId) -> bool {
        self.sections
            .iter()
            .filter(|s| !s.synthetic())
            .flat_map(|s| s.snapshot.tabs())
            .any(|tab| {
                tab.root
                    .leaves()
                    .iter()
                    .any(|(_, surface)| *surface == Surface::Graph(graph.0))
            })
    }

    /// The graph a tab shows when it is nothing but that graph.
    pub fn single_graph(&self, tab: &TabRef) -> Option<NodeId> {
        match &self.section(&tab.runner)?.tab(tab.tab)?.root {
            PaneNode::Leaf {
                surface: Surface::Graph(graph),
                ..
            } => Some(NodeId(*graph)),
            _ => None,
        }
    }

    /// Terminals `key`'s runner owns that no pane shows.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn detached(&self, key: &RunnerKey) -> &[DetachedTerminal] {
        self.section(key).map_or(&[], |s| &s.snapshot.detached)
    }

    /// The tab the bar marks, if there is any tab at all.
    pub fn active_tab(&self) -> Option<&TabRef> {
        self.active.as_ref()
    }

    /// The tab in front with its grid and its section.
    pub fn active(&self) -> Option<(&Section, &TabView)> {
        let active = self.active.as_ref()?;
        let section = self.section(&active.runner)?;
        Some((section, section.tab_view(active.tab)?))
    }

    /// Only a terminal pane asks, and the browser editor draws none.
    pub fn focused_pane(&self) -> Option<&PaneRef> {
        self.focused.as_ref()
    }

    /// The graph the focused pane shows, if it shows one.
    pub fn focused_graph(&self) -> Option<NodeId> {
        match self.surface_of(self.focused.as_ref()?)? {
            Surface::Graph(graph) => Some(NodeId(graph)),
            _ => None,
        }
    }

    /// What a pane shows.
    pub fn surface_of(&self, pane: &PaneRef) -> Option<Surface> {
        self.section(&pane.runner)?.surface_of(pane.pane)
    }

    /// Gives a pane the keyboard focus and brings its tab to the front, if
    /// the pane exists.
    pub fn focus(&mut self, pane: PaneRef) {
        let Some(tab) = self
            .section(&pane.runner)
            .and_then(|s| s.snapshot.tab_of(pane.pane))
            .map(|t| t.id)
        else {
            return;
        };
        self.active = Some(TabRef {
            runner: pane.runner.clone(),
            tab,
        });
        self.focused = Some(pane);
    }

    /// Brings a tab to the front, focusing its default pane.
    pub fn activate(&mut self, tab: TabRef) {
        let Some(section) = self.section(&tab.runner) else {
            return;
        };
        if section.tab(tab.tab).is_none() {
            return;
        }
        self.focused = section.default_pane(tab.tab).map(|pane| PaneRef {
            runner: tab.runner.clone(),
            pane,
        });
        self.active = Some(tab);
    }

    /// Brings the tab `step` places after the one in front to the front, in
    /// the bar's order across every section and wrapping at either end;
    /// `-1` is the previous tab. With no tab in front, the first or the last.
    pub fn cycle_tab(&mut self, step: isize) {
        let order: Vec<TabRef> = self
            .sections
            .iter()
            .flat_map(|section| {
                section.snapshot.tabs().map(|tab| TabRef {
                    runner: section.key.clone(),
                    tab: tab.id,
                })
            })
            .collect();
        if order.is_empty() {
            return;
        }
        let len = order.len() as isize;
        let next = match self
            .active
            .as_ref()
            .and_then(|active| order.iter().position(|tab| tab == active))
        {
            Some(index) => (index as isize + step).rem_euclid(len),
            None if step < 0 => len - 1,
            None => 0,
        };
        self.activate(order[next as usize].clone());
    }

    /// The tab showing `graph`, preferring the one in front.
    pub fn tab_of_graph(&self, graph: NodeId) -> Option<TabRef> {
        let shows = |tab: &TabSnapshot| {
            tab.root
                .leaves()
                .iter()
                .any(|(_, surface)| *surface == Surface::Graph(graph.0))
        };
        if let Some(active) = &self.active
            && self
                .section(&active.runner)
                .and_then(|s| s.tab(active.tab))
                .is_some_and(shows)
        {
            return Some(active.clone());
        }
        self.sections.iter().find_map(|section| {
            section.snapshot.tabs().find(|t| shows(t)).map(|t| TabRef {
                runner: section.key.clone(),
                tab: t.id,
            })
        })
    }

    /// Focuses the pane showing `graph`, returning whether one does.
    pub fn show_graph(&mut self, graph: NodeId) -> bool {
        let Some(tab) = self.tab_of_graph(graph) else {
            return false;
        };
        let pane = self.section(&tab.runner).and_then(|s| {
            s.tab(tab.tab)?
                .root
                .leaves()
                .into_iter()
                .find(|(_, surface)| *surface == Surface::Graph(graph.0))
                .map(|(pane, _)| pane)
        });
        match pane {
            Some(pane) => self.focus(PaneRef {
                runner: tab.runner,
                pane,
            }),
            None => self.activate(tab),
        }
        true
    }

    /// Whether a drag has ratios that have not been sent yet, so the caller
    /// knows whether it needs a clock.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn resize_pending(&self) -> bool {
        self.resize.pending()
    }

    pub fn update(&mut self, message: Message) -> Update {
        match message {
            // Reported in iced's vocabulary because that is what the widget
            // knows; translated here, where the maps are.
            Message::ActivatePane(pane) => {
                if let Some(pane) = self.stable(pane) {
                    self.focus(pane);
                }
                Update::none()
            }
            Message::CycleTab(step) => {
                self.cycle_tab(step);
                Update::none()
            }
            Message::TogglePlacement => {
                self.placement = match self.placement {
                    Placement::Top => Placement::Left,
                    Placement::Left => Placement::Top,
                };
                self.drag = None;
                Update::none()
            }
            Message::SplitFocused(axis) => match self.focused.clone() {
                None => Update::hint("no pane is focused"),
                Some(pane) => self.remote(
                    &pane.runner,
                    TopologyCommand::SplitWithTerminal {
                        pane: pane.pane,
                        axis,
                        profile: ProfileId::DEFAULT,
                    },
                ),
            },
            Message::CloseFocused => match self.focused.clone() {
                None => Update::none(),
                Some(pane) => {
                    self.remote(&pane.runner, TopologyCommand::ClosePane { pane: pane.pane })
                }
            },
            Message::CloseTab(tab) => {
                self.remote(&tab.runner, TopologyCommand::CloseTab { tab: tab.tab })
            }
            Message::Resize(event) => self.resized(event),
            Message::FlushResize => Update {
                commands: self.resize.take_if_due(Instant::now()),
                hint: None,
            },
            Message::ToggleSection(key) => {
                toggle(&mut self.collapsed, CollapseKey::Section(key));
                Update::none()
            }
            Message::ToggleGroup(key, group) => {
                toggle(&mut self.collapsed, CollapseKey::Group(key, group));
                Update::none()
            }
            Message::NewShell(key) => {
                let update = self.remote(
                    &key,
                    TopologyCommand::NewTerminalTab {
                        profile: ProfileId::DEFAULT,
                    },
                );
                if !update.commands.is_empty() {
                    self.expect_new_tab(&key);
                }
                update
            }
            Message::NewGroup(key) => {
                let count = self
                    .section(&key)
                    .map_or(0, |s| s.snapshot.groups().count());
                let n = count + 1;
                self.remote(
                    &key,
                    TopologyCommand::NewGroup {
                        name: format!("Group {n}"),
                        color_rgba: GROUP_COLORS[n % GROUP_COLORS.len()],
                    },
                )
            }
            Message::CycleGroupColor(key, group) => {
                let current = self
                    .section(&key)
                    .and_then(|s| s.snapshot.groups().find(|g| g.id == group))
                    .map(|g| g.color_rgba);
                let next = current
                    .and_then(|c| GROUP_COLORS.iter().position(|p| *p == c))
                    .map_or(0, |i| (i + 1) % GROUP_COLORS.len());
                self.remote(
                    &key,
                    TopologyCommand::SetGroupColor {
                        group,
                        color_rgba: GROUP_COLORS[next],
                    },
                )
            }
            Message::DissolveGroup(key, group) => {
                self.remote(&key, TopologyCommand::DissolveGroup { group })
            }
            Message::RenameGroupStart(key, group) => {
                let name = self
                    .section(&key)
                    .and_then(|s| s.snapshot.groups().find(|g| g.id == group))
                    .map(|g| g.name.clone());
                if let Some(name) = name {
                    self.renaming_group = Some((key, group, name));
                }
                Update::none()
            }
            Message::RenameGroupInput(text) => {
                if let Some((_, _, draft)) = self.renaming_group.as_mut() {
                    *draft = text;
                }
                Update::none()
            }
            Message::RenameGroupCommit => match self.renaming_group.take() {
                Some((key, group, name)) if !name.trim().is_empty() => self.remote(
                    &key,
                    TopologyCommand::RenameGroup {
                        group,
                        name: name.trim().to_owned(),
                    },
                ),
                _ => Update::none(),
            },
            Message::RenameTabStart(tab) => {
                let title = self
                    .section(&tab.runner)
                    .and_then(|s| s.tab(tab.tab))
                    .map(|t| t.title.clone());
                if let Some(title) = title {
                    self.renaming_tab = Some((tab, title));
                }
                Update::none()
            }
            Message::RenameTabInput(text) => {
                if let Some((_, draft)) = self.renaming_tab.as_mut() {
                    *draft = text;
                }
                Update::none()
            }
            Message::RenameTabCommit => {
                let Some((tab, draft)) = self.renaming_tab.take() else {
                    return Update::none();
                };
                let name = draft.trim();
                let current = self
                    .section(&tab.runner)
                    .and_then(|s| s.tab(tab.tab))
                    .map(|t| t.title.as_str());
                if name.is_empty() || current == Some(name) {
                    return Update::none();
                }
                self.remote(
                    &tab.runner,
                    TopologyCommand::RenameTab {
                        tab: tab.tab,
                        title: Some(name.to_owned()),
                    },
                )
            }
            Message::PressTab(tab) => self.press(DragSource::Tab(tab)),
            Message::PressGroup(key, group) => self.press(DragSource::Group(key, group)),
            Message::PressPane(pane) => self.press(DragSource::Pane(pane)),
            Message::DragMoved(position) => {
                if let Some(drag) = self.drag.as_mut() {
                    drag.cursor = position;
                    let origin = *drag.origin.get_or_insert(position);
                    if !drag.active && origin.distance(position) >= DRAG_THRESHOLD {
                        drag.active = true;
                    }
                }
                Update::none()
            }
            // Recorded while the press has not become a drag yet as well: the
            // pointer's moves reach this through a subscription and a hover
            // straight from the widget, and nothing orders the two.
            Message::Hover(target) => {
                let resolved = self.drag.as_ref().and_then(|drag| {
                    let section = self.section(target_runner(&target))?;
                    let drop = resolve_target(&section.snapshot, &drag.source, &target)?;
                    // A drop that would change nothing shows no marker.
                    (drop_allowed(&section.snapshot, section.synthetic(), &drag.source, &drop)
                        && drop_command(&section.snapshot, &drag.source, &drop).is_some())
                    .then(|| (marker_for(&section.snapshot, &drop), drop))
                });
                if let Some(drag) = self.drag.as_mut() {
                    (drag.marker, drag.target) = match resolved {
                        Some((marker, drop)) => (marker, Some(drop)),
                        None => (None, None),
                    };
                }
                Update::none()
            }
            Message::HoverPane(pane, side) => {
                let drop = side.map(|side| DropTarget::Pane(pane.clone(), side));
                let allowed = self.drag.as_ref().and_then(|drag| {
                    let drop = drop?;
                    let section = self.section(&pane.runner)?;
                    drop_allowed(&section.snapshot, section.synthetic(), &drag.source, &drop)
                        .then_some(drop)
                });
                if let Some(drag) = self.drag.as_mut() {
                    drag.target = allowed;
                    drag.marker = None;
                }
                Update::none()
            }
            // Leaving a region only clears a target in it: the widget the
            // pointer entered may have reported its own target first, in the
            // same event.
            Message::LeftBar => {
                if let Some(drag) = self.drag.as_mut()
                    && !matches!(drag.target, Some(DropTarget::Pane(..)))
                {
                    drag.target = None;
                    drag.marker = None;
                }
                Update::none()
            }
            Message::LeftPane(pane) => {
                if let Some(drag) = self.drag.as_mut()
                    && matches!(&drag.target, Some(DropTarget::Pane(p, _)) if *p == pane)
                {
                    drag.target = None;
                }
                Update::none()
            }
            Message::DragReleased => self.release(),
            Message::DragCancelled => {
                self.drag = None;
                Update::none()
            }
        }
    }

    fn press(&mut self, source: DragSource) -> Update {
        self.drag = Some(Drag {
            source,
            origin: None,
            cursor: Point::ORIGIN,
            active: false,
            target: None,
            marker: None,
        });
        Update::none()
    }

    /// The end of a press: a click when the pointer barely moved, a drop when
    /// it found an allowed target, nothing otherwise.
    fn release(&mut self) -> Update {
        let Some(drag) = self.drag.take() else {
            return Update::none();
        };
        if !drag.active {
            match drag.source {
                DragSource::Tab(tab) => self.activate(tab),
                DragSource::Pane(pane) => self.focus(pane),
                DragSource::Group(..) => {}
            }
            return Update::none();
        }
        let Some(target) = drag.target else {
            return Update::none();
        };
        let key = drag.source.runner().clone();
        let Some(section) = self.section(&key) else {
            return Update::none();
        };
        match drop_command(&section.snapshot, &drag.source, &target) {
            Some(command) => self.remote(&key, command),
            None => Update::none(),
        }
    }

    /// The runner's id for a pane the widget reported, in the active tab.
    fn stable(&self, pane: Pane) -> Option<PaneRef> {
        let (section, view) = self.active()?;
        Some(PaneRef {
            runner: section.key.clone(),
            pane: view.pane_id(pane)?,
        })
    }

    /// A drag of a split divider: applied here at once so the pane follows
    /// the cursor, and coalesced on its way to the runner.
    fn resized(&mut self, event: ResizeEvent) -> Update {
        let Some(active) = self.active.clone() else {
            return Update::none();
        };
        let Some(section) = self.section_mut(&active.runner) else {
            return Update::none();
        };
        let attached = section.attached;
        let Some(tab) = section.tab_view_mut(active.tab) else {
            return Update::none();
        };
        let split = tab.ids_by_split.get(&event.split).copied();
        tab.panes.resize(event.split, event.ratio);
        let Some(split) = split else {
            return Update::none();
        };
        if !attached {
            return Update::none();
        }
        self.resize.record(active.runner, split, event.ratio);
        Update {
            commands: self.resize.take_if_due(Instant::now()),
            hint: None,
        }
    }

    /// A structural change: the section's runner's to make, or refused while
    /// there is no runner attached to make it.
    fn remote(&self, key: &RunnerKey, command: TopologyCommand) -> Update {
        if key.is_synthetic() {
            return Update::hint("no runner executes these graphs");
        }
        if !self.attached(key) {
            return Update::hint("this runner's workspace is not attached");
        }
        Update {
            commands: vec![(key.clone(), command)],
            hint: None,
        }
    }

    /// Repairs the local selection after the structure changed. A focused
    /// pane that still exists keeps the focus and brings its tab to the
    /// front, wherever the pane went (a moved pane keeps its id); else the
    /// active tab stays when it survived, and otherwise the first tab of the
    /// same section, then of the first section with tabs, comes to the front
    /// with its default pane focused.
    fn repair(&mut self) {
        let followed = self.focused.as_ref().and_then(|pane| {
            let tab = self.section(&pane.runner)?.snapshot.tab_of(pane.pane)?.id;
            Some(TabRef {
                runner: pane.runner.clone(),
                tab,
            })
        });
        if let Some(tab) = followed {
            self.active = Some(tab);
        } else {
            let active_ok = self.active.as_ref().is_some_and(|tab| {
                self.section(&tab.runner)
                    .is_some_and(|s| s.tab(tab.tab).is_some())
            });
            if !active_ok {
                let same = self
                    .active
                    .as_ref()
                    .and_then(|tab| self.section(&tab.runner))
                    .and_then(|s| s.snapshot.tabs().next().map(|t| (s.key.clone(), t.id)));
                let any = || {
                    self.sections
                        .iter()
                        .find_map(|s| s.snapshot.tabs().next().map(|t| (s.key.clone(), t.id)))
                };
                self.active = same
                    .or_else(any)
                    .map(|(runner, tab)| TabRef { runner, tab });
            }
            self.focused = self.active.as_ref().and_then(|active| {
                let pane = self.section(&active.runner)?.default_pane(active.tab)?;
                Some(PaneRef {
                    runner: active.runner.clone(),
                    pane,
                })
            });
        }
        if let Some((key, group, _)) = &self.renaming_group
            && !self
                .section(key)
                .is_some_and(|s| s.snapshot.groups().any(|g| g.id == *group))
        {
            self.renaming_group = None;
        }
        if let Some((tab, _)) = &self.renaming_tab
            && self
                .section(&tab.runner)
                .and_then(|s| s.tab(tab.tab))
                .is_none()
        {
            self.renaming_tab = None;
        }
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Workspace::new()
    }
}

fn toggle(set: &mut HashSet<CollapseKey>, key: CollapseKey) {
    if !set.remove(&key) {
        set.insert(key);
    }
}

fn target_runner(target: &Target<GroupId, RunnerKey>) -> &RunnerKey {
    match target {
        Target::Before { section, .. } => section,
        Target::GroupEnd(section, _) | Target::SectionEnd(section) => section,
    }
}

/// The container a tab sits in: `Some(None)` for the top level,
/// `Some(Some(group))` for a group, `None` for a tab the snapshot lacks.
fn container_of(snapshot: &WorkspaceSnapshot, tab: TabId) -> Option<Option<GroupId>> {
    snapshot.items.iter().find_map(|item| match item {
        WorkspaceItem::Tab(t) if t.id == tab => Some(None),
        WorkspaceItem::Group(g) if g.tabs.iter().any(|t| t.id == tab) => Some(Some(g.id)),
        _ => None,
    })
}

fn group(snapshot: &WorkspaceSnapshot, id: GroupId) -> Option<&GroupSnapshot> {
    snapshot.groups().find(|g| g.id == id)
}

/// Whether a container is a locked group. The top level never is.
fn locked(snapshot: &WorkspaceSnapshot, container: Option<GroupId>) -> bool {
    container.is_some_and(|id| group(snapshot, id).is_some_and(|g| g.locked))
}

/// The top-level position of a group.
fn group_index(snapshot: &WorkspaceSnapshot, id: GroupId) -> Option<usize> {
    snapshot
        .items
        .iter()
        .position(|item| matches!(item, WorkspaceItem::Group(g) if g.id == id))
}

/// A position in a container, as drawn.
fn index_in(snapshot: &WorkspaceSnapshot, tab: TabId) -> Option<usize> {
    snapshot
        .items
        .iter()
        .find_map(|item| match item {
            WorkspaceItem::Group(g) => g.tabs.iter().position(|t| t.id == tab),
            WorkspaceItem::Tab(_) => None,
        })
        .or_else(|| {
            snapshot
                .items
                .iter()
                .position(|item| matches!(item, WorkspaceItem::Tab(t) if t.id == tab))
        })
}

/// What a hover over the tab bar means for this drag.
fn resolve_target(
    snapshot: &WorkspaceSnapshot,
    source: &DragSource,
    target: &Target<GroupId, RunnerKey>,
) -> Option<DropTarget> {
    let key = target_runner(target).clone();
    let is_group = matches!(source, DragSource::Group(..));
    match target {
        Target::Before { group, index, .. } => {
            if is_group && group.is_some() {
                return None;
            }
            Some(DropTarget::Slot(
                key,
                TabSlot {
                    group: *group,
                    index: u32::try_from(*index).unwrap_or(u32::MAX),
                },
            ))
        }
        // A group dragged onto another group's header goes before it; a tab
        // or a pane goes into it.
        Target::GroupEnd(_, id) if is_group => Some(DropTarget::Slot(
            key,
            TabSlot {
                group: None,
                index: u32::try_from(group_index(snapshot, *id)?).unwrap_or(u32::MAX),
            },
        )),
        Target::GroupEnd(_, id) => Some(DropTarget::IntoGroup(key, *id)),
        Target::SectionEnd(_) => Some(DropTarget::Slot(
            key,
            TabSlot {
                group: None,
                index: u32::try_from(snapshot.items.len()).unwrap_or(u32::MAX),
            },
        )),
    }
}

/// The tab bar's marker for a drop target: a line before a slot, the header
/// of a group a drop goes into, the section's end zone for the end.
fn marker_for(
    snapshot: &WorkspaceSnapshot,
    drop: &DropTarget,
) -> Option<Target<GroupId, RunnerKey>> {
    match drop {
        DropTarget::Slot(key, slot) => {
            let len = match slot.group {
                None => snapshot.items.len(),
                Some(id) => group(snapshot, id).map_or(0, |g| g.tabs.len()),
            };
            let index = slot.index as usize;
            if index >= len {
                return Some(match slot.group {
                    None => Target::SectionEnd(key.clone()),
                    Some(id) => Target::GroupEnd(key.clone(), id),
                });
            }
            Some(Target::Before {
                section: key.clone(),
                group: slot.group,
                index,
            })
        }
        DropTarget::IntoGroup(key, id) => Some(Target::GroupEnd(key.clone(), *id)),
        DropTarget::Pane(..) => None,
    }
}

/// Whether `source` may be dropped on `target`, given the section's current
/// structure.
///
/// Nothing moves between sections, nothing moves in a synthetic section,
/// and a locked group neither takes nor gives up a tab.
pub fn drop_allowed(
    snapshot: &WorkspaceSnapshot,
    synthetic: bool,
    source: &DragSource,
    target: &DropTarget,
) -> bool {
    if source.runner() != target.runner() || synthetic {
        return false;
    }
    let group_exists = |id: Option<GroupId>| id.is_none_or(|id| group(snapshot, id).is_some());
    match (source, target) {
        (DragSource::Tab(tab), DropTarget::Slot(_, slot)) => {
            let Some(from) = container_of(snapshot, tab.tab) else {
                return false;
            };
            if from == slot.group {
                return true;
            }
            group_exists(slot.group) && !locked(snapshot, from) && !locked(snapshot, slot.group)
        }
        (DragSource::Tab(tab), DropTarget::IntoGroup(_, id)) => {
            let Some(from) = container_of(snapshot, tab.tab) else {
                return false;
            };
            group_exists(Some(*id))
                && (from == Some(*id) || !(locked(snapshot, from) || locked(snapshot, Some(*id))))
        }
        (DragSource::Tab(tab), DropTarget::Pane(pane, _)) => {
            let Some(from) = container_of(snapshot, tab.tab) else {
                return false;
            };
            let Some(onto) = snapshot.tab_of(pane.pane).map(|t| t.id) else {
                return false;
            };
            onto != tab.tab
                && !locked(snapshot, from)
                && !locked(snapshot, container_of(snapshot, onto).flatten())
        }
        (DragSource::Group(_, id), DropTarget::Slot(_, slot)) => {
            slot.group.is_none() && group(snapshot, *id).is_some()
        }
        (DragSource::Group(..), _) => false,
        (DragSource::Pane(pane), target) => {
            let Some(from_tab) = snapshot.tab_of(pane.pane).map(|t| t.id) else {
                return false;
            };
            if locked(snapshot, container_of(snapshot, from_tab).flatten()) {
                return false;
            }
            match target {
                DropTarget::Slot(_, slot) => {
                    group_exists(slot.group) && !locked(snapshot, slot.group)
                }
                DropTarget::IntoGroup(_, id) => {
                    group_exists(Some(*id)) && !locked(snapshot, Some(*id))
                }
                DropTarget::Pane(onto, _) => {
                    let Some(onto_tab) = snapshot.tab_of(onto.pane).map(|t| t.id) else {
                        return false;
                    };
                    onto.pane != pane.pane
                        && !locked(snapshot, container_of(snapshot, onto_tab).flatten())
                }
            }
        }
    }
}

/// The command a drop sends, with the slot index counted the runner's way:
/// after the moved thing was taken out of its list.
pub fn drop_command(
    snapshot: &WorkspaceSnapshot,
    source: &DragSource,
    target: &DropTarget,
) -> Option<TopologyCommand> {
    Some(match (source, target) {
        (DragSource::Tab(tab), DropTarget::Slot(_, slot)) => {
            let mut to = *slot;
            let from = index_in(snapshot, tab.tab)?;
            if container_of(snapshot, tab.tab)? == slot.group {
                if from < slot.index as usize {
                    to.index -= 1;
                }
                if to.index as usize == from {
                    return None;
                }
            }
            TopologyCommand::MoveTab { tab: tab.tab, to }
        }
        (DragSource::Tab(tab), DropTarget::IntoGroup(_, id)) => {
            let last = group(snapshot, *id)?.tabs.last().map(|t| t.id);
            if last == Some(tab.tab) {
                return None;
            }
            TopologyCommand::MoveTab {
                tab: tab.tab,
                to: TabSlot {
                    group: Some(*id),
                    index: u32::MAX,
                },
            }
        }
        (DragSource::Tab(tab), DropTarget::Pane(pane, side)) => TopologyCommand::MergeTab {
            tab: tab.tab,
            pane: pane.pane,
            side: *side,
        },
        (DragSource::Group(_, id), DropTarget::Slot(_, slot)) => {
            let mut index = slot.index;
            let from = group_index(snapshot, *id)?;
            if from < index as usize {
                index -= 1;
            }
            if index as usize == from {
                return None;
            }
            TopologyCommand::MoveGroup { group: *id, index }
        }
        (DragSource::Pane(pane), DropTarget::Slot(_, slot)) => {
            // A pane that is its tab's only one takes the tab with it, which
            // shortens the list it came from.
            let mut to = *slot;
            let tab = snapshot.tab_of(pane.pane)?;
            if tab.root.leaf_count() == 1
                && container_of(snapshot, tab.id)? == slot.group
                && index_in(snapshot, tab.id)? < slot.index as usize
            {
                to.index -= 1;
            }
            TopologyCommand::MovePane {
                pane: pane.pane,
                to: PaneTarget::NewTab(to),
            }
        }
        (DragSource::Pane(pane), DropTarget::IntoGroup(_, id)) => TopologyCommand::MovePane {
            pane: pane.pane,
            to: PaneTarget::NewTab(TabSlot {
                group: Some(*id),
                index: u32::MAX,
            }),
        },
        (DragSource::Pane(pane), DropTarget::Pane(onto, side)) => TopologyCommand::MovePane {
            pane: pane.pane,
            to: PaneTarget::Beside {
                pane: onto.pane,
                side: *side,
            },
        },
        (DragSource::Group(..), _) => return None,
    })
}

/// Which side of a pane of `size` the point `at` (relative to the pane) is
/// near: the nearest edge when it is within 30 % of that dimension.
pub fn side_at(at: Point, size: iced::Size) -> Option<Side> {
    if size.width <= 0.0 || size.height <= 0.0 {
        return None;
    }
    let candidates = [
        (Side::Left, at.x / size.width),
        (Side::Right, (size.width - at.x) / size.width),
        (Side::Top, at.y / size.height),
        (Side::Bottom, (size.height - at.y) / size.height),
    ];
    candidates
        .into_iter()
        .filter(|(_, share)| (0.0..0.3).contains(share))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(side, _)| side)
}

/// A tab's accent as a colour, or the theme's default.
pub fn accent(tab: &TabSnapshot) -> Option<Color> {
    tab.accent_rgba.map(rgba)
}

/// A wire colour as an iced colour.
pub fn rgba([r, g, b, a]: [u8; 4]) -> Color {
    Color::from_rgba8(r, g, b, f32::from(a) / f32::from(u8::MAX))
}

/// The newest ratio per split, sent at most once per [`RESIZE_INTERVAL`].
///
/// A drag reports a ratio per cursor move and only the last one is worth
/// anything: the runner broadcasts each applied command to every client, so
/// forwarding the whole drag would multiply one gesture into a snapshot storm.
#[derive(Debug, Default)]
struct Coalescer {
    pending: HashMap<(RunnerKey, SplitId), f32>,
    sent: Option<Instant>,
}

impl Coalescer {
    fn record(&mut self, runner: RunnerKey, split: SplitId, ratio: f32) {
        self.pending.insert((runner, split), ratio);
    }

    fn pending(&self) -> bool {
        !self.pending.is_empty()
    }

    fn forget(&mut self, runner: &RunnerKey) {
        self.pending.retain(|(key, _), _| key != runner);
    }

    /// Everything held, if enough time has passed since the last batch.
    /// Ordered by runner and split so two runs of one drag produce the same
    /// commands.
    fn take_if_due(&mut self, now: Instant) -> Vec<(RunnerKey, TopologyCommand)> {
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
        let mut ratios: Vec<((RunnerKey, SplitId), f32)> = self.pending.drain().collect();
        ratios.sort_by(|a, b| (&a.0.0, a.0.1.0).cmp(&(&b.0.0, b.0.1.0)));
        ratios
            .into_iter()
            .map(|((runner, split), ratio)| (runner, TopologyCommand::ResizeSplit { split, ratio }))
            .collect()
    }
}

/// What handling a workspace message produced: commands for runners, and a
/// sentence for the status bar when a change had nowhere to go.
#[derive(Debug, Default)]
pub struct Update {
    pub commands: Vec<(RunnerKey, TopologyCommand)>,
    pub hint: Option<&'static str>,
}

impl Update {
    fn none() -> Update {
        Update::default()
    }

    fn hint(text: &'static str) -> Update {
        Update {
            commands: Vec::new(),
            hint: Some(text),
        }
    }
}

/// What the workspace chrome reports.
///
/// The presentation ones -- which tab is in front, which pane has focus,
/// where the tab bar is, what is collapsed, what is being dragged -- are
/// applied here. The rest change a runner's shared structure: they become
/// [`TopologyCommand`]s and take effect when that runner's next snapshot says
/// so.
///
/// A pane is named the way the widget named it -- an iced `Pane` -- because
/// that is all a click knows; the translation to the runner's [`PaneId`]
/// happens where the maps are.
#[derive(Debug, Clone)]
pub enum Message {
    ActivatePane(Pane),
    /// The next (`1`) or previous (`-1`) tab in the bar's order.
    CycleTab(isize),
    TogglePlacement,
    /// Split the focused pane, the new half a terminal.
    SplitFocused(Axis),
    /// Close the focused pane.
    CloseFocused,
    CloseTab(TabRef),
    Resize(ResizeEvent),
    /// The coalescing clock: sends whatever a drag left behind. Emitted by
    /// a timer subscription the browser editor does not have.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    FlushResize,
    ToggleSection(RunnerKey),
    ToggleGroup(RunnerKey, GroupId),
    NewShell(RunnerKey),
    NewGroup(RunnerKey),
    CycleGroupColor(RunnerKey, GroupId),
    DissolveGroup(RunnerKey, GroupId),
    RenameGroupStart(RunnerKey, GroupId),
    RenameGroupInput(String),
    RenameGroupCommit,
    /// A double click on a tab.
    RenameTabStart(TabRef),
    RenameTabInput(String),
    RenameTabCommit,
    PressTab(TabRef),
    PressGroup(RunnerKey, GroupId),
    PressPane(PaneRef),
    Hover(Target<GroupId, RunnerKey>),
    HoverPane(PaneRef, Option<Side>),
    /// The pointer left the tab bar.
    LeftBar,
    /// The pointer left a pane's drop overlay.
    LeftPane(PaneRef),
    DragMoved(Point),
    DragReleased,
    DragCancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeughaus_mux::TerminalId;

    /// The reverse of [`TabView::pane_id`]. Only tests need it: every id a
    /// message carries came out of the widget, so the editor never looks a
    /// pane up by the runner's id.
    fn iced_pane(tab: &TabView, id: PaneId) -> Option<Pane> {
        tab.ids_by_pane
            .iter()
            .find(|(_, stable)| **stable == id)
            .map(|(pane, _)| *pane)
    }

    fn leaf(pane: u64, surface: Surface) -> PaneNode {
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

    fn tab(id: u64, root: PaneNode) -> TabSnapshot {
        TabSnapshot {
            id: TabId(id),
            title: format!("t{id}"),
            accent_rgba: None,
            root,
        }
    }

    fn term(pane: u64) -> PaneNode {
        leaf(pane, Surface::Terminal(TerminalId(pane)))
    }

    fn snapshot(items: Vec<WorkspaceItem>) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            incarnation: RunnerIncarnation::from_bytes([7; 16]),
            revision: 1,
            items,
            detached: Vec::new(),
        }
    }

    fn one_tab(root: PaneNode) -> WorkspaceSnapshot {
        snapshot(vec![WorkspaceItem::Tab(tab(9, root))])
    }

    fn key() -> RunnerKey {
        RunnerKey::new("sha256:aa")
    }

    fn attached(snapshot: WorkspaceSnapshot) -> Workspace {
        let mut workspace = Workspace::new();
        workspace.ensure_runner(key(), "A".into());
        workspace.apply_snapshot(&key(), snapshot);
        workspace
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
            leaf(10, Surface::Graph(5)),
            split(2, Axis::Vertical, 0.75, term(11), leaf(12, Surface::Empty)),
        );
        let workspace = attached(one_tab(root.clone()));
        let (_, tab) = workspace.active().expect("the snapshot has one tab");
        assert_eq!(recover(tab.panes.layout(), tab), root);
    }

    #[test]
    fn every_pane_id_maps_both_ways() {
        let root = split(
            1,
            Axis::Vertical,
            0.5,
            leaf(10, Surface::Graph(5)),
            term(11),
        );
        let workspace = attached(one_tab(root));
        let (_, tab) = workspace.active().expect("the snapshot has one tab");
        for id in [PaneId(10), PaneId(11)] {
            let pane = iced_pane(tab, id).expect("a leaf has an iced pane");
            assert_eq!(tab.pane_id(pane), Some(id));
        }
    }

    /// A tab activated in one section keeps its focus in that section: the
    /// same pane id in another runner is a different pane.
    #[test]
    fn sections_keep_their_id_spaces_apart() {
        let mut workspace = attached(one_tab(term(10)));
        let other = RunnerKey::new("sha256:bb");
        workspace.ensure_runner(other.clone(), "B".into());
        workspace.apply_snapshot(&other, one_tab(leaf(10, Surface::Graph(3))));

        workspace.activate(TabRef {
            runner: other.clone(),
            tab: TabId(9),
        });
        assert_eq!(workspace.focused_graph(), Some(NodeId(3)));
        workspace.activate(TabRef {
            runner: key(),
            tab: TabId(9),
        });
        assert_eq!(workspace.focused_graph(), None);
        assert_eq!(
            workspace.focused_pane(),
            Some(&PaneRef {
                runner: key(),
                pane: PaneId(10)
            })
        );
    }

    #[test]
    fn a_synthetic_section_refuses_structure() {
        let mut workspace = Workspace::new();
        workspace.set_synthetic(
            RunnerKey::local(),
            "Local",
            Some(graph_snapshot([(NodeId(4), "Graph".to_owned())])),
        );
        assert_eq!(workspace.focused_graph(), Some(NodeId(4)));
        let update = workspace.update(Message::SplitFocused(Axis::Horizontal));
        assert!(update.commands.is_empty());
        assert!(update.hint.is_some());

        workspace.set_synthetic(RunnerKey::local(), "Local", None);
        assert!(workspace.sections().is_empty());
        assert!(workspace.active_tab().is_none());
    }

    #[test]
    fn focus_falls_back_to_the_first_tab_of_the_section() {
        let mut workspace = attached(snapshot(vec![
            WorkspaceItem::Tab(tab(1, term(10))),
            WorkspaceItem::Tab(tab(2, term(11))),
        ]));
        workspace.focus(PaneRef {
            runner: key(),
            pane: PaneId(11),
        });
        assert_eq!(workspace.active_tab().map(|t| t.tab), Some(TabId(2)));

        let mut next = snapshot(vec![WorkspaceItem::Tab(tab(1, term(10)))]);
        next.revision = 2;
        workspace.apply_snapshot(&key(), next);
        assert_eq!(workspace.active_tab().map(|t| t.tab), Some(TabId(1)));
        assert_eq!(workspace.focused_pane().map(|p| p.pane), Some(PaneId(10)));
    }

    #[test]
    fn a_lost_mux_empties_its_section() {
        let mut workspace = attached(one_tab(term(10)));
        workspace.detach(&key());
        assert!(!workspace.attached(&key()));
        assert!(workspace.active_tab().is_none());
    }

    #[test]
    fn a_structural_command_goes_to_the_focused_panes_runner() {
        let mut workspace = attached(one_tab(term(10)));
        let update = workspace.update(Message::SplitFocused(Axis::Horizontal));
        assert_eq!(
            update.commands,
            vec![(
                key(),
                TopologyCommand::SplitWithTerminal {
                    pane: PaneId(10),
                    axis: Axis::Horizontal,
                    profile: ProfileId::DEFAULT,
                }
            )]
        );
    }

    #[test]
    fn a_drag_sends_one_command_per_interval_with_the_newest_ratio() {
        let mut coalescer = Coalescer::default();
        let start = Instant::now();
        let resize = |ratio| {
            vec![(
                key(),
                TopologyCommand::ResizeSplit {
                    split: SplitId(1),
                    ratio,
                },
            )]
        };

        coalescer.record(key(), SplitId(1), 0.3);
        assert_eq!(coalescer.take_if_due(start), resize(0.3));

        // Mid-drag: every ratio is recorded, none is sent.
        for step in 1u8..8 {
            coalescer.record(key(), SplitId(1), 0.3 + 0.01 * f32::from(step));
            assert!(
                coalescer
                    .take_if_due(start + Duration::from_millis(u64::from(step) * 10))
                    .is_empty()
            );
        }
        assert!(coalescer.pending());
        assert_eq!(coalescer.take_if_due(start + RESIZE_INTERVAL), resize(0.37));
        assert!(!coalescer.pending());
    }

    /// The fixture for the drop rules: a loose tab 1 (two panes), a group 5
    /// with tabs 2 and 3, a locked group 6 with tab 4, and a loose tab 7.
    fn tree() -> WorkspaceSnapshot {
        snapshot(vec![
            WorkspaceItem::Tab(tab(1, split(1, Axis::Horizontal, 0.5, term(10), term(11)))),
            WorkspaceItem::Group(GroupSnapshot {
                id: GroupId(5),
                name: "g".into(),
                color_rgba: [0; 4],
                locked: false,
                tabs: vec![tab(2, term(20)), tab(3, term(30))],
            }),
            WorkspaceItem::Group(GroupSnapshot {
                id: GroupId(6),
                name: "Triggered".into(),
                color_rgba: [0; 4],
                locked: true,
                tabs: vec![tab(4, term(40))],
            }),
            WorkspaceItem::Tab(tab(7, term(70))),
        ])
    }

    fn tab_src(id: u64) -> DragSource {
        DragSource::Tab(TabRef {
            runner: key(),
            tab: TabId(id),
        })
    }

    fn pane_src(id: u64) -> DragSource {
        DragSource::Pane(PaneRef {
            runner: key(),
            pane: PaneId(id),
        })
    }

    fn slot(group: Option<u64>, index: u32) -> DropTarget {
        DropTarget::Slot(
            key(),
            TabSlot {
                group: group.map(GroupId),
                index,
            },
        )
    }

    fn into(group: u64) -> DropTarget {
        DropTarget::IntoGroup(key(), GroupId(group))
    }

    fn onto(pane: u64, side: Side) -> DropTarget {
        DropTarget::Pane(
            PaneRef {
                runner: key(),
                pane: PaneId(pane),
            },
            side,
        )
    }

    fn allowed(source: &DragSource, target: &DropTarget) -> bool {
        drop_allowed(&tree(), false, source, target)
    }

    #[test]
    fn nothing_crosses_sections_or_lands_in_a_synthetic_one() {
        let other = DropTarget::Slot(
            RunnerKey::new("sha256:bb"),
            TabSlot {
                group: None,
                index: 0,
            },
        );
        assert!(!allowed(&tab_src(1), &other));
        assert!(!drop_allowed(&tree(), true, &tab_src(1), &slot(None, 0)));
    }

    #[test]
    fn tabs_move_between_containers() {
        assert!(allowed(&tab_src(1), &slot(Some(5), 0)));
        assert!(allowed(&tab_src(2), &slot(None, 0)));
        assert!(allowed(&tab_src(7), &into(5)));
        assert!(!allowed(&tab_src(7), &slot(Some(99), 0)), "unknown group");
    }

    #[test]
    fn a_locked_group_neither_takes_nor_gives_up_a_tab() {
        assert!(!allowed(&tab_src(7), &into(6)));
        assert!(!allowed(&tab_src(7), &slot(Some(6), 0)));
        assert!(!allowed(&tab_src(4), &slot(None, 0)));
        assert!(!allowed(&tab_src(4), &into(5)));
        assert!(allowed(&tab_src(4), &slot(Some(6), 0)), "reorder inside");
        assert!(!allowed(&tab_src(4), &onto(10, Side::Left)));
        assert!(!allowed(&tab_src(7), &onto(40, Side::Left)));
        assert!(!allowed(&pane_src(40), &slot(None, 0)));
        assert!(!allowed(&pane_src(10), &into(6)));
        assert!(!allowed(&pane_src(10), &onto(40, Side::Top)));
    }

    #[test]
    fn a_tab_is_not_merged_into_itself_and_a_pane_not_beside_itself() {
        assert!(!allowed(&tab_src(1), &onto(11, Side::Right)));
        assert!(allowed(&tab_src(7), &onto(11, Side::Right)));
        assert!(!allowed(&pane_src(11), &onto(11, Side::Right)));
        assert!(allowed(&pane_src(11), &onto(10, Side::Top)));
        assert!(allowed(&pane_src(11), &slot(None, 4)));
    }

    #[test]
    fn a_group_goes_only_to_the_top_level() {
        let group = DragSource::Group(key(), GroupId(5));
        assert!(allowed(&group, &slot(None, 4)));
        assert!(!allowed(&group, &slot(Some(5), 0)));
        assert!(!allowed(&group, &into(6)));
        assert!(!allowed(&group, &onto(10, Side::Left)));
    }

    #[test]
    fn a_drop_onto_its_own_place_sends_nothing() {
        let snapshot = tree();
        // Tab 2 is the first of group 5: before itself and before its
        // successor are both where it is.
        assert_eq!(
            drop_command(&snapshot, &tab_src(2), &slot(Some(5), 0)),
            None
        );
        assert_eq!(
            drop_command(&snapshot, &tab_src(2), &slot(Some(5), 1)),
            None
        );
        assert_eq!(
            drop_command(&snapshot, &tab_src(2), &slot(Some(5), 2)),
            Some(TopologyCommand::MoveTab {
                tab: TabId(2),
                to: TabSlot {
                    group: Some(GroupId(5)),
                    index: 1
                }
            })
        );
        // Tab 3 already ends group 5.
        assert_eq!(drop_command(&snapshot, &tab_src(3), &into(5)), None);
        // Group 5 is root item 1.
        let group = DragSource::Group(key(), GroupId(5));
        assert_eq!(drop_command(&snapshot, &group, &slot(None, 1)), None);
        assert_eq!(drop_command(&snapshot, &group, &slot(None, 2)), None);
    }

    #[test]
    fn a_drop_index_is_counted_after_the_moved_item_left() {
        let snapshot = tree();
        // Tab 1 (index 0) dropped before tab 7 (index 3) lands at 2.
        assert_eq!(
            drop_command(&snapshot, &tab_src(1), &slot(None, 3)),
            Some(TopologyCommand::MoveTab {
                tab: TabId(1),
                to: TabSlot {
                    group: None,
                    index: 2
                }
            })
        );
        // Moving backwards needs no correction.
        assert_eq!(
            drop_command(&snapshot, &tab_src(7), &slot(None, 0)),
            Some(TopologyCommand::MoveTab {
                tab: TabId(7),
                to: TabSlot {
                    group: None,
                    index: 0
                }
            })
        );
        // Into another container: the source list does not matter.
        assert_eq!(
            drop_command(&snapshot, &tab_src(1), &slot(Some(5), 1)),
            Some(TopologyCommand::MoveTab {
                tab: TabId(1),
                to: TabSlot {
                    group: Some(GroupId(5)),
                    index: 1
                }
            })
        );
        assert_eq!(
            drop_command(
                &snapshot,
                &DragSource::Group(key(), GroupId(5)),
                &slot(None, 4)
            ),
            Some(TopologyCommand::MoveGroup {
                group: GroupId(5),
                index: 3
            })
        );
        // The only pane of tab 7 takes the tab with it.
        assert_eq!(
            drop_command(&snapshot, &pane_src(70), &slot(None, 4)),
            Some(TopologyCommand::MovePane {
                pane: PaneId(70),
                to: PaneTarget::NewTab(TabSlot {
                    group: None,
                    index: 3
                })
            })
        );
    }

    #[test]
    fn a_hover_over_a_group_header_means_into_it_or_before_it() {
        let snapshot = tree();
        let header = Target::GroupEnd(key(), GroupId(6));
        assert_eq!(
            resolve_target(&snapshot, &tab_src(1), &header),
            Some(into(6))
        );
        assert_eq!(
            resolve_target(&snapshot, &DragSource::Group(key(), GroupId(5)), &header),
            Some(slot(None, 2))
        );
        assert_eq!(
            resolve_target(&snapshot, &tab_src(1), &Target::SectionEnd(key())),
            Some(slot(None, 4))
        );
        assert_eq!(
            marker_for(&snapshot, &slot(None, 4)),
            Some(Target::SectionEnd(key()))
        );
    }

    #[test]
    fn a_pane_side_is_the_nearest_edge_within_thirty_percent() {
        let size = iced::Size::new(100.0, 50.0);
        assert_eq!(side_at(Point::new(90.0, 25.0), size), Some(Side::Right));
        assert_eq!(side_at(Point::new(5.0, 25.0), size), Some(Side::Left));
        assert_eq!(side_at(Point::new(50.0, 2.0), size), Some(Side::Top));
        assert_eq!(side_at(Point::new(50.0, 48.0), size), Some(Side::Bottom));
        assert_eq!(side_at(Point::new(50.0, 25.0), size), None);
    }

    /// A click on a tab is a press and a release without movement: it
    /// activates, and sends nothing.
    #[test]
    fn a_press_without_movement_is_a_click() {
        let mut workspace = attached(tree());
        workspace.update(Message::PressTab(TabRef {
            runner: key(),
            tab: TabId(7),
        }));
        workspace.update(Message::DragMoved(Point::new(10.0, 10.0)));
        workspace.update(Message::DragMoved(Point::new(11.0, 11.0)));
        let update = workspace.update(Message::DragReleased);
        assert!(update.commands.is_empty());
        assert_eq!(workspace.active_tab().map(|t| t.tab), Some(TabId(7)));
        assert!(workspace.drag.is_none());
    }

    #[test]
    fn a_drag_onto_a_tab_moves_it() {
        let mut workspace = attached(tree());
        workspace.update(Message::PressTab(TabRef {
            runner: key(),
            tab: TabId(7),
        }));
        workspace.update(Message::DragMoved(Point::new(10.0, 10.0)));
        workspace.update(Message::DragMoved(Point::new(10.0, 30.0)));
        workspace.update(Message::Hover(Target::GroupEnd(key(), GroupId(5))));
        assert_eq!(
            workspace.drag.as_ref().and_then(|d| d.marker.clone()),
            Some(Target::GroupEnd(key(), GroupId(5)))
        );
        let update = workspace.update(Message::DragReleased);
        assert_eq!(
            update.commands,
            vec![(
                key(),
                TopologyCommand::MoveTab {
                    tab: TabId(7),
                    to: TabSlot {
                        group: Some(GroupId(5)),
                        index: u32::MAX
                    }
                }
            )]
        );
    }

    fn tab_ref(id: u64) -> TabRef {
        TabRef {
            runner: key(),
            tab: TabId(id),
        }
    }

    fn revision(n: u64, items: Vec<WorkspaceItem>) -> WorkspaceSnapshot {
        let mut snapshot = snapshot(items);
        snapshot.revision = n;
        snapshot
    }

    #[test]
    fn a_moved_pane_keeps_the_focus_and_brings_its_new_tab_forward() {
        let mut workspace = attached(snapshot(vec![
            WorkspaceItem::Tab(tab(1, split(1, Axis::Horizontal, 0.5, term(10), term(11)))),
            WorkspaceItem::Tab(tab(2, term(20))),
        ]));
        workspace.focus(PaneRef {
            runner: key(),
            pane: PaneId(11),
        });

        workspace.apply_snapshot(
            &key(),
            revision(
                2,
                vec![
                    WorkspaceItem::Tab(tab(1, term(10))),
                    WorkspaceItem::Tab(tab(2, term(20))),
                    WorkspaceItem::Tab(tab(8, term(11))),
                ],
            ),
        );
        assert_eq!(workspace.active_tab(), Some(&tab_ref(8)));
        assert_eq!(workspace.focused_pane().map(|p| p.pane), Some(PaneId(11)));
    }

    #[test]
    fn a_shell_this_window_asked_for_comes_to_the_front() {
        let one = || WorkspaceItem::Tab(tab(1, term(10)));
        let two = || WorkspaceItem::Tab(tab(2, term(20)));
        let three = || WorkspaceItem::Tab(tab(3, term(30)));
        let mut workspace = attached(snapshot(vec![one(), two()]));
        assert_eq!(workspace.update(Message::NewShell(key())).commands.len(), 1);

        workspace.apply_snapshot(&key(), revision(2, vec![one(), two(), three()]));
        assert_eq!(workspace.active_tab(), Some(&tab_ref(3)));
        assert_eq!(workspace.focused_pane().map(|p| p.pane), Some(PaneId(30)));

        // Answered once: a tab someone else adds later stays behind.
        let four = WorkspaceItem::Tab(tab(4, term(40)));
        workspace.apply_snapshot(&key(), revision(3, vec![one(), two(), three(), four]));
        assert_eq!(workspace.active_tab(), Some(&tab_ref(3)));
    }

    #[test]
    fn cycling_walks_the_bar_through_groups_and_wraps() {
        let mut workspace = attached(tree());
        workspace.activate(tab_ref(1));
        let mut seen = Vec::new();
        for _ in 0..6 {
            workspace.update(Message::CycleTab(1));
            seen.push(workspace.active_tab().map(|t| t.tab.0));
        }
        assert_eq!(seen, [2, 3, 4, 7, 1, 2].map(Some));
        workspace.update(Message::CycleTab(-1));
        workspace.update(Message::CycleTab(-1));
        assert_eq!(workspace.active_tab(), Some(&tab_ref(7)));
        // The front tab's default pane takes the keyboard with it.
        assert_eq!(workspace.focused_pane().map(|p| p.pane), Some(PaneId(70)));
    }

    #[test]
    fn renaming_a_tab_sends_the_trimmed_new_title_only() {
        let mut workspace = attached(one_tab(term(10)));
        workspace.update(Message::RenameTabStart(tab_ref(9)));
        workspace.update(Message::RenameTabInput(" x ".into()));
        assert_eq!(
            workspace.update(Message::RenameTabCommit).commands,
            vec![(
                key(),
                TopologyCommand::RenameTab {
                    tab: TabId(9),
                    title: Some("x".into()),
                }
            )]
        );
        assert!(workspace.renaming_tab.is_none());

        // Empty, and unchanged from the current title "t9".
        for draft in ["  ", "t9"] {
            workspace.update(Message::RenameTabStart(tab_ref(9)));
            workspace.update(Message::RenameTabInput(draft.into()));
            assert!(
                workspace
                    .update(Message::RenameTabCommit)
                    .commands
                    .is_empty()
            );
        }
    }

    #[test]
    fn only_a_tab_that_is_one_graph_names_a_graph() {
        let graph = || leaf(10, Surface::Graph(5));
        let workspace = attached(snapshot(vec![
            WorkspaceItem::Tab(tab(1, graph())),
            WorkspaceItem::Tab(tab(2, split(1, Axis::Horizontal, 0.5, graph(), term(11)))),
            WorkspaceItem::Tab(tab(3, term(30))),
        ]));
        assert_eq!(workspace.single_graph(&tab_ref(1)), Some(NodeId(5)));
        assert_eq!(workspace.single_graph(&tab_ref(2)), None);
        assert_eq!(workspace.single_graph(&tab_ref(3)), None);
        assert_eq!(workspace.single_graph(&tab_ref(4)), None);
    }
}
