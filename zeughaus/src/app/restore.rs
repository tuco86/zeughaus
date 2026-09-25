//! What a restarting editor hands to the process that replaces it: the
//! window's size, the tab in front and the focused pane, what is collapsed,
//! where each graph's camera was, the nodes' sizes, the selection, an open
//! palette or rename, how far each terminal was scrolled back, and --
//! without a store -- the scratch document itself.
//!
//! The window's position is not among them: a Wayland client can neither
//! read nor set where its window sits, the compositor places it. Its size
//! and maximized state are requests the compositor honours, so those are.
//!
//! The tab in front, the focused pane, the selection, a rename's focus and
//! terminal scrolls are applied as they become possible rather than at boot:
//! a runner's tab and pane ids only exist once that runner's snapshot has
//! arrived, a local graph's tab once the document has loaded, and a terminal
//! scrolls only once its first screen is here. Tabs and panes are named by
//! their section's runner key, because their ids are unique per runner only.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use iced::{Point, Task};
use serde::{Deserialize, Serialize};
use zeughaus_core::{GraphDocument, NodeId};
use zeughaus_mux::{GroupId, PaneId, TabId, TerminalId};

use super::{App, RENAME_INPUT, Rename};
use crate::message::Message;
use crate::workspace::{CollapseKey, PaneRef, RunnerKey, TabRef};

/// Bumped when a field changes meaning; a restore file of another version
/// is ignored. Fields only ever added default when missing, so the image a
/// reload replaces can be older than the one that reads its file.
const VERSION: u32 = 2;

/// How long after boot the tabs and pane of a restore are still applied.
/// Past it, whatever did not arrive (a deleted graph, a runner that does not
/// come back) is given up on.
const RESTORE_WINDOW: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreState {
    pub version: u32,
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
    /// `(runner key, tab)` of the tab in front.
    #[serde(default)]
    pub active: Option<(String, u64)>,
    /// `(runner key, pane)` of the focused pane.
    #[serde(default)]
    pub focused_pane: Option<(String, u64)>,
    /// Collapsed sections `(runner, None)` and groups `(runner, Some(group))`.
    #[serde(default)]
    pub collapsed: Vec<(String, Option<u64>)>,
    /// `(graph, x, y, zoom)`.
    pub cameras: Vec<(u64, f32, f32, f32)>,
    /// The scratch graph of an editor without a store. With a store, the
    /// store is the document.
    pub document: Option<GraphDocument>,
    /// `(node, width, height)` of nodes the user resized.
    #[serde(default)]
    pub node_sizes: Vec<(u64, f32, f32)>,
    #[serde(default)]
    pub selected: Vec<u64>,
    /// The palette's input and highlighted entry, when it was open.
    #[serde(default)]
    pub palette: Option<(String, usize)>,
    /// A node rename in progress: the node and what was typed so far.
    #[serde(default)]
    pub rename: Option<(u64, String)>,
    /// `(runner key, terminal, rows above the live screen)` of terminals
    /// scrolled back. Counted from the bottom rather than as a stable row,
    /// because a runner that restarted too numbers its rows anew.
    #[serde(default)]
    pub scrolls: Vec<(String, u64, i64)>,
    /// Nested containers open as views in a section without a runner.
    #[serde(default)]
    pub views: Vec<u64>,
}

/// The parts of a restore still waiting for what they apply to.
pub(super) struct PendingRestore {
    active: Option<TabRef>,
    focus: Option<PaneRef>,
    selected: Vec<NodeId>,
    /// Whether the rename field still has to get the keyboard.
    rename_focus: bool,
    scrolls: Vec<(RunnerKey, TerminalId, i64)>,
    /// Nested views still waiting for their container to load.
    views: Vec<NodeId>,
    since: Instant,
}

/// Names the restore file for the process a restart `exec`s.
pub const ENV: &str = "ZEUGHAUS_RESTORE";

/// The restore file this process was started with, if any.
pub fn from_env() -> Option<RestoreState> {
    let file = std::env::var_os(ENV)?;
    take(Path::new(&file))
}

/// Where a restart writes the restore file. The PID survives the `exec`, so
/// two editors restarting at once cannot take each other's.
pub fn path() -> PathBuf {
    zeughaus_link::credentials::state_dir().join(format!("restore-{}.json", std::process::id()))
}

/// Writes `state` to `file`, creating the state directory if needed.
pub fn write(file: &Path, state: &RestoreState) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec(state).map_err(std::io::Error::other)?;
    std::fs::write(file, json)
}

