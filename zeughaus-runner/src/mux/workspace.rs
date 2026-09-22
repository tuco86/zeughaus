//! The shared workspace's structure, as the runner owns it.
//!
//! Pure: no sessions, no sockets. A [`TopologyCommand`] is applied to the
//! tree, the caller is told which terminals to create (through the `spawn`
//! callback, because a split needs its terminal's id before the tree can
//! hold it) and which to kill (returned, because the tree is updated first
//! and the kill must not fail it), and every applied command is one
//! revision. The invariants of the plan live here and nowhere else: exactly
//! one graph pane, never zero tabs, never an empty tab, split ratios kept
//! away from the edges.

use std::collections::BTreeSet;

use zeughaus_mux::workspace::{MIN_RATIO, ProfileId};
use zeughaus_mux::{
    AttachTarget, Axis, DetachedTerminal, PaneId, PaneNode, RunnerIncarnation, SplitId, SurfaceRef,
    TabId, TabSnapshot, TerminalId, TopologyCommand, WorkspaceSnapshot,
};

/// A tab as the runner keeps it: the user's title override is separate from
/// the title shown, which follows the terminal when there is no override.
#[derive(Debug, Clone)]
struct Tab {
    id: TabId,
    title: Option<String>,
    group: Option<String>,
    accent: Option<[u8; 4]>,
    root: PaneNode,
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
    tabs: Vec<Tab>,
    next_tab: u64,
    next_split: u64,
    next_pane: u64,
    /// Terminals the runner started for itself (a job's process). Their
    /// lifetime is the runner's, not a pane's: closing the pane that shows
    /// one detaches it, only `CloseTerminal` and the runner ends it.
    owned: BTreeSet<TerminalId>,
    /// The subset of `owned` no pane currently shows.
    detached: BTreeSet<TerminalId>,
}

impl Workspace {
    /// The workspace every runner starts with: one tab showing the graph.
    pub fn new(incarnation: RunnerIncarnation) -> Workspace {
        Workspace {
            incarnation,
            revision: 1,
            tabs: vec![Tab {
                id: TabId(1),
                title: None,
                group: None,
                accent: None,
                root: PaneNode::Leaf {
                    pane_id: PaneId(1),
                    surface: SurfaceRef::Graph,
                },
            }],
            next_tab: 2,
            next_split: 1,
            next_pane: 2,
            owned: BTreeSet::new(),
            detached: BTreeSet::new(),
        }
    }

