//! The shared workspace: tabs, each a tree of splits ending in surfaces.
//!
//! The runner owns this structure and every client sees the same one. What a
//! client owns -- which tab is active, which pane is focused, where the tab
//! bar sits -- is not here and never travels. A change to the shared part is a
//! [`TopologyCommand`] answered with the next [`WorkspaceSnapshot`]: whole
//! snapshots rather than patches, because a snapshot cannot be applied out
//! of order and there is one per revision, so a client that missed one
//! simply takes the newer.

use serde::{Deserialize, Serialize};

use crate::id::{PaneId, RunnerIncarnation, SplitId, TabId, TerminalId};

/// Everything shared about the workspace, at one revision.
///
/// `revision` increases by one per applied command. A client compares it to
/// what it holds and takes the newer; equal revisions are the same snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub incarnation: RunnerIncarnation,
    pub revision: u64,
    pub tabs: Vec<TabSnapshot>,
    /// Terminals the runner owns whose lifetime is not a pane's (jobs) and
    /// that no pane currently shows. A client lists them and shows one with
    /// [`TopologyCommand::AttachTerminal`]; closing its pane again puts it
    /// back here rather than killing it.
    pub detached: Vec<DetachedTerminal>,
}

/// One owned terminal no pane shows, with the title a client lists it under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetachedTerminal {
    pub terminal: TerminalId,
    pub title: String,
}

impl WorkspaceSnapshot {
    /// Every terminal referenced by any pane, in tree order.
    pub fn terminals(&self) -> impl Iterator<Item = TerminalId> + '_ {
        self.tabs.iter().flat_map(|tab| tab.root.terminals())
    }

    /// The tab holding `pane`, if any.
    pub fn tab_of(&self, pane: PaneId) -> Option<&TabSnapshot> {
        self.tabs.iter().find(|tab| tab.root.find(pane).is_some())
    }

    /// Whether exactly one pane shows the graph: the invariant the runner
    /// keeps and a client may assert.
    pub fn has_one_graph(&self) -> bool {
        self.tabs
            .iter()
            .flat_map(|tab| tab.root.leaves())
            .filter(|(_, surface)| *surface == SurfaceRef::Graph)
            .count()
            == 1
    }
}

/// One tab: a title, an optional group and accent for the tab bar, and the
/// tree of panes it shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabSnapshot {
    pub id: TabId,
    /// The title as shown; a terminal tab without a user override follows
    /// its terminal's OSC title, which the runner resolves before sending.
    pub title: String,
    pub group: Option<String>,
    /// RGBA, straight alpha. `None` draws the theme's default.
    pub accent_rgba: Option<[u8; 4]>,
    pub root: PaneNode,
}

/// A pane tree. Splits carry the ratio of the first child, `0.0..=1.0`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PaneNode {
    Split {
        id: SplitId,
        axis: Axis,
        /// Share of the first child along `axis`, clamped by the runner to
        /// `MIN_RATIO..=1 - MIN_RATIO`.
        ratio: f32,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
    Leaf {
        pane_id: PaneId,
        surface: SurfaceRef,
    },
}

/// A split ratio is kept away from the edges so no pane can be squeezed to
/// nothing and lost from view.
pub const MIN_RATIO: f32 = 0.05;

impl PaneNode {
    /// Every leaf, depth-first, first child before second.
    pub fn leaves(&self) -> Vec<(PaneId, SurfaceRef)> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<(PaneId, SurfaceRef)>) {
        match self {
            PaneNode::Split { first, second, .. } => {
                first.collect_leaves(out);
                second.collect_leaves(out);
            }
            PaneNode::Leaf { pane_id, surface } => out.push((*pane_id, *surface)),
        }
    }

    /// Every terminal shown by a leaf under this node.
    pub fn terminals(&self) -> impl Iterator<Item = TerminalId> {
        self.leaves().into_iter().filter_map(|(_, s)| match s {
            SurfaceRef::Terminal(t) => Some(t),
            _ => None,
        })
    }

    /// The leaf with `pane`, if it is under this node.
    pub fn find(&self, pane: PaneId) -> Option<&PaneNode> {
        match self {
            PaneNode::Leaf { pane_id, .. } if *pane_id == pane => Some(self),
            PaneNode::Leaf { .. } => None,
            PaneNode::Split { first, second, .. } => first.find(pane).or_else(|| second.find(pane)),
        }
    }

    /// The split with `id`, if it is under this node.
    pub fn find_split(&self, id: SplitId) -> Option<&PaneNode> {
        match self {
            PaneNode::Leaf { .. } => None,
            PaneNode::Split {
                id: own,
                first,
                second,
                ..
            } => {
                if *own == id {
                    Some(self)
                } else {
                    first.find_split(id).or_else(|| second.find_split(id))
                }
            }
        }
    }

    /// Number of leaves.
    pub fn leaf_count(&self) -> usize {
        match self {
            PaneNode::Leaf { .. } => 1,
            PaneNode::Split { first, second, .. } => first.leaf_count() + second.leaf_count(),
        }
    }

    /// Depth of the deepest leaf: a bound the codec checks so a hostile tree
    /// cannot recurse a client into a stack overflow.
    pub fn depth(&self) -> usize {
        match self {
            PaneNode::Leaf { .. } => 1,
            PaneNode::Split { first, second, .. } => 1 + first.depth().max(second.depth()),
        }
    }
}