/// The restore file at `file`, read once and deleted: a second start with
/// the same environment is a fresh one. `None`, with one line on stderr,
/// when it is missing or unusable.
pub fn take(file: &Path) -> Option<RestoreState> {
    let bytes = std::fs::read(file);
    let _ = std::fs::remove_file(file);
    let bytes = match bytes {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("[editor] cannot read {}: {e}", file.display());
            return None;
        }
    };
    match serde_json::from_slice::<RestoreState>(&bytes) {
        Ok(state) if state.version == VERSION => Some(state),
        Ok(state) => {
            eprintln!(
                "[editor] {} is version {}, not {VERSION}",
                file.display(),
                state.version
            );
            None
        }
        Err(e) => {
            eprintln!("[editor] cannot parse {}: {e}", file.display());
            None
        }
    }
}

impl App {
    /// Starts the editor, from a restore file when there is one.
    pub fn boot(session: Option<String>, restore: Option<RestoreState>) -> (App, Task<Message>) {
        let mut app = App::new(session);
        let Some(state) = restore else {
            return (app, Task::none());
        };
        for &(graph, x, y, zoom) in &state.cameras {
            app.cameras.insert(NodeId(graph), (Point::new(x, y), zoom));
        }
        for (runner, group) in &state.collapsed {
            let runner = RunnerKey::new(runner);
            app.workspace.collapsed.insert(match group {
                None => CollapseKey::Section(runner),
                Some(group) => CollapseKey::Group(runner, GroupId(*group)),
            });
        }
        app.window_size = iced::Size::new(state.width, state.height);
        app.window_maximized = state.maximized;
        for &(node, width, height) in &state.node_sizes {
            app.node_sizes
                .insert(NodeId(node), iced::Size::new(width, height));
        }
        let mut tasks = Vec::new();
        if let Some((input, selected)) = state.palette {
            // Built while closed: an open palette keeps the list it has.
            app.rebuild_palette();
            app.palette_open = true;
            app.palette_input = input;
            app.palette_selected = selected;
            tasks.push(iced_palette::focus_input());
        }
        let rename_focus = state.rename.is_some();
        app.renaming = state.rename.map(|(node, draft)| Rename {
            node: NodeId(node),
            draft,
        });
        app.pending_restore = Some(PendingRestore {
            active: state.active.map(|(runner, tab)| TabRef {
                runner: RunnerKey::new(&runner),
                tab: TabId(tab),
            }),
            focus: state.focused_pane.map(|(runner, pane)| PaneRef {
                runner: RunnerKey::new(&runner),
                pane: PaneId(pane),
            }),
            selected: state.selected.iter().copied().map(NodeId).collect(),
            rename_focus,
            scrolls: state
                .scrolls
                .iter()
                .map(|(runner, terminal, rows)| {
                    (RunnerKey::new(runner), TerminalId(*terminal), *rows)
                })
                .collect(),
            views: state.views.iter().copied().map(NodeId).collect(),
            since: Instant::now(),
        });
        if let Some(document) = state.document {
            tasks.push(Task::done(Message::GraphLoaded(document)));
        }
        (app, Task::batch(tasks))
    }

    /// What the next process needs to look like this one.
    pub(super) fn restore_state(&self) -> RestoreState {
        let mut cameras: Vec<(u64, f32, f32, f32)> = self
            .cameras
            .iter()
            .map(|(graph, (position, zoom))| (graph.0, position.x, position.y, *zoom))
            .collect();
        cameras.sort_by_key(|camera| camera.0);
        let mut node_sizes: Vec<(u64, f32, f32)> = self
            .node_sizes
            .iter()
            .map(|(node, size)| (node.0, size.width, size.height))
            .collect();
        node_sizes.sort_by_key(|size| size.0);
        let mut selected: Vec<u64> = self.selected.iter().map(|node| node.0).collect();
        selected.sort_unstable();
        let mut collapsed: Vec<(String, Option<u64>)> = self
            .workspace
            .collapsed
            .iter()
            .map(|key| match key {
                CollapseKey::Section(runner) => (runner.as_str().to_owned(), None),
                CollapseKey::Group(runner, group) => (runner.as_str().to_owned(), Some(group.0)),
            })
            .collect();
        collapsed.sort();
        RestoreState {
            version: VERSION,
            width: self.window_size.width,
            height: self.window_size.height,
            maximized: self.window_maximized,
            active: self
                .workspace
                .active_tab()
                .map(|tab| (tab.runner.as_str().to_owned(), tab.tab.0)),
            focused_pane: self
                .workspace
                .focused_pane()
                .map(|pane| (pane.runner.as_str().to_owned(), pane.pane.0)),
            collapsed,
            cameras,
            document: self.stdb.is_none().then(|| self.to_document()),
            node_sizes,
            selected,
            palette: self
                .palette_open
                .then(|| (self.palette_input.clone(), self.palette_selected)),
            rename: self
                .renaming
                .as_ref()
                .map(|rename| (rename.node.0, rename.draft.clone())),
            scrolls: self
                .terminal_scrolls()
                .into_iter()
                .map(|(runner, terminal, rows)| (runner.as_str().to_owned(), terminal.0, rows))
                .collect(),
            views: self.local_views.iter().map(|view| view.0).collect(),
        }
    }