    /// Takes ownership of a terminal the runner started itself. It is
    /// listed as detached until a client attaches it to a pane.
    pub fn add_owned(&mut self, terminal: TerminalId) {
        self.owned.insert(terminal);
        self.detached.insert(terminal);
        self.revision += 1;
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Every terminal any pane shows.
    #[cfg(test)]
    pub fn terminals(&self) -> Vec<TerminalId> {
        self.tabs
            .iter()
            .flat_map(|tab| tab.root.terminals().collect::<Vec<_>>())
            .collect()
    }

    /// The snapshot at the current revision. `title_of` resolves a
    /// terminal's current title for tabs without an override.
    pub fn snapshot(&self, title_of: &dyn Fn(TerminalId) -> Option<String>) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            incarnation: self.incarnation,
            revision: self.revision,
            tabs: self
                .tabs
                .iter()
                .map(|tab| TabSnapshot {
                    id: tab.id,
                    title: tab
                        .title
                        .clone()
                        .unwrap_or_else(|| default_title(&tab.root, title_of)),
                    group: tab.group.clone(),
                    accent_rgba: tab.accent,
                    root: tab.root.clone(),
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
    /// needs; a failure there refuses the command and changes nothing.
    /// `TakeControl`/`ReleaseControl` are not structural and are refused
    /// here: the service answers them from its lease table.
    pub fn apply(
        &mut self,
        command: &TopologyCommand,
        spawn: &mut dyn FnMut(ProfileId) -> Result<TerminalId, String>,
    ) -> Result<Applied, String> {
        let applied = match command {
            TopologyCommand::NewTerminalTab { profile } => {
                let terminal = spawn(*profile)?;
                let id = TabId(self.next_tab);
                self.next_tab += 1;
                let pane_id = self.mint_pane();
                self.tabs.push(Tab {
                    id,
                    title: None,
                    group: None,
                    accent: None,
                    root: PaneNode::Leaf {
                        pane_id,
                        surface: SurfaceRef::Terminal(terminal),
                    },
                });
                Applied::default()
            }
            TopologyCommand::SplitWithTerminal {
                pane,
                axis,
                profile,
            } => {
                let tab = self
                    .tabs
                    .iter()
                    .position(|tab| tab.root.find(*pane).is_some())
                    .ok_or_else(|| format!("no pane {pane}"))?;
                let terminal = spawn(*profile)?;
                let split = SplitId(self.next_split);
                self.next_split += 1;
                let pane_id = self.mint_pane();
                let new_leaf = PaneNode::Leaf {
                    pane_id,
                    surface: SurfaceRef::Terminal(terminal),
                };
                let root = std::mem::replace(
                    &mut self.tabs[tab].root,
                    PaneNode::Leaf {
                        pane_id: PaneId(0),
                        surface: SurfaceRef::Empty,
                    },
                );
                self.tabs[tab].root = split_leaf(root, *pane, split, *axis, new_leaf);
                Applied::default()
            }
            TopologyCommand::ClosePane { pane } => {
                let surface = self.remove_pane(*pane)?;
                self.detach_or_kill(terminal_of(surface).into_iter())
            }
            TopologyCommand::CloseTab { tab } => {
                let index = self
                    .tabs
                    .iter()
                    .position(|t| t.id == *tab)
                    .ok_or_else(|| format!("no tab {tab}"))?;
                if self.tabs.len() == 1 {
                    return Err("the last tab stays".to_owned());
                }
                let holds_graph = self.tabs[index]
                    .root
                    .leaves()
                    .iter()
                    .any(|(_, s)| *s == SurfaceRef::Graph);
                if holds_graph {
                    return Err("the tab with the graph stays".to_owned());
                }
                let removed = self.tabs.remove(index);
                self.detach_or_kill(removed.root.terminals())
            }
            TopologyCommand::ResizeSplit { split, ratio } => {
                if !ratio.is_finite() {
                    return Err("ratio is not a number".to_owned());
                }
                let ratio = ratio.clamp(MIN_RATIO, 1.0 - MIN_RATIO);
                let found = self
                    .tabs
                    .iter_mut()
                    .any(|tab| set_ratio(&mut tab.root, *split, ratio));
                if !found {
                    return Err(format!("no split {split}"));
                }
                Applied::default()
            }
            TopologyCommand::RenameTab { tab, title } => {
                self.tab_mut(*tab)?.title = title.clone().filter(|t| !t.trim().is_empty());
                Applied::default()
            }
            TopologyCommand::SetTabGroup { tab, group } => {
                self.tab_mut(*tab)?.group = group.clone().filter(|g| !g.trim().is_empty());
                Applied::default()
            }
            TopologyCommand::SetTabAccent { tab, accent_rgba } => {
                self.tab_mut(*tab)?.accent = *accent_rgba;
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
                        let id = TabId(self.next_tab);
                        self.next_tab += 1;
                        let pane_id = self.mint_pane();
                        self.tabs.push(Tab {
                            id,
                            title: None,
                            group: None,
                            accent: None,
                            root: PaneNode::Leaf {
                                pane_id,
                                surface: SurfaceRef::Terminal(*terminal),
                            },
                        });
                    }
                    AttachTarget::Split { pane, axis } => {
                        let tab = self
                            .tabs
                            .iter()
                            .position(|tab| tab.root.find(*pane).is_some())
                            .ok_or_else(|| format!("no pane {pane}"))?;
                        let split = SplitId(self.next_split);
                        self.next_split += 1;
                        let pane_id = self.mint_pane();
                        let new_leaf = PaneNode::Leaf {
                            pane_id,
                            surface: SurfaceRef::Terminal(*terminal),
                        };
                        let root = std::mem::replace(
                            &mut self.tabs[tab].root,
                            PaneNode::Leaf {
                                pane_id: PaneId(0),
                                surface: SurfaceRef::Empty,
                            },
                        );
                        self.tabs[tab].root = split_leaf(root, *pane, split, *axis, new_leaf);
                    }
                }
                self.detached.remove(terminal);
                Applied::default()
            }
            TopologyCommand::CloseTerminal { terminal } => {
                if !self.owned.contains(terminal) {
                    return Err(format!("{terminal} is not the runner's"));
                }
                // A shown terminal takes its pane with it, under the same
                // invariants a `ClosePane` obeys: if the pane cannot go,
                // neither can the terminal.
                if let Some(pane) = self.pane_of(*terminal) {
                    self.remove_pane(pane)?;
                }
                self.owned.remove(terminal);
                self.detached.remove(terminal);
                Applied {
                    killed: vec![*terminal],
                }
            }
        };
        self.revision += 1;
        Ok(applied)
    }

    fn tab_mut(&mut self, id: TabId) -> Result<&mut Tab, String> {
        self.tabs
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| format!("no tab {id}"))
    }

