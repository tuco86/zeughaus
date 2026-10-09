//! The shared workspace's structure, as the runner owns it.
//!
//! Pure: no sessions, no sockets. A [`TopologyCommand`] is applied to the
//! tree, the caller is told which terminals to create (through the `spawn`
//! callback, because a split needs its terminal's id before the tree can
//! hold it) and which to kill (returned, because the tree is updated first
//! and the kill must not fail it), and every applied command is one
//! revision, as is a terminal title the snapshot shows changing. The
//! invariants live here and nowhere else: never an empty tab, groups one
//! level deep, a locked group filled and emptied by the runner alone, split
//! ratios kept away from the edges. A workspace may hold no tab at all.

use std::collections::{BTreeSet, HashMap};

use super::persist::{self, SavedGroup, SavedItem, SavedTab, SavedTerminal, SavedWorkspace};
use super::{GraphSync, OwnedPlacement};

use zeughaus_mux::codec::{MAX_GROUPS, MAX_LEAVES, MAX_TABS};
use zeughaus_mux::workspace::{MAX_TITLE_BYTES, MAX_TREE_DEPTH, MIN_RATIO, ProfileId};
use zeughaus_mux::{
    AttachTarget, Axis, DetachedTerminal, GroupId, GroupSnapshot, PaneId, PaneNode, PaneTarget,
    RunnerIncarnation, SplitId, SurfaceRef, TabId, TabSnapshot, TerminalId, TopologyCommand,
    WorkspaceItem, WorkspaceSnapshot,
};

/// Why a tab or pane does not cross the boundary of a locked group: its
/// tabs are the runs the runner put there, one terminal each.
const LOCKED_MOVE: &str = "tabs do not move in or out of a locked group";
/// A locked group's tabs are one run each; a pane split into one would sit
/// where nothing can move it out, and would die with the run's tab.
const LOCKED_SPLIT: &str = "a locked group takes no new panes";

/// The runner's locked group, created for the first externally triggered
/// run. A client can neither create nor dissolve one.
const TRIGGERED_NAME: &str = "Triggered";
const TRIGGERED_COLOR: [u8; 4] = [128, 128, 128, 64];

/// What a tree holds while it is taken apart and put back together.
const PLACEHOLDER: PaneNode = PaneNode::Leaf {
    pane_id: PaneId(0),
    surface: SurfaceRef::Empty,
};

/// A tab as the runner keeps it: the user's title override is separate from
/// the title shown, which follows the terminal when there is no override.
#[derive(Debug, Clone)]
struct Tab {
    id: TabId,
    title: Option<String>,
    accent: Option<[u8; 4]>,
    root: PaneNode,
}

#[derive(Debug, Clone)]
struct Group {
    id: GroupId,
    name: String,
    color: [u8; 4],
    /// See [`GroupSnapshot::locked`].
    locked: bool,
    tabs: Vec<Tab>,
}

#[derive(Debug, Clone)]
enum Item {
    Tab(Tab),
    Group(Group),
}

/// Where a tab sits: `group` is the item index of its group (`None` at the
/// top level), `index` the position within that container. Valid until the
/// items change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TabPos {
    group: Option<usize>,
    index: usize,
}

/// What applying a command asks the caller to do besides taking the new
/// snapshot.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Terminals whose panes are gone: kill their sessions.
    pub killed: Vec<TerminalId>,
}

pub struct Workspace {
    incarnation: RunnerIncarnation,
    revision: u64,
    /// The top level, in display order.
    items: Vec<Item>,
    next_tab: u64,
    next_group: u64,
    next_split: u64,
    next_pane: u64,
    /// Terminals the runner started for itself (a job's process). Their
    /// lifetime is the runner's, not a pane's: closing the pane that shows
    /// one detaches it, only `CloseTerminal`, closing its tab in the locked
    /// group and the runner end it.
    owned: BTreeSet<TerminalId>,
    /// The subset of `owned` no pane currently shows.
    detached: BTreeSet<TerminalId>,
}

impl Workspace {
    /// The workspace every runner starts with: no tabs. The graphs it
    /// executes get theirs from [`Workspace::sync_graphs`].
    pub fn new(incarnation: RunnerIncarnation) -> Workspace {
        Workspace {
            incarnation,
            revision: 1,
            items: Vec::new(),
            next_tab: 1,
            next_group: 1,
            next_split: 1,
            next_pane: 1,
            owned: BTreeSet::new(),
            detached: BTreeSet::new(),
        }
    }

    /// Takes ownership of a terminal the runner started itself, placed as
    /// `placement` says.
    pub fn add_owned(&mut self, terminal: TerminalId, placement: OwnedPlacement) {
        self.owned.insert(terminal);
        match placement {
            OwnedPlacement::Detached => {
                self.detached.insert(terminal);
            }
            // A run the snapshot has no room for waits detached, where a
            // client can attach it once other tabs are gone.
            OwnedPlacement::Triggered if self.room_for(1, 1).is_ok() => {
                let tab = self.new_tab(SurfaceRef::Terminal(terminal));
                self.locked_group().tabs.push(tab);
            }
            OwnedPlacement::Triggered => {
                self.detached.insert(terminal);
            }
        }
        self.revision += 1;
    }

    /// Moves an owned terminal out of the locked group into the detached
    /// list: a run that failed is no longer the news the group is for, and
    /// a shell left behind in it stays reachable through the detached list.
    /// A terminal a client attached elsewhere stays where it is. Returns
    /// whether anything moved.
    pub fn hide_owned(&mut self, terminal: TerminalId) -> bool {
        if !self.owned.contains(&terminal) {
            return false;
        }
        let Some(pos) = self
            .pane_of(terminal)
            .and_then(|pane| self.pane_pos(pane).ok())
        else {
            return false;
        };
        if !self.locked(pos.group) {
            return false;
        }
        // A locked group's tab is one run's and is never split.
        self.take_tab(pos);
        self.detached.insert(terminal);
        self.revision += 1;
        true
    }

    /// The runner's locked group, created at the end of the top level the
    /// first time a run needs it.
    fn locked_group(&mut self) -> &mut Group {
        let exists = self
            .items
            .iter()
            .any(|item| matches!(item, Item::Group(group) if group.locked));
        if !exists {
            let id = GroupId(self.next_group);
            self.next_group += 1;
            self.items.push(Item::Group(Group {
                id,
                name: TRIGGERED_NAME.to_owned(),
                color: TRIGGERED_COLOR,
                locked: true,
                tabs: Vec::new(),
            }));
        }
        self.items
            .iter_mut()
            .find_map(|item| match item {
                Item::Group(group) if group.locked => Some(group),
                _ => None,
            })
            .expect("the locked group exists")
    }

    /// The structure as a restarted runner reads it back. The terminal
    /// counter and the per-terminal records are the service's.
    pub fn to_saved(&self, next_terminal: u64, terminals: Vec<SavedTerminal>) -> SavedWorkspace {
        let tab = |tab: &Tab| SavedTab {
            id: tab.id,
            title: tab.title.clone(),
            accent: tab.accent,
            root: tab.root.clone(),
        };
        SavedWorkspace {
            version: persist::VERSION,
            items: self
                .items
                .iter()
                .map(|item| match item {
                    Item::Tab(t) => SavedItem::Tab(tab(t)),
                    Item::Group(group) => SavedItem::Group(SavedGroup {
                        id: group.id,
                        name: group.name.clone(),
                        color_rgba: group.color,
                        locked: group.locked,
                        tabs: group.tabs.iter().map(tab).collect(),
                    }),
                })
                .collect(),
            next_tab: self.next_tab,
            next_group: self.next_group,
            next_split: self.next_split,
            next_pane: self.next_pane,
            next_terminal,
            owned: self.owned.iter().copied().collect(),
            detached: self.detached.iter().copied().collect(),
            terminals,
        }
    }

