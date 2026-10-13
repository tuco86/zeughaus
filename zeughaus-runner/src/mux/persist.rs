//! What a restarted runner needs to bring its terminals back: the workspace
//! structure, the id counters, and per terminal what its shim does not
//! know. Written to `<state-dir>/workspace.json` whenever the structure
//! changes, read once at start.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zeughaus_mux::{
    Axis, Dimensions, GroupId, PaneId, PaneNode, SplitId, SurfaceRef, TabId, TerminalId,
};

use super::workspace::remove_leaf;

/// Bumped when a field changes meaning. The previous version is migrated,
/// because its file names the user's live shells; any other version is
/// ignored and the runner starts with a fresh workspace.
pub const VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedWorkspace {
    pub version: u32,
    /// The top level in display order.
    pub items: Vec<SavedItem>,
    pub next_tab: u64,
    pub next_group: u64,
    pub next_split: u64,
    pub next_pane: u64,
    pub next_terminal: u64,
    pub owned: Vec<TerminalId>,
    pub detached: Vec<TerminalId>,
    pub terminals: Vec<SavedTerminal>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SavedItem {
    Tab(SavedTab),
    Group(SavedGroup),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedGroup {
    pub id: GroupId,
    pub name: String,
    pub color_rgba: [u8; 4],
    pub locked: bool,
    pub tabs: Vec<SavedTab>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedTab {
    pub id: TabId,
    pub title: Option<String>,
    pub accent: Option<[u8; 4]>,
    pub root: PaneNode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedTerminal {
    pub id: TerminalId,
    pub label: String,
    pub scrollback_rows: usize,
    /// The grid the replay is parsed at.
    pub size: Dimensions,
    /// Set for a job's terminal: what the runner that adopts it needs to
    /// finish the run's record when it ends.
    pub run: Option<SavedRun>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedRun {
    pub run_dir: PathBuf,
    pub started: u64,
    pub cwd: Option<PathBuf>,
    pub artifacts: Vec<String>,
}

/// Reads `path`. `None` when there is nothing usable, with the reason on
/// stderr unless the file simply does not exist.
pub fn load(path: &Path) -> Option<SavedWorkspace> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!("[mux] cannot read {}: {e}", path.display());
            return None;
        }
    };
    match parse(&bytes) {
        Ok(saved) => Some(saved),
        Err(e) => {
            eprintln!("[mux] {}: {e}; starting fresh", path.display());
            None
        }
    }
}

/// A saved workspace of this version, or of the one before it in today's
/// shape.
fn parse(bytes: &[u8]) -> Result<SavedWorkspace, String> {
    #[derive(Deserialize)]
    struct Header {
        version: u32,
    }
    let unparsable = |e: serde_json::Error| format!("cannot parse: {e}");
    let Header { version } = serde_json::from_slice(bytes).map_err(unparsable)?;
    match version {
        VERSION => serde_json::from_slice(bytes).map_err(unparsable),
        1 => serde_json::from_slice::<SavedWorkspaceV1>(bytes)
            .map(SavedWorkspaceV1::migrate)
            .map_err(unparsable),
        other => Err(format!("version {other} is not {VERSION}")),
    }
}

/// Writes `saved` to `path` through a temporary file and a rename, so a
/// runner killed mid-write leaves the previous state rather than half of
/// the new one.
pub fn store(path: &Path, saved: &SavedWorkspace) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(saved).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

/// A version-1 file: a flat list of tabs with a free-text group label, and a
/// graph pane that named no graph. Fields it has and version 2 does not are
/// left out and so ignored: the label is not a group.
#[derive(Deserialize)]
struct SavedWorkspaceV1 {
    tabs: Vec<SavedTabV1>,
    next_tab: u64,
    next_split: u64,
    next_pane: u64,
    next_terminal: u64,
    owned: Vec<TerminalId>,
    detached: Vec<TerminalId>,
    terminals: Vec<SavedTerminal>,
}

#[derive(Deserialize)]
struct SavedTabV1 {
    id: TabId,
    title: Option<String>,
    accent: Option<[u8; 4]>,
    root: PaneNodeV1,
}

#[derive(Deserialize)]
enum PaneNodeV1 {
    Split {
        id: SplitId,
        axis: Axis,
        ratio: f32,
        first: Box<PaneNodeV1>,
        second: Box<PaneNodeV1>,
    },
    Leaf {
        pane_id: PaneId,
        surface: SurfaceV1,
    },
}

#[derive(Deserialize)]
enum SurfaceV1 {
    Graph,
    Empty,
    Terminal(TerminalId),
}

impl SavedWorkspaceV1 {
    /// Every tab at the top level. A graph pane goes like a closed one: it
    /// named no graph, and the graphs this runner executes get their panes
    /// from the document. A tab that showed nothing else goes with it.
    fn migrate(self) -> SavedWorkspace {
        let items = self
            .tabs
            .into_iter()
            .filter_map(|tab| {
                let mut graphs = Vec::new();
                let root = tab.root.migrate(&mut graphs);
                let root = graphs.into_iter().try_fold(root, remove_leaf)?;
                Some(SavedItem::Tab(SavedTab {
                    id: tab.id,
                    title: tab.title,
                    accent: tab.accent,
                    root,
                }))
            })
            .collect();
        SavedWorkspace {
            version: VERSION,
            items,
            next_tab: self.next_tab,
            next_group: 1,
            next_split: self.next_split,
            next_pane: self.next_pane,
            next_terminal: self.next_terminal,
            owned: self.owned,
            detached: self.detached,
            terminals: self.terminals,
        }
    }
}

impl PaneNodeV1 {
    /// The same tree with a graph leaf left empty and its pane listed in
    /// `graphs`, for the caller to remove.
    fn migrate(self, graphs: &mut Vec<PaneId>) -> PaneNode {
        match self {
            PaneNodeV1::Split {
                id,
                axis,
                ratio,
                first,
                second,
            } => PaneNode::Split {
                id,
                axis,
                ratio,
                first: Box::new(first.migrate(graphs)),
                second: Box::new(second.migrate(graphs)),
            },
            PaneNodeV1::Leaf { pane_id, surface } => PaneNode::Leaf {
                pane_id,
                surface: match surface {
                    SurfaceV1::Graph => {
                        graphs.push(pane_id);
                        SurfaceRef::Empty
                    }
                    SurfaceV1::Empty => SurfaceRef::Empty,
                    SurfaceV1::Terminal(terminal) => SurfaceRef::Terminal(terminal),
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file as a version-1 runner wrote it: the graph tab, a tab split
    /// between a shell and the graph under a group label, and a job's tab.
    const V1: &str = r#"{
      "version": 1,
      "tabs": [
        {
          "id": 1, "title": null, "group": null, "accent": null,
          "root": { "Leaf": { "pane_id": 1, "surface": "Graph" } }
        },
        {
          "id": 2, "title": "work", "group": "build", "accent": [255, 0, 0, 255],
          "root": { "Split": {
            "id": 1, "axis": "Horizontal", "ratio": 0.3,
            "first": { "Leaf": { "pane_id": 3, "surface": "Graph" } },
            "second": { "Leaf": { "pane_id": 2, "surface": { "Terminal": 1 } } }
          } }
        },
        {
          "id": 3, "title": null, "group": null, "accent": null,
          "root": { "Split": {
            "id": 2, "axis": "Vertical", "ratio": 0.5,
            "first": { "Leaf": { "pane_id": 4, "surface": { "Terminal": 2 } } },
            "second": { "Leaf": { "pane_id": 5, "surface": "Empty" } }
          } }
        }
      ],
      "next_tab": 4, "next_split": 3, "next_pane": 6, "next_terminal": 3,
      "owned": [2], "detached": [],
      "terminals": [
        { "id": 1, "label": "sh", "scrollback_rows": 1000,
          "size": { "cols": 80, "rows": 24 }, "run": null },
        { "id": 2, "label": "job", "scrollback_rows": 10000,
          "size": { "cols": 120, "rows": 40 },
          "run": { "run_dir": "/tmp/runs/7", "started": 5, "cwd": null, "artifacts": ["out/*"] } }
      ]
    }"#;

    #[test]
    fn a_version_1_file_keeps_its_shells_and_loses_its_graph_panes() {
        let saved = parse(V1.as_bytes()).expect("a version-1 file loads");
        assert_eq!(saved.version, VERSION);
        assert_eq!(
            saved.items,
            vec![
                // The graph tab showed nothing else and is gone; the split
                // collapsed into the shell's leaf, keeping its pane id.
                SavedItem::Tab(SavedTab {
                    id: TabId(2),
                    title: Some("work".to_owned()),
                    accent: Some([255, 0, 0, 255]),
                    root: PaneNode::Leaf {
                        pane_id: PaneId(2),
                        surface: SurfaceRef::Terminal(TerminalId(1)),
                    },
                }),
                // An empty pane is not a graph pane and stays.
                SavedItem::Tab(SavedTab {
                    id: TabId(3),
                    title: None,
                    accent: None,
                    root: PaneNode::Split {
                        id: SplitId(2),
                        axis: Axis::Vertical,
                        ratio: 0.5,
                        first: Box::new(PaneNode::Leaf {
                            pane_id: PaneId(4),
                            surface: SurfaceRef::Terminal(TerminalId(2)),
                        }),
                        second: Box::new(PaneNode::Leaf {
                            pane_id: PaneId(5),
                            surface: SurfaceRef::Empty,
                        }),
                    },
                }),
            ]
        );
        assert_eq!(
            (
                saved.next_tab,
                saved.next_group,
                saved.next_split,
                saved.next_pane,
                saved.next_terminal
            ),
            (4, 1, 3, 6, 3)
        );
        assert_eq!(saved.owned, vec![TerminalId(2)]);
        assert_eq!(saved.terminals.len(), 2);
        assert_eq!(
            saved.terminals[1].run.as_ref().map(|run| &run.run_dir),
            Some(&PathBuf::from("/tmp/runs/7"))
        );
    }
}