    /// The pane showing `terminal`, if one does.
    fn pane_of(&self, terminal: TerminalId) -> Option<PaneId> {
        self.tabs.iter().find_map(|tab| {
            tab.root
                .leaves()
                .into_iter()
                .find(|(_, surface)| *surface == SurfaceRef::Terminal(terminal))
                .map(|(pane, _)| pane)
        })
    }

    /// Takes the leaf `pane` out of its tab, dropping the tab with it when
    /// it was the last one, and returns what the pane showed. The graph
    /// pane and the last pane of the last tab stay.
    fn remove_pane(&mut self, pane: PaneId) -> Result<SurfaceRef, String> {
        let tab = self
            .tabs
            .iter()
            .position(|tab| tab.root.find(pane).is_some())
            .ok_or_else(|| format!("no pane {pane}"))?;
        let surface = match self.tabs[tab].root.find(pane) {
            Some(PaneNode::Leaf { surface, .. }) => *surface,
            _ => return Err(format!("no pane {pane}")),
        };
        if surface == SurfaceRef::Graph {
            return Err("the graph pane cannot be closed".to_owned());
        }
        if self.tabs[tab].root.leaf_count() == 1 {
            if self.tabs.len() == 1 {
                return Err("the last pane of the last tab stays".to_owned());
            }
            self.tabs.remove(tab);
        } else {
            let root = std::mem::replace(
                &mut self.tabs[tab].root,
                PaneNode::Leaf {
                    pane_id: PaneId(0),
                    surface: SurfaceRef::Empty,
                },
            );
            self.tabs[tab].root = remove_leaf(root, pane).expect("pane was found");
        }
        Ok(surface)
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

    fn mint_pane(&mut self) -> PaneId {
        let id = PaneId(self.next_pane);
        self.next_pane += 1;
        id
    }
}

fn terminal_of(surface: SurfaceRef) -> Option<TerminalId> {
    match surface {
        SurfaceRef::Terminal(terminal) => Some(terminal),
        _ => None,
    }
}

/// A tab without an override is named after what it shows: the first
/// terminal's title, or the graph.
fn default_title(root: &PaneNode, title_of: &dyn Fn(TerminalId) -> Option<String>) -> String {
    for (_, surface) in root.leaves() {
        match surface {
            SurfaceRef::Graph => return "Graph".to_owned(),
            SurfaceRef::Terminal(t) => {
                if let Some(title) = title_of(t).filter(|t| !t.is_empty()) {
                    return title;
                }
                return "Terminal".to_owned();
            }
            SurfaceRef::Empty => {}
        }
    }
    "Empty".to_owned()
}

/// Replaces the leaf `pane` with a split of it and `new_leaf`.
fn split_leaf(
    node: PaneNode,
    pane: PaneId,
    id: SplitId,
    axis: Axis,
    new_leaf: PaneNode,
) -> PaneNode {
    match node {
        PaneNode::Leaf { pane_id, .. } if pane_id == pane => PaneNode::Split {
            id,
            axis,
            ratio: 0.5,
            first: Box::new(node),
            second: Box::new(new_leaf),
        },
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
                    first: Box::new(split_leaf(*first, pane, id, axis, new_leaf)),
                    second,
                }
            } else {
                PaneNode::Split {
                    id: own,
                    axis: own_axis,
                    ratio,
                    first,
                    second: Box::new(split_leaf(*second, pane, id, axis, new_leaf)),
                }
            }
        }
    }
}