    /// Applies whatever part of a pending restore has become possible, and
    /// forgets the restore once all of it has been applied or its time is
    /// up.
    pub(super) fn finish_restore(&mut self) -> Task<Message> {
        let Some(pending) = self.pending_restore.as_mut() else {
            return Task::none();
        };
        // Before the tab: a view's tab exists once its section is rebuilt.
        let nodes = &self.nodes;
        let views = &mut self.local_views;
        let mut opened = false;
        pending.views.retain(|view| {
            let arrived = nodes.get(view).is_some_and(|node| node.is_container);
            if arrived && !views.contains(view) {
                views.push(*view);
                opened = true;
            }
            !arrived
        });
        if opened {
            self.sync_synthetic_sections();
        }
        let Some(pending) = self.pending_restore.as_mut() else {
            return Task::none();
        };
        let workspace = &mut self.workspace;
        if let Some(tab) = pending.active.clone()
            && workspace
                .section(&tab.runner)
                .is_some_and(|s| s.snapshot.tabs().any(|t| t.id == tab.tab))
        {
            workspace.activate(tab);
            pending.active = None;
        }
        // After the tab: activating one moves the focus to its default pane.
        if pending.active.is_none()
            && let Some(pane) = pending.focus.clone()
            && workspace.surface_of(&pane).is_some()
        {
            workspace.focus(pane);
            pending.focus = None;
        }
        // Only once the focused graph is the one on screen: switching graphs
        // clears the selection.
        let nodes = &self.nodes;
        if pending.active.is_none()
            && self.current_graph == workspace.focused_graph().unwrap_or(NodeId(0))
        {
            let selected = &mut self.selected;
            pending.selected.retain(|node| {
                let arrived = nodes.contains_key(node);
                if arrived {
                    selected.insert(*node);
                }
                !arrived
            });
        }
        let mut tasks = Vec::new();
        if pending.rename_focus
            && self
                .renaming
                .as_ref()
                .is_some_and(|rename| nodes.contains_key(&rename.node))
        {
            pending.rename_focus = false;
            tasks.push(iced::widget::operation::focus(RENAME_INPUT));
        }
        let done = pending.active.is_none()
            && pending.focus.is_none()
            && pending.selected.is_empty()
            && pending.views.is_empty()
            && !pending.rename_focus;
        let expired = pending.since.elapsed() >= RESTORE_WINDOW;
        let scrolls = std::mem::take(&mut pending.scrolls);
        let mut waiting = Vec::new();
        for (runner, terminal, rows) in scrolls {
            if self.terminal_has_screen(&runner, terminal) {
                tasks.push(self.scroll_terminal(&runner, terminal, -rows));
            } else {
                waiting.push((runner, terminal, rows));
            }
        }
        match self.pending_restore.as_mut() {
            Some(pending) if !expired && !(done && waiting.is_empty()) => {
                pending.scrolls = waiting;
            }
            _ => self.pending_restore = None,
        }
        Task::batch(tasks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A restore file names tabs, panes, collapsed entries and scrolls by
    /// their runner, and a file from before sections carries none of that:
    /// the new fields default rather than refusing it.
    #[test]
    fn a_restore_file_round_trips_and_new_fields_default() {
        let state = RestoreState {
            version: VERSION,
            width: 800.0,
            height: 600.0,
            maximized: false,
            active: Some(("sha256:aa".into(), 3)),
            focused_pane: Some(("sha256:aa".into(), 4)),
            collapsed: vec![("sha256:aa".into(), None), ("sha256:bb".into(), Some(2))],
            cameras: vec![(7, 1.0, 2.0, 1.5)],
            document: None,
            node_sizes: Vec::new(),
            selected: Vec::new(),
            palette: None,
            rename: None,
            scrolls: vec![("sha256:aa".into(), 9, 12)],
            views: vec![5, 6],
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: RestoreState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.active, state.active);
        assert_eq!(back.collapsed, state.collapsed);
        assert_eq!(back.scrolls, state.scrolls);
        assert_eq!(back.views, state.views);

        let bare: RestoreState = serde_json::from_str(
            r#"{"version":2,"width":1.0,"height":1.0,"maximized":false,"cameras":[],"document":null}"#,
        )
        .unwrap();
        assert!(bare.active.is_none() && bare.collapsed.is_empty() && bare.scrolls.is_empty());
        assert!(bare.views.is_empty());
    }
}