    /// Rebuilds a saved workspace around the terminals that came back.
    ///
    /// A pane of a terminal that did not is removed like a closed one, a tab
    /// left without panes goes, and so does a group left without tabs unless
    /// it is the locked one, which the next triggered run fills again. Graph
    /// panes stay: whether their graph still exists is the next
    /// [`Workspace::sync_graphs`]'s to say. Every owned terminal ends up
    /// shown or detached.
    pub fn restore(
        incarnation: RunnerIncarnation,
        saved: &SavedWorkspace,
        alive: impl Fn(TerminalId) -> bool,
    ) -> Workspace {
        let mut workspace = Workspace {
            incarnation,
            revision: 1,
            items: Vec::new(),
            next_tab: saved.next_tab.max(1),
            next_group: saved.next_group.max(1),
            next_split: saved.next_split.max(1),
            next_pane: saved.next_pane.max(1),
            owned: saved.owned.iter().copied().filter(|t| alive(*t)).collect(),
            detached: BTreeSet::new(),
        };
        let restore_tab = |tab: &SavedTab| {
            let mut root = Some(tab.root.clone());
            for (pane, surface) in tab.root.leaves() {
                if let SurfaceRef::Terminal(terminal) = surface
                    && !alive(terminal)
                {
                    root = root.and_then(|root| remove_leaf(root, pane));
                }
            }
            root.map(|root| Tab {
                id: tab.id,
                title: tab.title.clone(),
                accent: tab.accent,
                root,
            })
        };
        for item in &saved.items {
            match item {
                SavedItem::Tab(tab) => {
                    if let Some(tab) = restore_tab(tab) {
                        workspace.items.push(Item::Tab(tab));
                    }
                }
                SavedItem::Group(group) => {
                    let tabs: Vec<Tab> = group.tabs.iter().filter_map(&restore_tab).collect();
                    // A group the user left empty is theirs to remove; one
                    // the restore emptied held nothing but dead terminals.
                    if tabs.is_empty() && !group.tabs.is_empty() && !group.locked {
                        continue;
                    }
                    workspace.items.push(Item::Group(Group {
                        id: group.id,
                        name: group.name.clone(),
                        color: group.color_rgba,
                        locked: group.locked,
                        tabs,
                    }));
                }
            }
        }
        let shown: BTreeSet<TerminalId> = workspace
            .tabs()
            .flat_map(|tab| tab.root.terminals().collect::<Vec<_>>())
            .collect();
        workspace.detached = workspace.owned.difference(&shown).copied().collect();
        workspace
    }

    /// Whether `terminal` belongs to this workspace: a pane shows it, or
    /// the runner owns it.
    pub fn knows(&self, terminal: TerminalId) -> bool {
        self.owned.contains(&terminal)
            || self
                .tabs()
                .any(|tab| tab.root.terminals().any(|t| t == terminal))
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Records that `terminal`'s title changed. It is a new revision when a
    /// snapshot shows that title -- a tab without an override named after
    /// it, or the detached list -- and nothing otherwise; returns whether.
    pub fn title_changed(&mut self, terminal: TerminalId) -> bool {
        let shown = self.detached.contains(&terminal)
            || self.tabs().any(|tab| {
                tab.title.is_none()
                    && title_source(&tab.root) == Some(SurfaceRef::Terminal(terminal))
            });
        if shown {
            self.revision += 1;
        }
        shown
    }

    /// Brings the graph panes in line with the document and returns whether
    /// that is a new revision.
    ///
    /// A leaf of a graph the document no longer holds goes like a closed pane.
    /// Each graph of this runner's that is not in `seen` gets a tab of its
    /// own unless a pane already shows it, and is recorded there: a graph
    /// whose tab a user closed stays closed for as long as `seen` lives,
    /// which is the process. A tab named after a graph whose name changed
    /// since `old_names` is a new revision too.
    pub fn sync_graphs(
        &mut self,
        update: &GraphSync,
        old_names: &HashMap<u64, String>,
        seen: &mut BTreeSet<u64>,
    ) -> bool {
        let mut changed = false;
        let gone: Vec<PaneId> = self
            .tabs()
            .flat_map(|tab| tab.root.leaves())
            .filter_map(|(pane, surface)| match surface {
                SurfaceRef::Graph(graph) if !update.exists.contains(&graph) => Some(pane),
                _ => None,
            })
            .collect();
        for pane in gone {
            changed |= self.remove_pane(pane).is_ok();
        }
        // A graph without room for its tab stays out of `seen`, so a later
        // sync adds it once there is room.
        let mut full = false;
        for &graph in &update.owned {
            if seen.contains(&graph) {
                continue;
            }
            if !self.shows_graph(graph) {
                if full {
                    continue;
                }
                if self.room_for(1, 1).is_err() {
                    eprintln!("[mux] no room for the tab of graph {graph}");
                    full = true;
                    continue;
                }
                let tab = self.new_tab(SurfaceRef::Graph(graph));
                self.items.push(Item::Tab(tab));
                changed = true;
            }
            seen.insert(graph);
        }
        let renamed = self.tabs().any(|tab| {
            tab.title.is_none()
                && matches!(title_source(&tab.root), Some(SurfaceRef::Graph(graph))
                    if old_names.get(&graph) != update.names.get(&graph))
        });
        if changed || renamed {
            self.revision += 1;
        }
        changed || renamed
    }

    fn shows_graph(&self, graph: u64) -> bool {
        self.tabs().any(|tab| {
            tab.root
                .leaves()
                .iter()
                .any(|(_, surface)| *surface == SurfaceRef::Graph(graph))
        })
    }

    /// Every terminal any pane shows.
    #[cfg(test)]
    pub fn terminals(&self) -> Vec<TerminalId> {
        self.tabs()
            .flat_map(|tab| tab.root.terminals().collect::<Vec<_>>())
            .collect()
    }

    /// The snapshot at the current revision. `title_of` resolves a
    /// terminal's current title and `graph_title` a graph's name, for tabs
    /// without an override.
    pub fn snapshot(
        &self,
        title_of: &dyn Fn(TerminalId) -> Option<String>,
        graph_title: &dyn Fn(u64) -> Option<String>,
    ) -> WorkspaceSnapshot {
        let tab = |tab: &Tab| TabSnapshot {
            id: tab.id,
            title: tab
                .title
                .clone()
                .unwrap_or_else(|| default_title(&tab.root, title_of, graph_title)),
            accent_rgba: tab.accent,
            root: tab.root.clone(),
        };
        WorkspaceSnapshot {
            incarnation: self.incarnation,
            revision: self.revision,
            items: self
                .items
                .iter()
                .map(|item| match item {
                    Item::Tab(t) => WorkspaceItem::Tab(tab(t)),
                    Item::Group(group) => WorkspaceItem::Group(GroupSnapshot {
                        id: group.id,
                        name: group.name.clone(),
                        color_rgba: group.color,
                        locked: group.locked,
                        tabs: group.tabs.iter().map(tab).collect(),
                    }),
                })
                .collect(),
            detached: self
                .detached
                .iter()
                .map(|terminal| DetachedTerminal {
                    terminal: *terminal,
                    title: title_of(*terminal)
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| terminal.0.to_string()),
                })
                .collect(),
        }
    }