/// Removes the leaf `pane`; its parent split collapses into the sibling.
/// `None` when the node itself is the leaf and has no sibling.
fn remove_leaf(node: PaneNode, pane: PaneId) -> Option<PaneNode> {
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

    fn pane_showing(ws: &Workspace, terminal: TerminalId) -> Option<PaneId> {
        ws.snapshot(&no_titles)
            .tabs
            .iter()
            .flat_map(|tab| tab.root.leaves())
            .find(|(_, surface)| *surface == SurfaceRef::Terminal(terminal))
            .map(|(pane, _)| pane)
    }

    fn detached_ids(ws: &Workspace) -> Vec<TerminalId> {
        ws.snapshot(&no_titles)
            .detached
            .into_iter()
            .map(|d| d.terminal)
            .collect()
    }

    #[test]
    fn a_new_tab_and_a_split_grow_the_tree() {
        let mut ws = Workspace::new(RunnerIncarnation::from_bytes([0; 16]));
        let mut spawn = spawner();
        ws.apply(
            &TopologyCommand::NewTerminalTab {
                profile: ProfileId::DEFAULT,
            },
            &mut spawn,
        )
        .unwrap();
        let snap = ws.snapshot(&no_titles);
        assert_eq!(snap.tabs.len(), 2);
        assert_eq!(snap.revision, 2);
        assert_eq!(snap.tabs[1].title, "Terminal");
        let pane = snap.tabs[1].root.leaves()[0].0;
        ws.apply(
            &TopologyCommand::SplitWithTerminal {
                pane,
                axis: Axis::Vertical,
                profile: ProfileId::DEFAULT,
            },
            &mut spawn,
        )
        .unwrap();
        let snap = ws.snapshot(&no_titles);
        assert_eq!(snap.tabs[1].root.leaf_count(), 2);
        assert!(snap.has_one_graph());
        assert_eq!(ws.terminals(), vec![TerminalId(101), TerminalId(102)]);
    }

    #[test]
    fn the_graph_pane_and_the_last_tab_stay() {
        let mut ws = Workspace::new(RunnerIncarnation::from_bytes([0; 16]));
        let mut spawn = spawner();
        assert!(
            ws.apply(&TopologyCommand::ClosePane { pane: PaneId(1) }, &mut spawn)
                .is_err()
        );
        assert!(
            ws.apply(&TopologyCommand::CloseTab { tab: TabId(1) }, &mut spawn)
                .is_err()
        );
        // Splitting the graph pane with a terminal, then closing the
        // terminal, leaves the graph alone again.
        ws.apply(
            &TopologyCommand::SplitWithTerminal {
                pane: PaneId(1),
                axis: Axis::Horizontal,
                profile: ProfileId::DEFAULT,
            },
            &mut spawn,
        )
        .unwrap();
        let applied = ws
            .apply(&TopologyCommand::ClosePane { pane: PaneId(2) }, &mut spawn)
            .unwrap();
        assert_eq!(applied.killed, vec![TerminalId(101)]);
        let snap = ws.snapshot(&no_titles);
        assert!(matches!(
            snap.tabs[0].root,
            PaneNode::Leaf {
                surface: SurfaceRef::Graph,
                ..
            }
        ));
        assert_eq!(ws.revision(), 3);
    }

    #[test]
    fn closing_a_tab_kills_its_terminals() {
        let mut ws = Workspace::new(RunnerIncarnation::from_bytes([0; 16]));
        let mut spawn = spawner();
        ws.apply(
            &TopologyCommand::NewTerminalTab {
                profile: ProfileId::DEFAULT,
            },
            &mut spawn,
        )
        .unwrap();
        let pane = ws.snapshot(&no_titles).tabs[1].root.leaves()[0].0;
        ws.apply(
            &TopologyCommand::SplitWithTerminal {
                pane,
                axis: Axis::Vertical,
                profile: ProfileId::DEFAULT,
            },
            &mut spawn,
        )
        .unwrap();
        let applied = ws
            .apply(&TopologyCommand::CloseTab { tab: TabId(2) }, &mut spawn)
            .unwrap();
        assert_eq!(applied.killed, vec![TerminalId(101), TerminalId(102)]);
        assert_eq!(ws.snapshot(&no_titles).tabs.len(), 1);
    }

    #[test]
    fn a_failed_spawn_changes_nothing() {
        let mut ws = Workspace::new(RunnerIncarnation::from_bytes([0; 16]));
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
        assert_eq!(ws.snapshot(&no_titles).tabs.len(), 1);
    }

    #[test]
    fn a_ratio_is_clamped_and_a_title_can_follow_the_terminal() {
        let mut ws = Workspace::new(RunnerIncarnation::from_bytes([0; 16]));
        let mut spawn = spawner();
        ws.apply(
            &TopologyCommand::SplitWithTerminal {
                pane: PaneId(1),
                axis: Axis::Horizontal,
                profile: ProfileId::DEFAULT,
            },
            &mut spawn,
        )
        .unwrap();
        ws.apply(
            &TopologyCommand::ResizeSplit {
                split: SplitId(1),
                ratio: 0.001,
            },
            &mut spawn,
        )
        .unwrap();
        let snap = ws.snapshot(&|t| Some(format!("vim {t}")));
        assert!(matches!(snap.tabs[0].root, PaneNode::Split { ratio, .. } if ratio == MIN_RATIO));
        assert_eq!(
            snap.tabs[0].title, "Graph",
            "the graph comes first in tree order"
        );
        ws.apply(
            &TopologyCommand::RenameTab {
                tab: TabId(1),
                title: Some("work".into()),
            },
            &mut spawn,
        )
        .unwrap();
        assert_eq!(ws.snapshot(&no_titles).tabs[0].title, "work");
        assert!(
            ws.apply(
                &TopologyCommand::ResizeSplit {
                    split: SplitId(9),
                    ratio: 0.5
                },
                &mut spawn
            )
            .is_err()
        );
    }

    #[test]
    fn an_owned_terminal_outlives_the_pane_that_showed_it() {
        let mut ws = Workspace::new(RunnerIncarnation::from_bytes([0; 16]));
        let mut spawn = spawner();
        let job = TerminalId(500);
        ws.add_owned(job);
        assert_eq!(
            ws.snapshot(&no_titles).detached,
            vec![DetachedTerminal {
                terminal: job,
                title: "500".to_owned(),
            }],
            "an untitled terminal is listed under its id"
        );

        ws.apply(
            &TopologyCommand::AttachTerminal {
                terminal: job,
                target: AttachTarget::Split {
                    pane: PaneId(1),
                    axis: Axis::Horizontal,
                },
            },
            &mut spawn,
        )
        .unwrap();
        assert!(
            detached_ids(&ws).is_empty(),
            "a shown terminal is not detached"
        );
        let pane = pane_showing(&ws, job).expect("the job is shown");

        let applied = ws
            .apply(&TopologyCommand::ClosePane { pane }, &mut spawn)
            .unwrap();
        assert!(
            applied.killed.is_empty(),
            "closing a pane must not end the runner's own terminal"
        );
        assert_eq!(detached_ids(&ws), vec![job]);
    }

    #[test]
    fn closing_an_owned_terminal_kills_and_forgets_it() {
        let mut ws = Workspace::new(RunnerIncarnation::from_bytes([0; 16]));
        let mut spawn = spawner();
        let shown = TerminalId(500);
        let unshown = TerminalId(501);
        ws.add_owned(shown);
        ws.add_owned(unshown);
        ws.apply(
            &TopologyCommand::AttachTerminal {
                terminal: shown,
                target: AttachTarget::NewTab,
            },
            &mut spawn,
        )
        .unwrap();

        let applied = ws
            .apply(
                &TopologyCommand::CloseTerminal { terminal: shown },
                &mut spawn,
            )
            .unwrap();
        assert_eq!(applied.killed, vec![shown]);
        assert!(pane_showing(&ws, shown).is_none(), "its pane went with it");
        assert_eq!(ws.snapshot(&no_titles).tabs.len(), 1);

        let applied = ws
            .apply(
                &TopologyCommand::CloseTerminal { terminal: unshown },
                &mut spawn,
            )
            .unwrap();
        assert_eq!(applied.killed, vec![unshown]);
        assert!(detached_ids(&ws).is_empty());
        assert!(
            ws.apply(
                &TopologyCommand::AttachTerminal {
                    terminal: shown,
                    target: AttachTarget::NewTab,
                },
                &mut spawn,
            )
            .is_err(),
            "a closed terminal is forgotten"
        );
    }

    #[test]
    fn only_an_unshown_terminal_of_the_runners_can_be_attached() {
        let mut ws = Workspace::new(RunnerIncarnation::from_bytes([0; 16]));
        let mut spawn = spawner();
        assert!(
            ws.apply(
                &TopologyCommand::AttachTerminal {
                    terminal: TerminalId(999),
                    target: AttachTarget::NewTab,
                },
                &mut spawn,
            )
            .is_err(),
            "a terminal the runner does not own cannot be attached"
        );
        assert!(
            ws.apply(
                &TopologyCommand::CloseTerminal {
                    terminal: TerminalId(999),
                },
                &mut spawn,
            )
            .is_err()
        );

        let job = TerminalId(500);
        ws.add_owned(job);
        ws.apply(
            &TopologyCommand::AttachTerminal {
                terminal: job,
                target: AttachTarget::NewTab,
            },
            &mut spawn,
        )
        .unwrap();
        let revision = ws.revision();
        assert!(
            ws.apply(
                &TopologyCommand::AttachTerminal {
                    terminal: job,
                    target: AttachTarget::NewTab,
                },
                &mut spawn,
            )
            .is_err(),
            "a terminal shows in one pane at a time"
        );
        assert_eq!(ws.revision(), revision, "a refusal changes nothing");
        assert!(pane_showing(&ws, job).is_some());
    }
}