/// Deepest tree either side accepts.
pub const MAX_TREE_DEPTH: usize = 32;

/// Which way a split divides its space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Axis {
    /// Children side by side, divider vertical.
    Horizontal,
    /// Children stacked, divider horizontal.
    Vertical,
}

/// What a pane shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SurfaceRef {
    /// The node graph. Exactly one leaf shows it, because the editor's
    /// graph camera and selection are global state.
    Graph,
    /// Nothing, transiently: a leaf whose terminal could not be created, or
    /// a reconstruction that lost its surface. Never the result of a split.
    Empty,
    Terminal(TerminalId),
}

/// A change to the shared structure, sent on the control exchange and
/// answered with a [`crate::CommandReply`] and the next snapshot.
///
/// Terminals are only ever created from a runner-configured profile named by
/// id: no command carries an argv, an environment or a working directory.
/// [`TopologyCommand::AttachTerminal`] is no exception -- it shows a
/// terminal the runner already created for itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TopologyCommand {
    /// A new tab whose single pane is a fresh terminal.
    NewTerminalTab {
        profile: ProfileId,
    },
    /// Split `pane`; the new sibling is a fresh terminal.
    SplitWithTerminal {
        pane: PaneId,
        axis: Axis,
        profile: ProfileId,
    },
    /// Remove a pane. A terminal pane's child is killed. The graph pane
    /// cannot be closed, nor the last pane of the last tab.
    ClosePane {
        pane: PaneId,
    },
    /// Remove a tab and kill every terminal in it. The last tab cannot be
    /// closed; the tab holding the graph pane cannot either.
    CloseTab {
        tab: TabId,
    },
    ResizeSplit {
        split: SplitId,
        ratio: f32,
    },
    RenameTab {
        tab: TabId,
        title: Option<String>,
    },
    SetTabGroup {
        tab: TabId,
        group: Option<String>,
    },
    SetTabAccent {
        tab: TabId,
        accent_rgba: Option<[u8; 4]>,
    },
    /// Become the controller of `terminal`, revoking whoever holds it.
    TakeControl {
        terminal: TerminalId,
    },
    /// Give up control of `terminal` if this client holds it.
    ReleaseControl {
        terminal: TerminalId,
    },
    /// Show a terminal the runner owns in a new pane. Refused if it is
    /// already shown, unknown, or not one of the runner's own.
    AttachTerminal {
        terminal: TerminalId,
        target: AttachTarget,
    },
    /// Kill a terminal the runner owns, shown or not, and forget it. The
    /// pane showing it, if any, closes with it.
    CloseTerminal {
        terminal: TerminalId,
    },
}

/// Where [`TopologyCommand::AttachTerminal`] puts the pane it creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachTarget {
    NewTab,
    Split { pane: PaneId, axis: Axis },
}

/// A runner-side shell profile, by id. `0` is the runner's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProfileId(pub u32);

impl ProfileId {
    pub const DEFAULT: ProfileId = ProfileId(0);
}

/// Longest tab title, group name or terminal title accepted on the wire.
pub const MAX_TITLE_BYTES: usize = 512;

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> PaneNode {
        PaneNode::Split {
            id: SplitId(1),
            axis: Axis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneNode::Leaf {
                pane_id: PaneId(1),
                surface: SurfaceRef::Graph,
            }),
            second: Box::new(PaneNode::Split {
                id: SplitId(2),
                axis: Axis::Vertical,
                ratio: 0.3,
                first: Box::new(PaneNode::Leaf {
                    pane_id: PaneId(2),
                    surface: SurfaceRef::Terminal(TerminalId(10)),
                }),
                second: Box::new(PaneNode::Leaf {
                    pane_id: PaneId(3),
                    surface: SurfaceRef::Terminal(TerminalId(11)),
                }),
            }),
        }
    }

    #[test]
    fn leaves_come_in_tree_order() {
        let ids: Vec<_> = tree().leaves().into_iter().map(|(p, _)| p).collect();
        assert_eq!(ids, [PaneId(1), PaneId(2), PaneId(3)]);
        assert_eq!(
            tree().terminals().collect::<Vec<_>>(),
            [TerminalId(10), TerminalId(11)]
        );
        assert_eq!(tree().depth(), 3);
        assert_eq!(tree().leaf_count(), 3);
    }

    #[test]
    fn a_pane_and_a_split_can_be_found() {
        let t = tree();
        assert!(matches!(
            t.find(PaneId(3)),
            Some(PaneNode::Leaf {
                pane_id: PaneId(3),
                ..
            })
        ));
        assert!(t.find(PaneId(9)).is_none());
        assert!(
            matches!(t.find_split(SplitId(2)), Some(PaneNode::Split { ratio, .. }) if *ratio == 0.3)
        );
    }

    #[test]
    fn one_graph_is_the_invariant() {
        let snapshot = WorkspaceSnapshot {
            incarnation: RunnerIncarnation::from_bytes([0; 16]),
            revision: 1,
            tabs: vec![TabSnapshot {
                id: TabId(1),
                title: "one".into(),
                group: None,
                accent_rgba: None,
                root: tree(),
            }],
            detached: Vec::new(),
        };
        assert!(snapshot.has_one_graph());
        assert_eq!(snapshot.tab_of(PaneId(2)).map(|t| t.id), Some(TabId(1)));
    }
}