    /// Applies a structural command. `spawn` creates the terminal a command
    /// needs; a failure there refuses the command and changes nothing, as
    /// does every other refusal. `TakeControl`/`ReleaseControl` are not
    /// structural and are refused here: the service answers them from its
    /// lease table.
    pub fn apply(
        &mut self,
        command: &TopologyCommand,
        spawn: &mut dyn FnMut(ProfileId) -> Result<TerminalId, String>,
    ) -> Result<Applied, String> {
        let applied = match command {
            TopologyCommand::NewTerminalTab { profile } => {
                self.room_for(1, 1)?;
                let terminal = spawn(*profile)?;
                let tab = self.new_tab(SurfaceRef::Terminal(terminal));
                self.items.push(Item::Tab(tab));
                Applied::default()
            }
            TopologyCommand::SplitWithTerminal {
                pane,
                axis,
                profile,
            } => {
                let pos = self.pane_pos(*pane)?;
                if self.locked(pos.group) {
                    return Err(LOCKED_SPLIT.to_owned());
                }
                self.fits_beside(pos, *pane, 1)?;
                self.room_for(0, 1)?;
                let terminal = spawn(*profile)?;
                let leaf = self.new_leaf(SurfaceRef::Terminal(terminal));
                self.put_beside(pos, *pane, *axis, leaf, false);
                Applied::default()
            }
            TopologyCommand::ClosePane { pane } => {
                let surface = self.remove_pane(*pane)?;
                self.detach_or_kill(terminal_of(surface).into_iter())
            }
            TopologyCommand::CloseTab { tab } => {
                let pos = self.tab_pos(*tab)?;
                let locked = self.locked(pos.group);
                let removed = self.take_tab(pos);
                if locked {
                    // The tab is the run's: its terminal goes with it, the
                    // runner's own included.
                    let killed: Vec<TerminalId> = removed.root.terminals().collect();
                    for terminal in &killed {
                        self.owned.remove(terminal);
                        self.detached.remove(terminal);
                    }
                    Applied { killed }
                } else {
                    self.detach_or_kill(removed.root.terminals())
                }
            }
            TopologyCommand::ResizeSplit { split, ratio } => {
                if !ratio.is_finite() {
                    return Err("ratio is not a number".to_owned());
                }
                let ratio = ratio.clamp(MIN_RATIO, 1.0 - MIN_RATIO);
                let found = self
                    .tabs_mut()
                    .any(|tab| set_ratio(&mut tab.root, *split, ratio));
                if !found {
                    return Err(format!("no split {split}"));
                }
                Applied::default()
            }
            TopologyCommand::RenameTab { tab, title } => {
                let pos = self.tab_pos(*tab)?;
                self.tab_at_mut(pos).title = title.clone().filter(|t| !t.trim().is_empty());
                Applied::default()
            }
            TopologyCommand::SetTabAccent { tab, accent_rgba } => {
                let pos = self.tab_pos(*tab)?;
                self.tab_at_mut(pos).accent = *accent_rgba;
                Applied::default()
            }
            TopologyCommand::TakeControl { .. } | TopologyCommand::ReleaseControl { .. } => {
                return Err("not a structural command".to_owned());
            }
            TopologyCommand::AttachTerminal { terminal, target } => {
                if !self.owned.contains(terminal) {
                    return Err(format!("{terminal} is not the runner's"));
                }
                if !self.detached.contains(terminal) {
                    return Err(format!("{terminal} is already shown"));
                }
                match target {
                    AttachTarget::NewTab => {
                        self.room_for(1, 1)?;
                        let tab = self.new_tab(SurfaceRef::Terminal(*terminal));
                        self.items.push(Item::Tab(tab));
                    }
                    AttachTarget::Split { pane, axis } => {
                        let pos = self.pane_pos(*pane)?;
                        if self.locked(pos.group) {
                            return Err(LOCKED_SPLIT.to_owned());
                        }
                        self.fits_beside(pos, *pane, 1)?;
                        self.room_for(0, 1)?;
                        let leaf = self.new_leaf(SurfaceRef::Terminal(*terminal));
                        self.put_beside(pos, *pane, *axis, leaf, false);
                    }
                }
                self.detached.remove(terminal);
                Applied::default()
            }
            TopologyCommand::CloseTerminal { terminal } => {
                if !self.owned.contains(terminal) {
                    return Err(format!("{terminal} is not the runner's"));
                }
                if let Some(pane) = self.pane_of(*terminal) {
                    self.remove_pane(pane)?;
                }
                self.owned.remove(terminal);
                self.detached.remove(terminal);
                Applied {
                    killed: vec![*terminal],
                }
            }
            TopologyCommand::OpenGraph { graph } => {
                self.room_for(1, 1)?;
                let tab = self.new_tab(SurfaceRef::Graph(*graph));
                self.items.push(Item::Tab(tab));
                Applied::default()
            }
            TopologyCommand::NewGroup { name, color_rgba } => {
                // One place in the snapshot's bound stays free for the
                // locked group, which the runner creates whenever a
                // triggered run needs it.
                let groups = self
                    .items
                    .iter()
                    .filter(|item| matches!(item, Item::Group(group) if !group.locked))
                    .count();
                if groups >= MAX_GROUPS - 1 {
                    return Err(format!("at most {} groups", MAX_GROUPS - 1));
                }
                let id = GroupId(self.next_group);
                self.next_group += 1;
                self.items.push(Item::Group(Group {
                    id,
                    name: name.clone(),
                    color: *color_rgba,
                    locked: false,
                    tabs: Vec::new(),
                }));
                Applied::default()
            }
            TopologyCommand::RenameGroup { group, name } => {
                let index = self.group_index(*group)?;
                self.group_at_mut(index).name = name.clone();
                Applied::default()
            }
            TopologyCommand::SetGroupColor { group, color_rgba } => {
                let index = self.group_index(*group)?;
                self.group_at_mut(index).color = *color_rgba;
                Applied::default()
            }
            TopologyCommand::DissolveGroup { group } => {
                let index = self.group_index(*group)?;
                if self.locked(Some(index)) {
                    return Err("a locked group stays".to_owned());
                }
                let Item::Group(dissolved) = self.items.remove(index) else {
                    unreachable!("group_index names a group");
                };
                self.items
                    .splice(index..index, dissolved.tabs.into_iter().map(Item::Tab));
                Applied::default()
            }
            TopologyCommand::MoveGroup { group, index } => {
                let from = self.group_index(*group)?;
                let moved = self.items.remove(from);
                let at = (*index as usize).min(self.items.len());
                self.items.insert(at, moved);
                Applied::default()
            }
            TopologyCommand::MoveTab { tab, to } => {
                let from = self.tab_pos(*tab)?;
                let target = to.group.map(|g| self.group_index(g)).transpose()?;
                if from.group != target && (self.locked(from.group) || self.locked(target)) {
                    return Err(LOCKED_MOVE.to_owned());
                }
                let moved = self.take_tab(from);
                // Taking a top-level tab out shifts the items after it.
                let target = to
                    .group
                    .map(|g| self.group_index(g).expect("checked above"));
                self.insert_tab(target, to.index, moved);
                Applied::default()
            }
            TopologyCommand::MergeTab { tab, pane, side } => {
                let from = self.tab_pos(*tab)?;
                let to = self.pane_pos(*pane)?;
                if from == to {
                    return Err("a tab cannot merge into itself".to_owned());
                }
                if self.locked(from.group) || self.locked(to.group) {
                    return Err(LOCKED_MOVE.to_owned());
                }
                self.fits_beside(to, *pane, self.tab_at(from).root.depth())?;
                let merged = self.take_tab(from);
                let to = self.pane_pos(*pane).expect("the pane is in another tab");
                self.put_beside(to, *pane, side.axis(), merged.root, side.is_first());
                Applied::default()
            }
            TopologyCommand::MovePane { pane, to } => {
                let from = self.pane_pos(*pane)?;
                if self.locked(from.group) {
                    return Err(LOCKED_MOVE.to_owned());
                }
                match to {
                    PaneTarget::NewTab(slot) => {
                        let target = slot.group.map(|g| self.group_index(g)).transpose()?;
                        if self.locked(target) {
                            return Err(LOCKED_MOVE.to_owned());
                        }
                        // A pane that is its tab's only one takes the tab
                        // with it, so the count stays.
                        if self.tab_at(from).root.leaf_count() > 1 {
                            self.room_for(1, 0)?;
                        }
                    }
                    PaneTarget::Beside { pane: target, .. } => {
                        if target == pane {
                            return Err("a pane cannot move beside itself".to_owned());
                        }
                        let at = self.pane_pos(*target)?;
                        if self.locked(at.group) {
                            return Err(LOCKED_MOVE.to_owned());
                        }
                        self.fits_beside(at, *target, 1)?;
                    }
                }
                let surface = self.remove_pane(*pane)?;
                // The pane keeps its id: a client focused on it stays so.
                let leaf = PaneNode::Leaf {
                    pane_id: *pane,
                    surface,
                };
                match to {
                    PaneTarget::NewTab(slot) => {
                        let tab = Tab {
                            id: self.mint_tab(),
                            title: None,
                            accent: None,
                            root: leaf,
                        };
                        let target = slot
                            .group
                            .map(|g| self.group_index(g).expect("checked above"));
                        self.insert_tab(target, slot.index, tab);
                    }
                    PaneTarget::Beside { pane: target, side } => {
                        let at = self.pane_pos(*target).expect("checked above");
                        self.put_beside(at, *target, side.axis(), leaf, side.is_first());
                    }
                }
                Applied::default()
            }
        };
        self.revision += 1;
        Ok(applied)
    }

    /// Every tab, loose and grouped, in display order.
    fn tabs(&self) -> impl Iterator<Item = &Tab> {
        self.items.iter().flat_map(|item| match item {
            Item::Tab(tab) => std::slice::from_ref(tab).iter(),
            Item::Group(group) => group.tabs.iter(),
        })
    }

    fn tabs_mut(&mut self) -> impl Iterator<Item = &mut Tab> {
        self.items.iter_mut().flat_map(|item| match item {
            Item::Tab(tab) => std::slice::from_mut(tab).iter_mut(),
            Item::Group(group) => group.tabs.iter_mut(),
        })
    }

    fn find_tab(&self, wanted: impl Fn(&Tab) -> bool) -> Option<TabPos> {
        self.items
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                Item::Tab(tab) => wanted(tab).then_some(TabPos { group: None, index }),
                Item::Group(group) => group.tabs.iter().position(&wanted).map(|i| TabPos {
                    group: Some(index),
                    index: i,
                }),
            })
    }

    fn tab_pos(&self, id: TabId) -> Result<TabPos, String> {
        self.find_tab(|tab| tab.id == id)
            .ok_or_else(|| format!("no tab {id}"))
    }

    /// Where the tab holding the leaf `pane` sits.
    fn pane_pos(&self, pane: PaneId) -> Result<TabPos, String> {
        self.find_tab(|tab| tab.root.find(pane).is_some())
            .ok_or_else(|| format!("no pane {pane}"))
    }

    fn tab_at(&self, pos: TabPos) -> &Tab {
        let tab = match pos.group {
            None => match self.items.get(pos.index) {
                Some(Item::Tab(tab)) => Some(tab),
                _ => None,
            },
            Some(group) => match self.items.get(group) {
                Some(Item::Group(group)) => group.tabs.get(pos.index),
                _ => None,
            },
        };
        tab.expect("a position this workspace handed out")
    }

    fn tab_at_mut(&mut self, pos: TabPos) -> &mut Tab {
        let tab = match pos.group {
            None => match self.items.get_mut(pos.index) {
                Some(Item::Tab(tab)) => Some(tab),
                _ => None,
            },
            Some(group) => match self.items.get_mut(group) {
                Some(Item::Group(group)) => group.tabs.get_mut(pos.index),
                _ => None,
            },
        };
        tab.expect("a position this workspace handed out")
    }

    /// Takes the tab at `pos` out of its container. A group it leaves
    /// empty stays: removing a group is its own command.
    fn take_tab(&mut self, pos: TabPos) -> Tab {
        match pos.group {
            None => match self.items.remove(pos.index) {
                Item::Tab(tab) => tab,
                Item::Group(_) => panic!("item {} is not a tab", pos.index),
            },
            Some(group) => self.group_at_mut(group).tabs.remove(pos.index),
        }
    }

    /// Puts `tab` at `index` of the top level (`group: None`) or of the
    /// group at that item index; an index past the end appends.
    fn insert_tab(&mut self, group: Option<usize>, index: u32, tab: Tab) {
        let index = index as usize;
        match group {
            None => {
                let at = index.min(self.items.len());
                self.items.insert(at, Item::Tab(tab));
            }
            Some(group) => {
                let tabs = &mut self.group_at_mut(group).tabs;
                tabs.insert(index.min(tabs.len()), tab);
            }
        }
    }

    /// The item index of the group `id`.
    fn group_index(&self, id: GroupId) -> Result<usize, String> {
        self.items
            .iter()
            .position(|item| matches!(item, Item::Group(group) if group.id == id))
            .ok_or_else(|| format!("no group {id}"))
    }

    fn group_at_mut(&mut self, index: usize) -> &mut Group {
        match &mut self.items[index] {
            Item::Group(group) => group,
            Item::Tab(_) => panic!("item {index} is not a group"),
        }
    }

    /// Whether the container at `group` (an item index; `None` is the top
    /// level) is a locked group.
    fn locked(&self, group: Option<usize>) -> bool {
        group.is_some_and(|g| matches!(&self.items[g], Item::Group(group) if group.locked))
    }

    /// The pane showing `terminal`, if one does.
    fn pane_of(&self, terminal: TerminalId) -> Option<PaneId> {
        self.tabs().find_map(|tab| {
            tab.root
                .leaves()
                .into_iter()
                .find(|(_, surface)| *surface == SurfaceRef::Terminal(terminal))
                .map(|(pane, _)| pane)
        })
    }

    /// Takes the leaf `pane` out of its tab, dropping the tab with it when
    /// it was the tab's last, and returns what the pane showed.
    fn remove_pane(&mut self, pane: PaneId) -> Result<SurfaceRef, String> {
        let pos = self.pane_pos(pane)?;
        let tab = self.tab_at_mut(pos);
        let surface = match tab.root.find(pane) {
            Some(PaneNode::Leaf { surface, .. }) => *surface,
            _ => return Err(format!("no pane {pane}")),
        };
        match remove_leaf(std::mem::replace(&mut tab.root, PLACEHOLDER), pane) {
            Some(root) => tab.root = root,
            None => {
                self.take_tab(pos);
            }
        }
        Ok(surface)
    }

    /// Refuses, before anything changed, to put a tree `depth` deep beside
    /// the leaf `pane` of the tab at `pos` when the tab would then nest
    /// deeper than a client accepts.
    fn fits_beside(&self, pos: TabPos, pane: PaneId, depth: usize) -> Result<(), String> {
        let at = depth_of(&self.tab_at(pos).root, pane).ok_or_else(|| format!("no pane {pane}"))?;
        if at + depth > MAX_TREE_DEPTH {
            return Err(format!("panes nest at most {MAX_TREE_DEPTH} deep"));
        }
        Ok(())
    }

    /// Refuses a change that would leave more tabs or panes than a snapshot
    /// may carry: the codec rejects such a snapshot, which would cost every
    /// client its control exchange.
    fn room_for(&self, tabs: usize, panes: usize) -> Result<(), String> {
        if self.tabs().count() + tabs > MAX_TABS {
            return Err(format!("the workspace holds at most {MAX_TABS} tabs"));
        }
        let leaves: usize = self.tabs().map(|tab| tab.root.leaf_count()).sum();
        if leaves + panes > MAX_LEAVES {
            return Err(format!("the workspace holds at most {MAX_LEAVES} panes"));
        }
        Ok(())
    }

    /// Replaces the leaf `pane` of the tab at `pos` with an even split of it
    /// and `subtree`, `subtree` first when `first`.
    fn put_beside(
        &mut self,
        pos: TabPos,
        pane: PaneId,
        axis: Axis,
        subtree: PaneNode,
        first: bool,
    ) {
        let id = SplitId(self.next_split);
        self.next_split += 1;
        let tab = self.tab_at_mut(pos);
        let root = std::mem::replace(&mut tab.root, PLACEHOLDER);
        tab.root = split_leaf(root, pane, id, axis, subtree, first);
    }

    /// A terminal that just lost its pane is killed unless the runner owns
    /// it: an owned one goes back to the detached list instead.
    fn detach_or_kill(&mut self, terminals: impl Iterator<Item = TerminalId>) -> Applied {
        let mut killed = Vec::new();
        for terminal in terminals {
            if self.owned.contains(&terminal) {
                self.detached.insert(terminal);
            } else {
                killed.push(terminal);
            }
        }
        Applied { killed }
    }

    /// A tab of one fresh pane showing `surface`.
    fn new_tab(&mut self, surface: SurfaceRef) -> Tab {
        Tab {
            id: self.mint_tab(),
            title: None,
            accent: None,
            root: self.new_leaf(surface),
        }
    }

    fn new_leaf(&mut self, surface: SurfaceRef) -> PaneNode {
        let pane_id = PaneId(self.next_pane);
        self.next_pane += 1;
        PaneNode::Leaf { pane_id, surface }
    }

    fn mint_tab(&mut self) -> TabId {
        let id = TabId(self.next_tab);
        self.next_tab += 1;
        id
    }
}

fn terminal_of(surface: SurfaceRef) -> Option<TerminalId> {
    match surface {
        SurfaceRef::Terminal(terminal) => Some(terminal),
        _ => None,
    }
}

/// What a tab without an override is named after: its first surface that is
/// not empty.
fn title_source(root: &PaneNode) -> Option<SurfaceRef> {
    match root {
        PaneNode::Leaf {
            surface: SurfaceRef::Empty,
            ..
        } => None,
        PaneNode::Leaf { surface, .. } => Some(*surface),
        PaneNode::Split { first, second, .. } => {
            title_source(first).or_else(|| title_source(second))
        }
    }
}

/// A tab without an override is named after what it shows: the first
/// terminal's title, or the graph's name.
fn default_title(
    root: &PaneNode,
    title_of: &dyn Fn(TerminalId) -> Option<String>,
    graph_title: &dyn Fn(u64) -> Option<String>,
) -> String {
    match title_source(root) {
        Some(SurfaceRef::Graph(graph)) => graph_title(graph)
            .filter(|t| !t.is_empty())
            .map(|mut title| {
                // A node's name is the document's and unbounded; the wire's is
                // not.
                if title.len() > MAX_TITLE_BYTES {
                    let mut end = MAX_TITLE_BYTES;
                    while !title.is_char_boundary(end) {
                        end -= 1;
                    }
                    title.truncate(end);
                }
                title
            })
            .unwrap_or_else(|| "Graph".to_owned()),
        Some(SurfaceRef::Terminal(t)) => title_of(t)
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "Terminal".to_owned()),
        Some(SurfaceRef::Empty) | None => "Empty".to_owned(),
    }
}

/// How deep the leaf `pane` sits under `node`, the node itself being 1.
fn depth_of(node: &PaneNode, pane: PaneId) -> Option<usize> {
    match node {
        PaneNode::Leaf { pane_id, .. } => (*pane_id == pane).then_some(1),
        PaneNode::Split { first, second, .. } => depth_of(first, pane)
            .or_else(|| depth_of(second, pane))
            .map(|depth| depth + 1),
    }
}

/// Replaces the leaf `pane` with an even split of it and `subtree`, the
/// leaf first unless `subtree_first`.
fn split_leaf(
    node: PaneNode,
    pane: PaneId,
    id: SplitId,
    axis: Axis,
    subtree: PaneNode,
    subtree_first: bool,
) -> PaneNode {
    match node {
        PaneNode::Leaf { pane_id, .. } if pane_id == pane => {
            let (first, second) = if subtree_first {
                (subtree, node)
            } else {
                (node, subtree)
            };
            PaneNode::Split {
                id,
                axis,
                ratio: 0.5,
                first: Box::new(first),
                second: Box::new(second),
            }
        }
        PaneNode::Leaf { .. } => node,
        PaneNode::Split {
            id: own,
            axis: own_axis,
            ratio,
            first,
            second,
        } => {
            if first.find(pane).is_some() {
                PaneNode::Split {
                    id: own,
                    axis: own_axis,
                    ratio,
                    first: Box::new(split_leaf(*first, pane, id, axis, subtree, subtree_first)),
                    second,
                }
            } else {
                PaneNode::Split {
                    id: own,
                    axis: own_axis,
                    ratio,
                    first,
                    second: Box::new(split_leaf(*second, pane, id, axis, subtree, subtree_first)),
                }
            }
        }
    }
}

/// Removes the leaf `pane`; its parent split collapses into the sibling.
/// `None` when the node itself is the leaf and has no sibling.
pub(super) fn remove_leaf(node: PaneNode, pane: PaneId) -> Option<PaneNode> {
    match node {
        PaneNode::Leaf { pane_id, .. } if pane_id == pane => None,
        PaneNode::Leaf { .. } => Some(node),
        PaneNode::Split {
            id,
            axis,
            ratio,
            first,
            second,
        } => {
            if matches!(*first, PaneNode::Leaf { pane_id, .. } if pane_id == pane) {
                return Some(*second);
            }
            if matches!(*second, PaneNode::Leaf { pane_id, .. } if pane_id == pane) {
                return Some(*first);
            }
            let first =
                remove_leaf(*first, pane).expect("a split's child is never the leaf itself");
            let second =
                remove_leaf(*second, pane).expect("a split's child is never the leaf itself");
            Some(PaneNode::Split {
                id,
                axis,
                ratio,
                first: Box::new(first),
                second: Box::new(second),
            })
        }
    }
}

fn set_ratio(node: &mut PaneNode, split: SplitId, ratio: f32) -> bool {
    match node {
        PaneNode::Leaf { .. } => false,
        PaneNode::Split {
            id,
            ratio: own,
            first,
            second,
            ..
        } => {
            if *id == split {
                *own = ratio;
                true
            } else {
                set_ratio(first, split, ratio) || set_ratio(second, split, ratio)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use zeughaus_mux::{Side, TabSlot};

    use super::*;

    fn spawner() -> impl FnMut(ProfileId) -> Result<TerminalId, String> {
        let mut next = 100;
        move |_| {
            next += 1;
            Ok(TerminalId(next))
        }
    }

    fn no_titles(_: TerminalId) -> Option<String> {
        None
    }

    fn no_names(_: u64) -> Option<String> {
        None
    }

    fn snap(ws: &Workspace) -> WorkspaceSnapshot {
        ws.snapshot(&no_titles, &no_names)
    }

    fn fresh() -> Workspace {
        Workspace::new(RunnerIncarnation::from_bytes([0; 16]))
    }

    /// A workspace with `n` terminal tabs, showing 101, 102, ...
    fn with_tabs(
        n: usize,
    ) -> (
        Workspace,
        impl FnMut(ProfileId) -> Result<TerminalId, String>,
    ) {
        let mut ws = fresh();
        let mut spawn = spawner();
        for _ in 0..n {
            apply(
                &mut ws,
                &mut spawn,
                TopologyCommand::NewTerminalTab {
                    profile: ProfileId::DEFAULT,
                },
            );
        }
        (ws, spawn)
    }

    fn apply(
        ws: &mut Workspace,
        spawn: &mut dyn FnMut(ProfileId) -> Result<TerminalId, String>,
        command: TopologyCommand,
    ) -> Applied {
        ws.apply(&command, spawn).expect("applied")
    }

    fn refusal(ws: &mut Workspace, command: TopologyCommand) -> String {
        let revision = ws.revision();
        let before = snap(ws);
        let reason = ws.apply(&command, &mut spawner()).expect_err("refused");
        assert_eq!(ws.revision(), revision, "a refusal is no revision");
        assert_eq!(snap(ws), before, "a refusal changes nothing");
        reason
    }

    fn pane_showing(ws: &Workspace, terminal: TerminalId) -> Option<PaneId> {
        snap(ws)
            .tabs()
            .flat_map(|tab| tab.root.leaves())
            .find(|(_, surface)| *surface == SurfaceRef::Terminal(terminal))
            .map(|(pane, _)| pane)
    }

    fn tab_showing(ws: &Workspace, terminal: TerminalId) -> TabId {
        snap(ws)
            .tabs()
            .find(|tab| tab.root.terminals().any(|t| t == terminal))
            .expect("a tab shows it")
            .id
    }

    fn detached_ids(ws: &Workspace) -> Vec<TerminalId> {
        snap(ws).detached.into_iter().map(|d| d.terminal).collect()
    }

    /// The top level as `T<first terminal>` per tab and `G[...]` per group.
    fn layout(ws: &Workspace) -> Vec<String> {
        let tab = |tab: &TabSnapshot| {
            format!(
                "T{}",
                tab.root.terminals().next().map_or(0, |terminal| terminal.0)
            )
        };
        snap(ws)
            .items
            .iter()
            .map(|item| match item {
                WorkspaceItem::Tab(t) => tab(t),
                WorkspaceItem::Group(group) => format!(
                    "G[{}]",
                    group.tabs.iter().map(tab).collect::<Vec<_>>().join(" ")
                ),
            })
            .collect()
    }

    fn new_group(ws: &mut Workspace) -> GroupId {
        apply(
            ws,
            &mut spawner(),
            TopologyCommand::NewGroup {
                name: "g".into(),
                color_rgba: [1, 2, 3, 4],
            },
        );
        snap(ws).groups().last().expect("the new group").id
    }

    fn move_tab(ws: &mut Workspace, terminal: u64, group: Option<GroupId>, index: u32) {
        let tab = tab_showing(ws, TerminalId(terminal));
        apply(
            ws,
            &mut spawner(),
            TopologyCommand::MoveTab {
                tab,
                to: TabSlot { group, index },
            },
        );
    }

    #[test]
    fn a_new_tab_and_a_split_grow_the_tree() {
        let (mut ws, mut spawn) = with_tabs(1);
        let snapshot = snap(&ws);
        assert_eq!(snapshot.tabs().count(), 1);
        assert_eq!(snapshot.revision, 2);
        let tab = snapshot.tabs().next().unwrap();
        assert_eq!(tab.title, "Terminal");
        let pane = tab.root.leaves()[0].0;
        apply(
            &mut ws,
            &mut spawn,
            TopologyCommand::SplitWithTerminal {
                pane,
                axis: Axis::Vertical,
                profile: ProfileId::DEFAULT,
            },
        );
        assert_eq!(snap(&ws).tabs().next().unwrap().root.leaf_count(), 2);
        assert_eq!(ws.terminals(), vec![TerminalId(101), TerminalId(102)]);
    }

    #[test]
    fn a_title_change_is_a_revision_only_where_a_snapshot_shows_it() {
        let (mut ws, mut spawn) = with_tabs(1);
        let snapshot = snap(&ws);
        let first = snapshot.tabs().next().unwrap();
        let (tab, pane) = (first.id, first.root.leaves()[0].0);
        apply(
            &mut ws,
            &mut spawn,
            TopologyCommand::SplitWithTerminal {
                pane,
                axis: Axis::Vertical,
                profile: ProfileId::DEFAULT,
            },
        );
        let named_after = TerminalId(101);
        let beside = TerminalId(102);
        let before = ws.revision();

        assert!(ws.title_changed(named_after));
        assert_eq!(ws.revision(), before + 1);
        assert!(
            !ws.title_changed(beside),
            "the tab is named after its first terminal only"
        );
        assert_eq!(ws.revision(), before + 1);

        apply(
            &mut ws,
            &mut spawn,
            TopologyCommand::RenameTab {
                tab,
                title: Some("work".into()),
            },
        );
        let renamed = ws.revision();
        assert!(
            !ws.title_changed(named_after),
            "an override hides the terminal's title"
        );
        assert_eq!(ws.revision(), renamed);
    }

    #[test]
    fn the_last_pane_and_tab_can_close() {
        let (mut ws, mut spawn) = with_tabs(1);
        let pane = pane_showing(&ws, TerminalId(101)).unwrap();
        let applied = apply(&mut ws, &mut spawn, TopologyCommand::ClosePane { pane });
        assert_eq!(applied.killed, vec![TerminalId(101)]);
        assert_eq!(snap(&ws).items, Vec::new());

        apply(&mut ws, &mut spawn, TopologyCommand::OpenGraph { graph: 7 });
        let tab = snap(&ws).tabs().next().unwrap().id;
        let applied = apply(&mut ws, &mut spawn, TopologyCommand::CloseTab { tab });
        assert!(applied.killed.is_empty());
        assert_eq!(snap(&ws).items, Vec::new());
    }

    #[test]
    fn closing_a_tab_kills_its_terminals() {
        let (mut ws, mut spawn) = with_tabs(1);
        let pane = pane_showing(&ws, TerminalId(101)).unwrap();
        apply(
            &mut ws,
            &mut spawn,
            TopologyCommand::SplitWithTerminal {
                pane,
                axis: Axis::Vertical,
                profile: ProfileId::DEFAULT,
            },
        );
        let tab = tab_showing(&ws, TerminalId(101));
        let applied = apply(&mut ws, &mut spawn, TopologyCommand::CloseTab { tab });
        assert_eq!(applied.killed, vec![TerminalId(101), TerminalId(102)]);
        assert_eq!(snap(&ws).tabs().count(), 0);
    }

    #[test]
    fn a_failed_spawn_changes_nothing() {
        let mut ws = fresh();
        let mut failing = |_: ProfileId| Err("no shell".to_owned());
        assert_eq!(
            ws.apply(
                &TopologyCommand::NewTerminalTab {
                    profile: ProfileId::DEFAULT
                },
                &mut failing
            ),
            Err("no shell".to_owned())
        );
        assert_eq!(ws.revision(), 1);
        assert_eq!(snap(&ws).tabs().count(), 0);
    }

    #[test]
    fn a_ratio_is_clamped_and_a_title_can_follow_the_graph() {
        let mut ws = fresh();
        let mut spawn = spawner();
        apply(&mut ws, &mut spawn, TopologyCommand::OpenGraph { graph: 7 });
        let graph_pane = snap(&ws).tabs().next().unwrap().root.leaves()[0].0;
        apply(
            &mut ws,
            &mut spawn,
            TopologyCommand::SplitWithTerminal {
                pane: graph_pane,
                axis: Axis::Horizontal,
                profile: ProfileId::DEFAULT,
            },
        );
        apply(
            &mut ws,
            &mut spawn,
            TopologyCommand::ResizeSplit {
                split: SplitId(1),
                ratio: 0.001,
            },
        );
        let snapshot = ws.snapshot(&|t| Some(format!("vim {t}")), &|g| {
            (g == 7).then(|| "Pipeline".to_owned())
        });
        let tab = snapshot.tabs().next().unwrap();
        assert!(matches!(tab.root, PaneNode::Split { ratio, .. } if ratio == MIN_RATIO));
        assert_eq!(
            tab.title, "Pipeline",
            "the graph comes first in tree order and is named after its node"
        );
        assert_eq!(
            snap(&ws).tabs().next().unwrap().title,
            "Graph",
            "a graph without a name"
        );
        let id = tab.id;
        apply(
            &mut ws,
            &mut spawn,
            TopologyCommand::RenameTab {
                tab: id,
                title: Some("work".into()),
            },
        );
        assert_eq!(snap(&ws).tabs().next().unwrap().title, "work");
        refusal(
            &mut ws,
            TopologyCommand::ResizeSplit {
                split: SplitId(9),
                ratio: 0.5,
            },
        );
    }

    #[test]
    fn an_owned_terminal_outlives_the_pane_that_showed_it() {
        let (mut ws, mut spawn) = with_tabs(1);
        let job = TerminalId(500);
        ws.add_owned(job, OwnedPlacement::Detached);
        assert_eq!(
            snap(&ws).detached,
            vec![DetachedTerminal {
                terminal: job,
                title: "500".to_owned(),
            }],
            "an untitled terminal is listed under its id"
        );

        let beside = pane_showing(&ws, TerminalId(101)).unwrap();
        apply(
            &mut ws,
            &mut spawn,
            TopologyCommand::AttachTerminal {
                terminal: job,
                target: AttachTarget::Split {
                    pane: beside,
                    axis: Axis::Horizontal,
                },
            },
        );
        assert!(
            detached_ids(&ws).is_empty(),
            "a shown terminal is not detached"
        );
        let pane = pane_showing(&ws, job).expect("the job is shown");

        let applied = apply(&mut ws, &mut spawn, TopologyCommand::ClosePane { pane });
        assert!(
            applied.killed.is_empty(),
            "closing a pane must not end the runner's own terminal"
        );
        assert_eq!(detached_ids(&ws), vec![job]);
    }

    #[test]
    fn closing_an_owned_terminal_kills_and_forgets_it() {
        let mut ws = fresh();
        let mut spawn = spawner();
        let shown = TerminalId(500);
        let unshown = TerminalId(501);
        ws.add_owned(shown, OwnedPlacement::Detached);
        ws.add_owned(unshown, OwnedPlacement::Detached);
        apply(
            &mut ws,
            &mut spawn,
            TopologyCommand::AttachTerminal {
                terminal: shown,
                target: AttachTarget::NewTab,
            },
        );

        let applied = apply(
            &mut ws,
            &mut spawn,
            TopologyCommand::CloseTerminal { terminal: shown },
        );
        assert_eq!(applied.killed, vec![shown]);
        assert!(pane_showing(&ws, shown).is_none(), "its pane went with it");
        assert_eq!(snap(&ws).tabs().count(), 0);

        let applied = apply(
            &mut ws,
            &mut spawn,
            TopologyCommand::CloseTerminal { terminal: unshown },
        );
        assert_eq!(applied.killed, vec![unshown]);
        assert!(detached_ids(&ws).is_empty());
        refusal(
            &mut ws,
            TopologyCommand::AttachTerminal {
                terminal: shown,
                target: AttachTarget::NewTab,
            },
        );
    }

    #[test]
    fn only_an_unshown_terminal_of_the_runners_can_be_attached() {
        let mut ws = fresh();
        let mut spawn = spawner();
        refusal(
            &mut ws,
            TopologyCommand::AttachTerminal {
                terminal: TerminalId(999),
                target: AttachTarget::NewTab,
            },
        );
        refusal(
            &mut ws,
            TopologyCommand::CloseTerminal {
                terminal: TerminalId(999),
            },
        );

        let job = TerminalId(500);
        ws.add_owned(job, OwnedPlacement::Detached);
        apply(
            &mut ws,
            &mut spawn,
            TopologyCommand::AttachTerminal {
                terminal: job,
                target: AttachTarget::NewTab,
            },
        );
        refusal(
            &mut ws,
            TopologyCommand::AttachTerminal {
                terminal: job,
                target: AttachTarget::NewTab,
            },
        );
        assert!(pane_showing(&ws, job).is_some());
    }

    #[test]
    fn a_tab_moves_within_into_and_out_of_a_group() {
        let (mut ws, _) = with_tabs(3);
        let group = new_group(&mut ws);
        assert_eq!(layout(&ws), ["T101", "T102", "T103", "G[]"]);

        // Within the top level: the index counts after the tab was taken
        // out, and one past the end appends.
        move_tab(&mut ws, 101, None, 2);
        assert_eq!(layout(&ws), ["T102", "T103", "T101", "G[]"]);
        move_tab(&mut ws, 102, None, u32::MAX);
        assert_eq!(layout(&ws), ["T103", "T101", "G[]", "T102"]);

        // Into the group, which the top-level tab before it leaves.
        move_tab(&mut ws, 103, Some(group), 0);
        assert_eq!(layout(&ws), ["T101", "G[T103]", "T102"]);
        move_tab(&mut ws, 102, Some(group), 0);
        assert_eq!(layout(&ws), ["T101", "G[T102 T103]"]);

        // Within the group.
        move_tab(&mut ws, 102, Some(group), 1);
        assert_eq!(layout(&ws), ["T101", "G[T103 T102]"]);

        // Out of it; the emptied group stays.
        move_tab(&mut ws, 103, None, 0);
        move_tab(&mut ws, 102, None, 99);
        assert_eq!(layout(&ws), ["T103", "T101", "G[]", "T102"]);

        let tab = tab_showing(&ws, TerminalId(101));
        let reason = refusal(
            &mut ws,
            TopologyCommand::MoveTab {
                tab,
                to: TabSlot {
                    group: Some(GroupId(99)),
                    index: 0,
                },
            },
        );
        assert_eq!(reason, "no group GroupId#99");
    }

    #[test]
    fn groups_move_and_dissolve_in_place() {
        let (mut ws, _) = with_tabs(3);
        let group = new_group(&mut ws);
        move_tab(&mut ws, 101, Some(group), 0);
        move_tab(&mut ws, 102, Some(group), 1);
        assert_eq!(layout(&ws), ["T103", "G[T101 T102]"]);

        apply(
            &mut ws,
            &mut spawner(),
            TopologyCommand::MoveGroup { group, index: 0 },
        );
        assert_eq!(layout(&ws), ["G[T101 T102]", "T103"]);

        apply(
            &mut ws,
            &mut spawner(),
            TopologyCommand::RenameGroup {
                group,
                name: "build".into(),
            },
        );
        apply(
            &mut ws,
            &mut spawner(),
            TopologyCommand::SetGroupColor {
                group,
                color_rgba: [9, 9, 9, 9],
            },
        );
        let snapshot = snap(&ws);
        let shown = snapshot.groups().next().unwrap();
        assert_eq!(
            (shown.name.as_str(), shown.color_rgba, shown.locked),
            ("build", [9, 9, 9, 9], false)
        );

        apply(
            &mut ws,
            &mut spawner(),
            TopologyCommand::DissolveGroup { group },
        );
        assert_eq!(layout(&ws), ["T101", "T102", "T103"]);
        refusal(&mut ws, TopologyCommand::DissolveGroup { group });
    }

    /// A tab of 101 merged beside the pane of 102.
    fn merged(side: Side) -> PaneNode {
        let (mut ws, _) = with_tabs(2);
        let pane = pane_showing(&ws, TerminalId(102)).unwrap();
        let tab = tab_showing(&ws, TerminalId(101));
        apply(
            &mut ws,
            &mut spawner(),
            TopologyCommand::MergeTab { tab, pane, side },
        );
        let snapshot = snap(&ws);
        assert_eq!(snapshot.tabs().count(), 1, "the merged tab is gone");
        snapshot.tabs().next().unwrap().root.clone()
    }

    #[test]
    fn a_merge_splits_on_the_side_it_names() {
        for (side, axis, first) in [
            (Side::Left, Axis::Horizontal, 101),
            (Side::Right, Axis::Horizontal, 102),
            (Side::Top, Axis::Vertical, 101),
            (Side::Bottom, Axis::Vertical, 102),
        ] {
            let PaneNode::Split {
                axis: got,
                ratio,
                first: got_first,
                ..
            } = merged(side)
            else {
                panic!("{side:?}: not a split");
            };
            assert_eq!(got, axis, "{side:?}");
            assert_eq!(ratio, 0.5, "{side:?}");
            assert_eq!(
                got_first.terminals().collect::<Vec<_>>(),
                vec![TerminalId(first)],
                "{side:?}"
            );
        }
    }

    #[test]
    fn a_tab_does_not_merge_into_itself() {
        let (mut ws, _) = with_tabs(1);
        let tab = tab_showing(&ws, TerminalId(101));
        let pane = pane_showing(&ws, TerminalId(101)).unwrap();
        let reason = refusal(
            &mut ws,
            TopologyCommand::MergeTab {
                tab,
                pane,
                side: Side::Left,
            },
        );
        assert_eq!(reason, "a tab cannot merge into itself");
    }

    #[test]
    fn a_moved_pane_keeps_its_id_and_takes_an_emptied_tab_with_it() {
        let (mut ws, mut spawn) = with_tabs(2);
        let pane = pane_showing(&ws, TerminalId(101)).unwrap();
        let beside = pane_showing(&ws, TerminalId(102)).unwrap();
        apply(
            &mut ws,
            &mut spawn,
            TopologyCommand::MovePane {
                pane,
                to: PaneTarget::Beside {
                    pane: beside,
                    side: Side::Right,
                },
            },
        );
        assert_eq!(layout(&ws), ["T102"], "the last leaf took its tab along");
        assert_eq!(pane_showing(&ws, TerminalId(101)), Some(pane));

        // Out again into a tab of its own, at the front.
        apply(
            &mut ws,
            &mut spawn,
            TopologyCommand::MovePane {
                pane,
                to: PaneTarget::NewTab(TabSlot {
                    group: None,
                    index: 0,
                }),
            },
        );
        assert_eq!(layout(&ws), ["T101", "T102"]);
        assert_eq!(pane_showing(&ws, TerminalId(101)), Some(pane));
        assert_eq!(
            snap(&ws).tabs().nth(1).unwrap().root.leaf_count(),
            1,
            "the split collapsed"
        );

        let reason = refusal(
            &mut ws,
            TopologyCommand::MovePane {
                pane,
                to: PaneTarget::Beside {
                    pane,
                    side: Side::Top,
                },
            },
        );
        assert_eq!(reason, "a pane cannot move beside itself");
    }

    #[test]
    fn triggered_runs_share_one_locked_group_that_nothing_moves_across() {
        let (mut ws, mut spawn) = with_tabs(1);
        ws.add_owned(TerminalId(500), OwnedPlacement::Triggered);
        ws.add_owned(TerminalId(501), OwnedPlacement::Triggered);
        assert_eq!(layout(&ws), ["T101", "G[T500 T501]"]);
        let locked = snap(&ws).groups().next().unwrap().clone();
        assert_eq!(
            (locked.name.as_str(), locked.color_rgba, locked.locked),
            ("Triggered", [128, 128, 128, 64], true)
        );
        assert!(detached_ids(&ws).is_empty(), "a triggered run is shown");

        let shell_tab = tab_showing(&ws, TerminalId(101));
        let shell_pane = pane_showing(&ws, TerminalId(101)).unwrap();
        let run_tab = tab_showing(&ws, TerminalId(500));
        let run_pane = pane_showing(&ws, TerminalId(500)).unwrap();
        let into = TabSlot {
            group: Some(locked.id),
            index: 0,
        };
        for command in [
            TopologyCommand::MoveTab {
                tab: shell_tab,
                to: into,
            },
            TopologyCommand::MoveTab {
                tab: run_tab,
                to: TabSlot {
                    group: None,
                    index: 0,
                },
            },
            TopologyCommand::MergeTab {
                tab: shell_tab,
                pane: run_pane,
                side: Side::Left,
            },
            TopologyCommand::MergeTab {
                tab: run_tab,
                pane: shell_pane,
                side: Side::Left,
            },
            TopologyCommand::MovePane {
                pane: run_pane,
                to: PaneTarget::NewTab(TabSlot {
                    group: None,
                    index: 0,
                }),
            },
            TopologyCommand::MovePane {
                pane: shell_pane,
                to: PaneTarget::NewTab(into),
            },
            TopologyCommand::MovePane {
                pane: shell_pane,
                to: PaneTarget::Beside {
                    pane: run_pane,
                    side: Side::Left,
                },
            },
        ] {
            assert_eq!(refusal(&mut ws, command), LOCKED_MOVE);
        }
        assert_eq!(
            refusal(&mut ws, TopologyCommand::DissolveGroup { group: locked.id }),
            "a locked group stays"
        );

        // Within the locked group a tab still moves.
        move_tab(&mut ws, 501, Some(locked.id), 0);
        assert_eq!(layout(&ws), ["T101", "G[T501 T500]"]);

        // Its tab is the run's: closing it kills the runner's own terminal.
        let applied = apply(
            &mut ws,
            &mut spawn,
            TopologyCommand::CloseTab { tab: run_tab },
        );
        assert_eq!(applied.killed, vec![TerminalId(500)]);
        assert!(!ws.knows(TerminalId(500)));
        assert!(detached_ids(&ws).is_empty());
        assert_eq!(layout(&ws), ["T101", "G[T501]"]);
    }

    #[test]
    fn a_hidden_run_leaves_the_locked_group_for_the_detached_list() {
        let (mut ws, mut spawn) = with_tabs(1);
        ws.add_owned(TerminalId(500), OwnedPlacement::Triggered);
        ws.add_owned(TerminalId(501), OwnedPlacement::Triggered);
        let before = ws.revision();
        assert!(ws.hide_owned(TerminalId(500)));
        assert!(ws.revision() > before);
        assert_eq!(layout(&ws), ["T101", "G[T501]"]);
        assert_eq!(detached_ids(&ws), vec![TerminalId(500)]);
        assert!(!ws.hide_owned(TerminalId(500)), "already hidden");

        // Attached somewhere by hand, it stays where the client put it; a
        // terminal the runner does not own is never touched.
        apply(
            &mut ws,
            &mut spawn,
            TopologyCommand::AttachTerminal {
                terminal: TerminalId(500),
                target: AttachTarget::NewTab,
            },
        );
        assert!(!ws.hide_owned(TerminalId(500)));
        assert!(!ws.hide_owned(TerminalId(101)));
        assert_eq!(layout(&ws), ["T101", "G[T501]", "T500"]);
    }

    #[test]
    fn a_locked_group_takes_no_new_panes() {
        let mut ws = fresh();
        ws.add_owned(TerminalId(500), OwnedPlacement::Triggered);
        ws.add_owned(TerminalId(600), OwnedPlacement::Detached);
        let run_pane = pane_showing(&ws, TerminalId(500)).unwrap();
        let split = || TopologyCommand::SplitWithTerminal {
            pane: run_pane,
            axis: Axis::Vertical,
            profile: ProfileId::DEFAULT,
        };
        assert_eq!(refusal(&mut ws, split()), LOCKED_SPLIT);
        // Refused before a terminal exists: a spawn would have failed with
        // its own reason.
        let mut failing = |_| Err("spawned".to_owned());
        assert_eq!(
            ws.apply(&split(), &mut failing),
            Err(LOCKED_SPLIT.to_owned())
        );

        let attach = TopologyCommand::AttachTerminal {
            terminal: TerminalId(600),
            target: AttachTarget::Split {
                pane: run_pane,
                axis: Axis::Horizontal,
            },
        };
        assert_eq!(refusal(&mut ws, attach), LOCKED_SPLIT);
        assert_eq!(detached_ids(&ws), [TerminalId(600)]);
        assert_eq!(layout(&ws), ["G[T500]"]);
    }

    #[test]
    fn the_workspace_never_outgrows_a_snapshot() {
        let (mut ws, mut spawn) = with_tabs(MAX_TABS);
        let tabs_full = format!("the workspace holds at most {MAX_TABS} tabs");
        let new_tab = || TopologyCommand::NewTerminalTab {
            profile: ProfileId::DEFAULT,
        };
        assert_eq!(refusal(&mut ws, new_tab()), tabs_full);
        assert_eq!(
            refusal(&mut ws, TopologyCommand::OpenGraph { graph: 7 }),
            tabs_full
        );
        // One pane per tab: the panes are at their bound as well.
        let pane = pane_showing(&ws, TerminalId(101)).unwrap();
        assert_eq!(
            refusal(
                &mut ws,
                TopologyCommand::SplitWithTerminal {
                    pane,
                    axis: Axis::Vertical,
                    profile: ProfileId::DEFAULT,
                }
            ),
            format!("the workspace holds at most {MAX_LEAVES} panes")
        );

        // A triggered run without room waits detached.
        ws.add_owned(TerminalId(900), OwnedPlacement::Triggered);
        assert_eq!(detached_ids(&ws), [TerminalId(900)]);
        assert_eq!(snap(&ws).tabs().count(), MAX_TABS);

        // A graph without room gets no tab and is not taken as seen, so the
        // first sync with room shows it.
        let mut seen = BTreeSet::new();
        let update = sync(&[7], &[7], &[]);
        assert!(!ws.sync_graphs(&update, &HashMap::new(), &mut seen));
        assert!(graphs_shown(&ws).is_empty());
        assert!(!seen.contains(&7));

        let tab = tab_showing(&ws, TerminalId(101));
        apply(&mut ws, &mut spawn, TopologyCommand::CloseTab { tab });
        assert!(ws.sync_graphs(&update, &HashMap::new(), &mut seen));
        assert_eq!(graphs_shown(&ws), [7]);
        assert!(seen.contains(&7));
        assert_eq!(refusal(&mut ws, new_tab()), tabs_full);
    }

    fn sync(owned: &[u64], exists: &[u64], names: &[(u64, &str)]) -> GraphSync {
        GraphSync {
            owned: owned.to_vec(),
            names: names.iter().map(|(id, n)| (*id, n.to_string())).collect(),
            exists: exists.iter().copied().collect::<HashSet<_>>(),
        }
    }

    fn graphs_shown(ws: &Workspace) -> Vec<u64> {
        snap(ws)
            .tabs()
            .flat_map(|tab| tab.root.leaves())
            .filter_map(|(_, surface)| match surface {
                SurfaceRef::Graph(graph) => Some(graph),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn graph_panes_follow_the_document() {
        let mut ws = fresh();
        let mut seen = BTreeSet::new();
        let mut names = HashMap::new();
        let mut spawn = spawner();
        // A container opened for viewing before the first sync.
        apply(&mut ws, &mut spawn, TopologyCommand::OpenGraph { graph: 9 });

        let first = sync(&[1, 2], &[1, 2, 3, 9], &[(1, "One")]);
        assert!(ws.sync_graphs(&first, &names, &mut seen));
        names = first.names.clone();
        assert_eq!(graphs_shown(&ws), [9, 1, 2]);

        // Nothing new: no revision.
        let revision = ws.revision();
        assert!(!ws.sync_graphs(&first, &names, &mut seen));
        assert_eq!(ws.revision(), revision);

        // A graph whose tab was closed stays closed; a graph a pane already
        // shows gets no second one; a new one gets a tab; the deleted
        // container's pane goes, and its tab with it.
        let one = snap(&ws)
            .tabs()
            .find(|tab| tab.root.leaves()[0].1 == SurfaceRef::Graph(1))
            .unwrap()
            .id;
        apply(&mut ws, &mut spawn, TopologyCommand::CloseTab { tab: one });
        let second = sync(&[1, 2, 3, 4], &[1, 2, 3, 4], &[(1, "One")]);
        assert!(ws.sync_graphs(&second, &names, &mut seen));
        assert_eq!(graphs_shown(&ws), [2, 3, 4]);

        // A renamed graph is a new revision where a tab is named after it.
        let revision = ws.revision();
        let renamed = sync(&[1, 2, 3, 4], &[1, 2, 3, 4], &[(1, "One"), (2, "Two")]);
        assert!(ws.sync_graphs(&renamed, &second.names, &mut seen));
        assert_eq!(ws.revision(), revision + 1);
        let unshown = sync(&[1, 2, 3, 4], &[1, 2, 3, 4], &[(1, "Uno"), (2, "Two")]);
        assert!(!ws.sync_graphs(&unshown, &renamed.names, &mut seen));
    }

    /// A workspace with a graph tab, a tab split between 101 and 102, the
    /// runner's detached 200, its 201 shown in a tab of its own inside a
    /// group, and its triggered 202 in the locked group.
    fn saved_fixture() -> (Workspace, SavedWorkspace) {
        let (mut ws, mut spawn) = with_tabs(0);
        apply(&mut ws, &mut spawn, TopologyCommand::OpenGraph { graph: 7 });
        apply(
            &mut ws,
            &mut spawn,
            TopologyCommand::NewTerminalTab {
                profile: ProfileId::DEFAULT,
            },
        );
        let pane = pane_showing(&ws, TerminalId(101)).unwrap();
        apply(
            &mut ws,
            &mut spawn,
            TopologyCommand::SplitWithTerminal {
                pane,
                axis: Axis::Vertical,
                profile: ProfileId::DEFAULT,
            },
        );
        ws.add_owned(TerminalId(200), OwnedPlacement::Detached);
        ws.add_owned(TerminalId(201), OwnedPlacement::Detached);
        apply(
            &mut ws,
            &mut spawn,
            TopologyCommand::AttachTerminal {
                terminal: TerminalId(201),
                target: AttachTarget::NewTab,
            },
        );
        let group = new_group(&mut ws);
        move_tab(&mut ws, 201, Some(group), 0);
        ws.add_owned(TerminalId(202), OwnedPlacement::Triggered);
        let saved = ws.to_saved(300, Vec::new());
        (ws, saved)
    }

    #[test]
    fn a_restore_drops_the_panes_of_terminals_that_did_not_come_back() {
        let (before, saved) = saved_fixture();
        assert_eq!(layout(&before), ["T0", "T101", "G[T201]", "G[T202]"]);
        let alive = [101, 200, 201, 202].map(TerminalId);
        let mut ws = Workspace::restore(RunnerIncarnation::from_bytes([1; 16]), &saved, |t| {
            alive.contains(&t)
        });

        let snapshot = snap(&ws);
        let old = snap(&before);
        assert_eq!(snapshot.items.len(), old.items.len());
        assert_eq!(
            snapshot.tabs().map(|t| t.id).collect::<Vec<_>>(),
            old.tabs().map(|t| t.id).collect::<Vec<_>>(),
            "every tab kept a live pane and its id"
        );
        assert_eq!(graphs_shown(&ws), [7], "a graph pane stays");
        // The split lost 102 and collapsed into 101's leaf, same pane id.
        assert_eq!(
            snapshot.tabs().nth(1).unwrap().root,
            PaneNode::Leaf {
                pane_id: pane_showing(&before, TerminalId(101)).unwrap(),
                surface: SurfaceRef::Terminal(TerminalId(101)),
            }
        );
        assert_eq!(detached_ids(&ws), vec![TerminalId(200)]);
        assert!(pane_showing(&ws, TerminalId(201)).is_some());
        assert!(ws.knows(TerminalId(200)) && !ws.knows(TerminalId(102)));

        // Counters continue where they were: nothing new collides.
        apply(
            &mut ws,
            &mut spawner(),
            TopologyCommand::NewTerminalTab {
                profile: ProfileId::DEFAULT,
            },
        );
        let group = new_group(&mut ws);
        let snapshot = snap(&ws);
        let new_tab = snapshot.tabs().last().unwrap();
        assert!(old.tabs().all(|t| t.id != new_tab.id));
        let new_pane = new_tab.root.leaves()[0].0;
        assert!(
            old.tabs()
                .flat_map(|t| t.root.leaves())
                .all(|(p, _)| p != new_pane)
        );
        assert!(old.groups().all(|g| g.id != group));
    }

    #[test]
    fn a_restore_without_survivors_keeps_graphs_the_locked_group_and_nothing_else() {
        let (_, saved) = saved_fixture();
        let ws = Workspace::restore(RunnerIncarnation::from_bytes([1; 16]), &saved, |_| false);
        assert_eq!(layout(&ws), ["T0", "G[]"]);
        assert!(snap(&ws).groups().next().unwrap().locked);
        assert!(detached_ids(&ws).is_empty());
        assert!(!ws.knows(TerminalId(200)));
    }
}
