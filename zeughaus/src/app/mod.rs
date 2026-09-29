//! The editor application: the window's state, the messages it answers and
//! the surfaces it draws.
//!
//! [`App`] owns every field; the child modules are its responsibilities, and
//! each reaches those fields directly rather than through accessors it would
//! be the only caller of.

/// The graph as the editor holds it: nodes, edges, node instances, and the
/// rules a wire has to pass.
mod graph;
/// Arranging a graph in columns by depth.
mod layout;
/// What one node draws.
mod node_view;
/// What a restarting editor leaves for the process that replaces it.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod restore;
/// What the runtime reported: values, failures, feeds, particles.
mod runtime;
/// The shared store: rows in, edits out.
#[cfg(not(target_arch = "wasm32"))]
mod store;
/// The runner's terminals and the panes that show them.
#[cfg(not(target_arch = "wasm32"))]
mod terminal;

#[cfg(not(target_arch = "wasm32"))]
use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

use iced::keyboard;
use iced::widget::{button, column, container, mouse_area, pane_grid, row, space, stack, text};
use iced::{Element, Event, Length, Point, Subscription, Task, Vector};
use iced_nodegraph::{
    EdgeStyle, NodeGraph, NodeStatus, NodeStyle, Pattern, PinInfo, PinRef, PinStyle,
    default_edge_style, default_node_style, default_pin_style, edge as ng_edge, input_not_occupied,
    node as ng_node,
};
// Particles are drawn from what the runtime delivered, and the wasm editor has
// no sync layer to hear it from.
#[cfg(not(target_arch = "wasm32"))]
use iced_nodegraph::{ParticleStyle, default_particle_style, particle};
use iced_palette::{get_filtered_command_index, is_toggle_shortcut};
#[cfg(not(target_arch = "wasm32"))]
use zeughaus_capture::CapturePlugin;
use zeughaus_core::{
    DomainPlugin, ExecutableNode, NodeDefinition, NodeId, PinKind, TypeConverters,
};
#[cfg(not(target_arch = "wasm32"))]
use zeughaus_core::{EdgeData, GraphDocument};
use zeughaus_flow::FlowPlugin;
use zeughaus_graph::GraphPlugin;
#[cfg(not(target_arch = "wasm32"))]
use zeughaus_llm::LlmPlugin;
use zeughaus_ml::MlPlugin;
use zeughaus_theme::Theme;
use zeughaus_transform::TransformPlugin;

use graph::{EditorEdge, EditorNode, NodeEdges, Wire, flow_reaches, wire_refusal};
use layout::auto_layout;
pub use node_view::PinVisual;
use node_view::{DisplayValue, NodeChrome, build_node_element, dim, is_display, pin_color};
#[cfg(not(target_arch = "wasm32"))]
use runtime::PARTICLE_SPEED;
use runtime::RuntimeView;
#[cfg(not(target_arch = "wasm32"))]
use terminal::MuxState;

#[cfg(not(target_arch = "wasm32"))]
use crate::feed;
use crate::message::{GraphIds, Message};
use crate::palette;
use crate::workspace::{self, DropTarget, PaneRef, RunnerKey, Surface, TabRef, Workspace};

/// How long a status-bar hint stays. Long enough to read one sentence after
/// the drop that produced it, short enough that it is gone before the next
/// thing the user tries.
const HINT_LIFETIME: std::time::Duration = std::time::Duration::from_secs(3);
/// Height of the editor's own titlebar, which is also the tab strip's row
/// while the bar sits on top.
const TITLEBAR_HEIGHT: f32 = 30.0;
/// How far the pointer moves with the button held on the titlebar before
/// the press becomes a window drag. A click, or the second click of a
/// double-click, never moves the window.
const TITLEBAR_DRAG_THRESHOLD: f32 = 4.0;
/// Width of the tab sidebar while the bar sits at the left edge.
const SIDEBAR_WIDTH: f32 = 180.0;

/// A node rename in progress: which node, and what has been typed so far.
struct Rename {
    node: NodeId,
    draft: String,
}

/// The one rename field; only one node is renamed at a time.
const RENAME_INPUT: iced::widget::Id = iced::widget::Id::new("node-rename");
/// The group rename field; only one group is renamed at a time.
const GROUP_RENAME_INPUT: iced::widget::Id = iced::widget::Id::new("group-rename");
/// The tab rename field; only one tab is renamed at a time.
const TAB_RENAME_INPUT: iced::widget::Id = iced::widget::Id::new("tab-rename");
/// Height of the strip a pane is dragged by.
const PANE_GRIP_HEIGHT: f32 = 18.0;

pub struct App {
    workspace: Workspace,
    /// What every widget, the graph, the tab bar and the terminals draw
    /// with. Cloned on every frame by [`App::theme`], which is why it is an
    /// `Arc` behind the scenes.
    theme: Theme,
    /// Every theme this window can switch to: the bundled pack, then the
    /// files under the state directory. The palette lists it and
    /// [`Message::SetTheme`] is resolved against it.
    themes: Vec<Theme>,
    // Editor state
    nodes: HashMap<NodeId, EditorNode>,
    node_order: Vec<NodeId>,
    edges: Vec<EditorEdge>,
    /// `edges` grouped by the nodes they touch, rebuilt whenever the edge set
    /// changes. What a node shows is read from the values on its own edges,
    /// and answering that by scanning every edge twice, for every node, on
    /// every arriving value is O(nodes x edges) at frame rate.
    edge_index: HashMap<NodeId, NodeEdges>,
    /// One node instance per node, for what a node knows about itself: the
    /// pins it declares, the settings it offers, the pins it derives from what
    /// is connected, and what it makes of a value typed into a setting.
    ///
    /// Never executed here. The runner is the only process that runs a graph,
    /// and everything on screen is what it reported.
    instances: HashMap<NodeId, Box<dyn ExecutableNode>>,
    selected: HashSet<NodeId>,
    /// The graph of the focused pane: where the palette spawns nodes and
    /// what an auto layout arranges. `NodeId(0)` while no graph pane has the
    /// focus. Editor-local: what one window looks at is not shared state.
    current_graph: NodeId,
    /// Camera per graph, shared by every pane showing it. A missing entry is
    /// the origin at zoom 1.
    cameras: HashMap<NodeId, (Point, f32)>,
    /// A graph this window just created or asked to open, focused as soon as
    /// a pane shows it: a runner answers with a snapshot, not at once.
    pending_graph_focus: Option<NodeId>,
    /// Nested containers opened as view tabs in a section no runner stands
    /// behind, where there is nobody to ask for a tab.
    local_views: Vec<NodeId>,

    plugins: Vec<Box<dyn DomainPlugin>>,
    catalog: Vec<NodeDefinition>,
    /// The palette's command list, built once from the catalog.
    ///
    /// The catalog is fixed at startup -- plugins are registered in `new` and
    /// never again -- while search filtering stays dynamic and reads this
    /// list. Building it per redraw would cost one `String` per catalog entry
    /// per keystroke and per sync poll.
    palette_commands: Vec<iced_palette::Command<Message>>,
    /// Which output type reaches which input type, directly or by conversion.
    /// Built from the plugins and asked whenever a wire is drawn, so the
    /// editor refuses exactly what the runner could not carry.
    converters: Arc<TypeConverters>,

    // What each node shows inline (node_id -> text or decoded frame)
    display_values: HashMap<NodeId, DisplayValue>,

    // Content size of nodes the user resized by dragging their corner grip.
    // Editor-local: a size is a per-user view preference, so it is deliberately
    // not part of the shared graph document.
    node_sizes: HashMap<NodeId, iced::Size>,

    // Edges from the store whose endpoint nodes have not arrived yet. A
    // subscription applies as one burst with no ordering between tables, so an
    // edge routinely precedes its nodes; dropping those is what left a fresh
    // window showing a fraction of the wires.
    #[cfg(not(target_arch = "wasm32"))]
    pending_edges: Vec<EdgeData>,

    // In-node settings (node_id -> setting name -> the text the user sees)
    node_settings: HashMap<NodeId, HashMap<String, String>>,

    /// What the window is showing, in logical pixels. Reported on opening
    /// and resizing; before the first window event, use the size `main`
    /// requests so palette placement has a valid viewport.
    window_size: iced::Size,
    #[cfg(not(target_arch = "wasm32"))]
    window_maximized: bool,
    /// Whether the window has the keyboard focus. Starts true: a window that
    /// just opened is the one the user opened.
    #[cfg(not(target_arch = "wasm32"))]
    window_focused: bool,
    /// Device pixels per logical pixel, asked for when the window opens and
    /// updated when it changes; terminal panes draw whole device pixels.
    scale_factor: f32,
    /// Where the pointer last was over the titlebar's drag region, and where
    /// a press there that has not become a window drag yet happened.
    titlebar_cursor: Point,
    titlebar_press: Option<Point>,

    // Status bar
    last_error: String,
    /// A sentence about something the editor just turned down, and when it was
    /// said. Shown in the status bar until [`HINT_LIFETIME`] has passed: a
    /// refused wire is worth one sentence and nothing more, and a notice that
    /// stays becomes furniture nobody reads.
    hint: Option<(String, iced::time::Instant)>,

    // Command palette state
    palette_open: bool,
    palette_input: String,
    palette_selected: usize,
    /// The node whose header currently shows a name field, if any.
    renaming: Option<Rename>,

    // The store connection, which rebuilds itself when the host goes away.
    // Held rather than a bare `DbConnection` because there is no such thing as
    // a connection a process can be handed once: a host restart or a dropped
    // packet ends it, and editing against the corpse reaches nobody.
    #[cfg(not(target_arch = "wasm32"))]
    stdb: Option<zeughaus_sync::Store>,
    // Edits that have not reached the store, in the order they were made.
    // Replayed when the connection comes back. Each entry carries the payload
    // as it was at the time of the edit, not a reference to state that the
    // reconnect's snapshot may have overwritten in the meantime.
    #[cfg(not(target_arch = "wasm32"))]
    outbox: Vec<store::Outbound>,
    // Reducer failures already logged, so a store that refuses every call does
    // not fill the terminal with one line per edit.
    #[cfg(not(target_arch = "wasm32"))]
    logged_sends: HashSet<String>,
    // Receiver for remote changes, drained on the SyncPoll timer.
    #[cfg(not(target_arch = "wasm32"))]
    sync_rx: Option<std::sync::mpsc::Receiver<zeughaus_sync::SyncEvent>>,
    // True while applying a remote change, so it does not echo back as a reducer.
    #[cfg(not(target_arch = "wasm32"))]
    applying_remote: bool,
    // The joined collaboration session id (database name), if any. Shown via the
    // palette "Copy Session ID" command so others can `join` the same session.
    #[cfg(not(target_arch = "wasm32"))]
    session_id: Option<String>,
    // How many runtime processes the store knows about. This process registers
    // as `Role::Viewer`, so it is never one of them: the count only answers
    // "is anything computing the values on screen".
    #[cfg(not(target_arch = "wasm32"))]
    runtimes: usize,
    /// What the runtime reported and what this editor holds about it: values,
    /// failures, feeds, particles. See [`RuntimeView`].
    runtime: RuntimeView,
    /// What a node said about a setting it refused, per setting key. Drawn
    /// under the field, so a rejected value does not sit there looking
    /// accepted.
    ///
    /// Filled from two directions: this window applying a setting locally, and
    /// the runtime reporting a refusal for a value some window pushed
    /// ([`zeughaus_link::RuntimeEvent::SettingRejected`]). They agree about
    /// what a refusal is, so they share the map; a snapshot or a lost runtime
    /// replaces it wholesale, and a local refusal that has not reached the
    /// store yet is recorded again by the next keystroke.
    setting_errors: HashMap<NodeId, HashMap<String, String>>,
    /// Settings edits the store has not seen yet. See [`crate::pending`].
    #[cfg(not(target_arch = "wasm32"))]
    pending: crate::pending::PendingEdits,
    /// Each connected runner's terminal multiplexer, while one is reachable.
    /// A runner without one has an empty section and refuses every terminal
    /// action.
    #[cfg(not(target_arch = "wasm32"))]
    mux: BTreeMap<RunnerKey, MuxState>,
    /// Running without a window (`zeughaus --headless`): nobody sits in front
    /// of a file dialog, and rfd would open one on the desktop of whoever
    /// started the process.
    #[cfg(all(feature = "remote", not(target_arch = "wasm32")))]
    headless: bool,
    /// The parts of a restore file that wait for their tabs and panes.
    #[cfg(not(target_arch = "wasm32"))]
    pending_restore: Option<restore::PendingRestore>,
}

impl App {
    pub fn new(session: Option<String>) -> Self {
        #[cfg(target_arch = "wasm32")]
        let _ = session;
        // Capture and LLM plugins are native-only (DXGI capture, local model
        // host). The wasm editor designs graphs; native runners execute them.
        let plugins: Vec<Box<dyn DomainPlugin>> = vec![
            Box::new(TransformPlugin),
            Box::new(MlPlugin),
            Box::new(FlowPlugin),
            Box::new(GraphPlugin),
            #[cfg(not(target_arch = "wasm32"))]
            Box::new(CapturePlugin),
            #[cfg(not(target_arch = "wasm32"))]
            Box::new(zeughaus_db::DbPlugin),
            #[cfg(not(target_arch = "wasm32"))]
            Box::new(zeughaus_record::RecordPlugin),
            #[cfg(not(target_arch = "wasm32"))]
            Box::new(LlmPlugin),
            // Detached: a job node is drawn and configured here and executed
            // by a runner, so the plugin is registered for its catalog alone.
            Box::new(zeughaus_job::JobPlugin::detached()),
        ];
        let catalog: Vec<NodeDefinition> = plugins.iter().flat_map(|p| p.node_catalog()).collect();

        // The type-converter registry: the builtins plus each plugin's own,
        // which is what lets a wire cross from one plugin's types into
        // another's.
        let converters = Arc::new({
            let mut c = TypeConverters::with_builtins();
            for p in &plugins {
                p.register_converters(&mut c);
            }
            c
        });

        // `session` is the join token, or `None` to host the default session
        // on the local store. The token is what the palette's "Copy Session
        // ID" shares so a buddy can join.
        //
        // The first connection still has to succeed: a token naming a store
        // that is not there is a startup mistake, and retrying it forever
        // would only hide it. What is not fatal is LOSING it -- see
        // [`zeughaus_sync::Store`]. With no store at all this window edits a
        // local scratch graph, which is what makes an editor usable before
        // `spacetime start`; that graph is gone when the window closes unless
        // it is saved to a file.
        #[cfg(not(target_arch = "wasm32"))]
        let (stdb, sync_rx, session_id) = {
            let session = zeughaus_sync::Session::resolve(session.as_deref());
            // Role::Viewer: this process edits and displays, it never executes,
            // so it must not register in the runtime table and be elected owner.
            match zeughaus_sync::Store::open(
                &session.uri,
                &session.database,
                zeughaus_sync::Role::Viewer,
            ) {
                Ok((store, rx)) => {
                    eprintln!("[stdb] session token: {}", session.token);
                    (Some(store), Some(rx), Some(session.token))
                }
                Err(e) => {
                    eprintln!(
                        "[stdb] cannot reach {} / {}: {e} -- editing locally \
                         (start it with `spacetime start` and restart to collaborate)",
                        session.uri, session.database
                    );
                    (None, None, None)
                }
            }
        };

        // The bundled pack first, then what the user dropped into the state
        // directory: a file theme that takes a pack name loses, so a bundled
        // name means one thing on every machine.
        #[cfg(not(target_arch = "wasm32"))]
        let prefs = crate::prefs::load();
        #[cfg_attr(target_arch = "wasm32", allow(unused_mut))]
        let mut themes: Vec<Theme> = Theme::pack().to_vec();
        #[cfg(not(target_arch = "wasm32"))]
        themes.extend(crate::prefs::themes());
        // A remembered name nothing answers to -- a file theme deleted since,
        // a typo in the file -- leaves the default in place rather than
        // refusing to open the window.
        #[cfg(not(target_arch = "wasm32"))]
        let theme = themes
            .iter()
            .find(|theme| theme.name() == prefs.theme)
            .cloned()
            .unwrap_or_default();
        #[cfg(target_arch = "wasm32")]
        let theme = Theme::default();

        #[cfg_attr(target_arch = "wasm32", allow(unused_mut))]
        let mut workspace = Workspace::new();
        #[cfg(not(target_arch = "wasm32"))]
        {
            workspace.placement = prefs.tabs.into();
        }

        // Nothing is attached yet: the runner's half of the palette is
        // filled in by the first workspace snapshot.
        let palette_commands =
            palette::build_commands(&catalog, &palette::RunnerState::default(), &themes);

        let mut app = Self {
            workspace,
            theme,
            themes,
            nodes: HashMap::new(),
            node_order: Vec::new(),
            edges: Vec::new(),
            edge_index: HashMap::new(),
            instances: HashMap::new(),
            selected: HashSet::new(),
            current_graph: NodeId(0),
            cameras: HashMap::new(),
            pending_graph_focus: None,
            local_views: Vec::new(),
            plugins,
            palette_commands,
            catalog,
            converters,
            display_values: HashMap::new(),
            node_sizes: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            pending_edges: Vec::new(),
            node_settings: HashMap::new(),
            window_size: crate::WINDOW_SIZE,
            #[cfg(not(target_arch = "wasm32"))]
            window_maximized: false,
            #[cfg(not(target_arch = "wasm32"))]
            window_focused: true,
            scale_factor: 1.0,
            titlebar_cursor: Point::ORIGIN,
            titlebar_press: None,
            last_error: String::new(),
            hint: None,
            palette_open: false,
            renaming: None,
            palette_input: String::new(),
            palette_selected: 0,
            #[cfg(not(target_arch = "wasm32"))]
            stdb,
            #[cfg(not(target_arch = "wasm32"))]
            sync_rx,
            #[cfg(not(target_arch = "wasm32"))]
            applying_remote: false,
            #[cfg(not(target_arch = "wasm32"))]
            session_id,
            // No runtime is known until the subscription delivers that table,
            // and this process never adds itself to it.
            #[cfg(not(target_arch = "wasm32"))]
            runtimes: 0,
            #[cfg(not(target_arch = "wasm32"))]
            outbox: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            logged_sends: HashSet::new(),
            // Nothing is known about a runtime until one announces itself in
            // the store: no address, no values, no feed.
            runtime: RuntimeView::default(),
            setting_errors: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            pending: crate::pending::PendingEdits::new(),
            // No mux until a runtime announces where it serves.
            #[cfg(not(target_arch = "wasm32"))]
            mux: BTreeMap::new(),
            #[cfg(all(feature = "remote", not(target_arch = "wasm32")))]
            headless: false,
            #[cfg(not(target_arch = "wasm32"))]
            pending_restore: None,
        };
        // Without a store this window edits a scratch document, and a fresh
        // one opens on a canvas rather than on an empty tab bar. A restore
        // or a loaded file replaces it.
        #[cfg(not(target_arch = "wasm32"))]
        let scratch = app.stdb.is_none();
        #[cfg(target_arch = "wasm32")]
        let scratch = true;
        if scratch && let Some(graph) = app.create_graph("") {
            app.pending_graph_focus = Some(graph);
        }
        app.sync_workspace();
        app
    }

    /// Marks this editor as windowless. See the `headless` field.
    #[cfg(all(feature = "remote", not(target_arch = "wasm32")))]
    pub fn set_headless(&mut self) {
        self.headless = true;
    }

    /// What a save or load does without a window: one sentence, no dialog.
    #[cfg(all(feature = "remote", not(target_arch = "wasm32")))]
    fn refuse_file_dialog(&mut self) -> Task<Message> {
        self.hint = Some((
            "file dialogs are off in headless mode".to_owned(),
            iced::time::Instant::now(),
        ));
        Task::none()
    }

    /// No store, nothing to hold back.
    #[cfg(target_arch = "wasm32")]
    fn flush_pending(&mut self) {}

    /// Drops the status-bar hint once it has been on screen long enough.
    fn expire_hint(&mut self) {
        if self
            .hint
            .as_ref()
            .is_some_and(|(_, said)| said.elapsed() >= HINT_LIFETIME)
        {
            self.hint = None;
        }
    }

    /// The browser editor has no transport to a runner, so a structural
    /// change has nowhere to go. Unreachable while the chrome disables the
    /// buttons, and the honest answer if it ever is reached.
    #[cfg(target_arch = "wasm32")]
    fn send_topology(&mut self, _key: &RunnerKey, _command: zeughaus_mux::TopologyCommand) {
        self.hint = Some((
            "the browser editor cannot change the shared workspace".to_owned(),
            iced::time::Instant::now(),
        ));
    }

    /// Whether anything is executing the graph and whether video is arriving,
    /// for the status bar.
    ///
    /// Worth permanent screen space: with no runtime connected every value on
    /// screen is a leftover nobody will refresh, and that explains away every
    /// otherwise surprising number. The frame total is there for the same
    /// reason one step further in -- a feed count alone looks identical whether
    /// frames are flowing or the stream has stalled, and a number that climbs
    /// is the difference.
    #[cfg(not(target_arch = "wasm32"))]
    fn runtime_text(&self) -> String {
        // First, because it outranks everything after it: with no store this
        // window is editing a graph nobody else will see, and the runtime
        // counts behind it are whatever the last connection said.
        if self.stdb.is_some() && !self.store_live() {
            let owed = self.outbox.len();
            return match owed {
                0 => " | store offline (reconnecting)".to_string(),
                1 => " | store offline (reconnecting, 1 edit waiting)".to_string(),
                n => format!(" | store offline (reconnecting, {n} edits waiting)"),
            };
        }
        let mut text = match self.runtimes {
            0 => " | no runtime".to_string(),
            1 => " | 1 runner".to_string(),
            n => format!(" | {n} runners"),
        };
        // Whether values are arriving, not just whether a runner exists: the
        // event subscriptions are what carry them, and a dropped one leaves
        // every number of that runner's graphs a leftover.
        let links = self.runtime.links.len();
        if links > 0 {
            let live = self
                .runtime
                .links
                .values()
                .filter(|link| link.traffic_live)
                .count();
            if live == links {
                text.push_str(" | live");
            } else {
                text.push_str(&format!(" | reconnecting ({live} of {links} live)"));
            }
        }
        if !self.runtime.feeds.is_empty() {
            let feeds = self.runtime.feeds.len();
            let plural = if feeds == 1 { "" } else { "s" };
            text.push_str(&format!(
                " | {feeds} feed{plural}, {} frames",
                self.runtime.frames_received
            ));
        }
        // Whether a shell is reachable, said in its own words: a terminal
        // pane showing a last-known screen looks exactly like a live one.
        if links > 0 {
            let attached = self.mux.values().filter(|mux| mux.attached).count();
            if attached == links {
                text.push_str(" | mux: attached");
            } else {
                text.push_str(&format!(" | mux: {attached} of {links} attached"));
            }
        }
        text
    }

    // The wasm editor has no sync layer, so it has no runtime to report on.
    #[cfg(target_arch = "wasm32")]
    fn runtime_text(&self) -> String {
        String::new()
    }

    /// The world point in the middle of what this window is showing.
    ///
    /// Where the palette puts a node, so it appears where the user is looking.
    /// The window size is tracked rather than assumed: a fixed 1280x800 guess
    /// puts every new node in the upper-left quadrant of a maximised window,
    /// far from the middle it is supposed to be at.
    fn viewport_center(&self) -> Point {
        let (position, zoom) = self.camera(self.current_graph);
        Point::new(
            self.window_size.width * 0.5 / zoom - position.x,
            self.window_size.height * 0.5 / zoom - position.y,
        )
    }

    /// A graph's camera: where its panes look and how close.
    fn camera(&self, graph: NodeId) -> (Point, f32) {
        self.cameras
            .get(&graph)
            .copied()
            .unwrap_or((Point::ORIGIN, 1.0))
    }

    fn palette_confirm(&mut self) -> Option<Message> {
        // The global key subscription emits PaletteConfirm on every Enter, even
        // when the palette is closed (the closure can't see our state). Guard
        // here so a stray Enter never spawns the index-0 catalog node.
        if !self.palette_open {
            return None;
        }
        let commands = &self.palette_commands;
        let original_idx =
            get_filtered_command_index(&self.palette_input, commands, self.palette_selected)?;

        let cmd = commands.get(original_idx)?;
        if let iced_palette::CommandAction::Message(msg) = &cmd.action {
            let msg = msg.clone();
            self.palette_close();
            Some(msg)
        } else {
            None
        }
    }

    fn palette_close(&mut self) {
        self.palette_open = false;
        self.palette_input.clear();
        self.palette_selected = 0;
    }

    /// Rebuilds the palette's command list.
    ///
    /// The catalog half is fixed at startup; the runner's half is not -- its
    /// detached terminals come and go with every workspace snapshot, and
    /// whether it can be held at all depends on an endpoint being known. Per
    /// snapshot, not per redraw: a structural change is rare, a redraw is not.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn rebuild_palette(&mut self) {
        // Never under an open palette: the selection is an index into this
        // list, and a snapshot arriving mid-search would move the entry the
        // next Enter runs. Opening it rebuilds first.
        if self.palette_open {
            return;
        }
        let runners = self
            .runtime
            .links
            .iter()
            .map(|(key, link)| palette::RunnerPalette {
                key: key.clone(),
                label: link.label.as_str(),
                detached: self.workspace.detached(key),
                reachable: true,
            })
            .collect();
        let pane_commands = self
            .workspace
            .focused_pane()
            .is_some_and(|pane| self.workspace.attached(&pane.runner));
        let commands = palette::build_commands(
            &self.catalog,
            &palette::RunnerState {
                runners,
                pane_commands,
            },
            &self.themes,
        );
        self.palette_commands = commands;
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        let task = self.apply(message);
        self.sync_workspace();
        #[cfg(not(target_arch = "wasm32"))]
        if self.pending_restore.is_some() {
            let restored = self.finish_restore();
            self.sync_workspace();
            return Task::batch([task, restored]);
        }
        task
    }

    /// Brings the synthetic sections, a pending graph focus and
    /// `current_graph` in line with the document and the workspace.
    ///
    /// Runs after every message, because the answer can change under any of
    /// them: a graph created or deleted here or by a peer, a runner coming
    /// or going, a pane focused, a loaded file replacing every node. Doing
    /// it once here instead of at each of those sites is what keeps them
    /// from ever disagreeing. A selection from another graph is dropped
    /// because a delete would act on nodes the user can no longer see.
    fn sync_workspace(&mut self) {
        self.sync_synthetic_sections();
        if let Some(graph) = self.pending_graph_focus
            && ((self.workspace.show_graph(graph) && !self.awaits_runner_tab(graph))
                || !self.nodes.contains_key(&graph))
        {
            self.pending_graph_focus = None;
        }
        let target = self.workspace.focused_graph().unwrap_or(NodeId(0));
        if target == self.current_graph {
            return;
        }
        self.current_graph = target;
        self.selected.clear();
    }

    /// The sections no runner stands behind: without a store every graph is
    /// local; with one, the graphs no runner's workspace shows are listed as
    /// not running, and the section is gone when there are none.
    fn sync_synthetic_sections(&mut self) {
        let nodes = &self.nodes;
        self.local_views
            .retain(|graph| nodes.get(graph).is_some_and(|node| node.is_container));
        #[cfg(not(target_arch = "wasm32"))]
        let store = self.stdb.is_some();
        #[cfg(target_arch = "wasm32")]
        let store = false;
        #[cfg(not(target_arch = "wasm32"))]
        if store {
            self.hand_over_views();
        }
        // A graph leaves this section only once a runner's tab shows it, or
        // while its runner's mux is still attaching (which would otherwise
        // flash it here); a graph its runner does not show stays reachable.
        let shown_elsewhere = |graph: NodeId| -> bool {
            #[cfg(not(target_arch = "wasm32"))]
            {
                if self.workspace.runner_shows(graph) {
                    return true;
                }
                self.runner_of(graph)
                    .map(RunnerKey::new)
                    .is_some_and(|key| {
                        self.runtime.links.contains_key(&key) && !self.workspace.attached(&key)
                    })
            }
            #[cfg(target_arch = "wasm32")]
            {
                let _ = graph;
                false
            }
        };
        let graphs: Vec<(NodeId, String)> = self
            .top_level_graphs()
            .filter(|node| node.is_container)
            .map(|node| node.id)
            .chain(self.local_views.iter().copied())
            .filter(|graph| !store || !shown_elsewhere(*graph))
            .filter_map(|graph| Some((graph, self.nodes.get(&graph)?.display_name.clone())))
            .collect();
        let (shown, hidden, label) = if store {
            (RunnerKey::offline(), RunnerKey::local(), "Not running")
        } else {
            (RunnerKey::local(), RunnerKey::offline(), "Local")
        };
        self.workspace.set_synthetic(hidden, "", None);
        let snapshot = (!store || !graphs.is_empty()).then(|| workspace::graph_snapshot(graphs));
        self.workspace.set_synthetic(shown, label, snapshot);
    }

    /// Whether `graph`'s runner is attached but its tab has not arrived yet:
    /// until it does, the graph is listed as not running, and a focus meant
    /// for it must wait for the runner's tab rather than settle there.
    fn awaits_runner_tab(&self, graph: NodeId) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.runner_of(graph)
                .is_some_and(|runner| self.workspace.attached(&RunnerKey::new(runner)))
                && !self.workspace.runner_shows(graph)
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = graph;
            false
        }
    }

    /// Hands every nested view this window keeps to its graph's runner once
    /// that runner's workspace is attached: from then on the view is the
    /// runner's tab, shared with every editor. A view in front stays in
    /// front through the hand-over.
    #[cfg(not(target_arch = "wasm32"))]
    fn hand_over_views(&mut self) {
        let offline = RunnerKey::offline();
        let mut index = 0;
        while let Some(&view) = self.local_views.get(index) {
            let Some(key) = self
                .runner_of(view)
                .map(RunnerKey::new)
                .filter(|key| self.workspace.attached(key))
            else {
                index += 1;
                continue;
            };
            self.local_views.remove(index);
            let in_front = self.workspace.active_tab()
                == Some(&TabRef {
                    runner: offline.clone(),
                    tab: zeughaus_mux::TabId(view.0),
                });
            if in_front {
                self.pending_graph_focus = Some(view);
            }
            if !self.workspace.runner_shows(view) {
                self.send_topology(
                    &key,
                    zeughaus_mux::TopologyCommand::OpenGraph { graph: view.0 },
                );
            }
        }
    }

    /// Keeps every node where it is on screen while the sidebar moves in or
    /// out. The canvas origin shifts by the sidebar's width, so every camera
    /// moves the other way: `screen = origin + (world + position) * zoom`
    /// makes that `-dx / zoom` in camera units.
    fn pan_for_sidebar(&mut self) {
        let dx = match self.workspace.placement {
            iced_tabs::Placement::Left => SIDEBAR_WIDTH,
            iced_tabs::Placement::Top => -SIDEBAR_WIDTH,
        };
        for (position, zoom) in self.cameras.values_mut() {
            position.x -= dx / *zoom;
        }
    }

    /// The graphs among `panes`' surfaces, split into top-level ones (which
    /// closing deletes) and nested views (which closing only hides).
    fn graph_leaves(&self, surfaces: impl IntoIterator<Item = Surface>) -> (Vec<u64>, Vec<NodeId>) {
        let mut top = Vec::new();
        let mut nested = Vec::new();
        for surface in surfaces {
            let Surface::Graph(graph) = surface else {
                continue;
            };
            match self.nodes.get(&NodeId(graph)) {
                Some(node) if node.parent == NodeId(0) => top.push(graph),
                Some(_) => nested.push(NodeId(graph)),
                None => {}
            }
        }
        (top, nested)
    }

    /// What closing a tab or a pane does to the graphs it shows: a top-level
    /// graph is deleted, the way closing a shell ends it; a nested view in a
    /// section without a runner is only hidden. Returns whether the close
    /// still has to reach a runner.
    fn close_graphs(
        &mut self,
        runner: &RunnerKey,
        surfaces: Vec<Surface>,
    ) -> (bool, Task<Message>) {
        let (top, nested) = self.graph_leaves(surfaces);
        if runner.is_synthetic() {
            self.local_views.retain(|view| !nested.contains(view));
        }
        let task = if top.is_empty() {
            Task::none()
        } else {
            self.apply(Message::DeleteNodes(top))
        };
        (!runner.is_synthetic(), task)
    }

    /// Records what this window looks like under the state directory, so the
    /// next one opens the same way. Best effort: a preference that cannot be
    /// written is worth a line on stderr and nothing more.
    #[cfg(not(target_arch = "wasm32"))]
    fn save_prefs(&self) {
        crate::prefs::save(self.theme.name(), self.workspace.placement);
    }

    fn apply(&mut self, message: Message) -> Task<Message> {
        // Held-back settings edits reach the store before anything that reads
        // or changes the shared graph. A wire drawn onto a pin the store does
        // not know about yet, or a node deleted before its own text ever
        // arrived, would leave a graph nobody can reconstruct. The typing
        // messages are of course exempt -- holding them back is the point.
        #[cfg(not(target_arch = "wasm32"))]
        if store::observes_store(&message) {
            self.flush_pending();
        }
        match message {
            Message::Workspace(message) => {
                // Closing what shows a top-level graph deletes the graph; a
                // synthetic section has no runner to tell.
                let mut closed = Task::none();
                match &message {
                    workspace::Message::CloseTab(tab) => {
                        let surfaces: Vec<Surface> = self
                            .workspace
                            .section(&tab.runner)
                            .and_then(|s| s.snapshot.tabs().find(|t| t.id == tab.tab))
                            .map(|t| t.root.leaves().into_iter().map(|(_, s)| s).collect())
                            .unwrap_or_default();
                        let (remote, task) = self.close_graphs(&tab.runner.clone(), surfaces);
                        if !remote {
                            return task;
                        }
                        closed = task;
                    }
                    workspace::Message::CloseFocused => {
                        if let Some(pane) = self.workspace.focused_pane().cloned() {
                            let surfaces = self.workspace.surface_of(&pane).into_iter().collect();
                            let (remote, task) = self.close_graphs(&pane.runner, surfaces);
                            if !remote {
                                return task;
                            }
                            closed = task;
                        }
                    }
                    // A tab that is one graph is named by the graph: the
                    // node's name reaches every editor and the runner's
                    // title follows it.
                    workspace::Message::RenameTabCommit => {
                        let graph = self
                            .workspace
                            .renaming_tab
                            .as_ref()
                            .and_then(|(tab, _)| self.workspace.single_graph(tab))
                            .filter(|graph| self.nodes.contains_key(graph));
                        if let Some(graph) = graph
                            && let Some((_, draft)) = self.workspace.renaming_tab.take()
                        {
                            let name = draft.trim();
                            if let Some(node) = self.nodes.get_mut(&graph)
                                && !name.is_empty()
                                && node.display_name != name
                            {
                                name.clone_into(&mut node.display_name);
                                #[cfg(not(target_arch = "wasm32"))]
                                self.push_rename(graph, name.to_owned());
                            }
                            return Task::none();
                        }
                    }
                    _ => {}
                }
                let focus_rename = match message {
                    workspace::Message::RenameGroupStart(..) => Some(GROUP_RENAME_INPUT),
                    workspace::Message::RenameTabStart(..) => Some(TAB_RENAME_INPUT),
                    _ => None,
                };
                let placement = self.workspace.placement;
                let update = self.workspace.update(message);
                if self.workspace.placement != placement {
                    self.pan_for_sidebar();
                    #[cfg(not(target_arch = "wasm32"))]
                    self.save_prefs();
                }
                if let Some(hint) = update.hint {
                    self.hint = Some((hint.to_owned(), iced::time::Instant::now()));
                }
                // Structural changes are the runners' to make.
                for (key, command) in update.commands {
                    self.send_topology(&key, command);
                }
                let renaming = self.workspace.renaming_group.is_some()
                    || self.workspace.renaming_tab.is_some();
                if let Some(input) = focus_rename
                    && renaming
                {
                    return Task::batch([
                        closed,
                        iced::widget::operation::focus(input.clone()),
                        iced::widget::operation::select_all(input),
                    ]);
                }
                return closed;
            }
            Message::NewGraph(key) => {
                // The offline section offers no control: a graph for a
                // runner that is not there would not run either.
                let runner = if key.is_synthetic() { "" } else { key.as_str() };
                if let Some(graph) = self.create_graph(runner) {
                    self.pending_graph_focus = Some(graph);
                }
            }
            Message::EdgeConnected { from, to } => {
                // iced_nodegraph normalizes on_connect to (output, input), so
                // `from` is always the output pin and `to` the input pin. A pin
                // on a container belongs to a boundary node inside it: edges in
                // the store always connect real nodes.
                let Some((from_node, from_pin)) =
                    self.resolve_boundary(NodeId(from.node_id), &from.pin_id, true)
                else {
                    return Task::none();
                };
                let Some((to_node, to_pin)) =
                    self.resolve_boundary(NodeId(to.node_id), &to.pin_id, false)
                else {
                    return Task::none();
                };
                self.connect_edge(from_node, from_pin, to_node, to_pin);
                // The Display node's source changed, so what it should be
                // watching changed with it.
                #[cfg(not(target_arch = "wasm32"))]
                return self.reconcile_runtime();
            }
            Message::EdgeDisconnected { from, to } => {
                let Some((from_node, from_pin)) =
                    self.resolve_boundary(NodeId(from.node_id), &from.pin_id, true)
                else {
                    return Task::none();
                };
                let Some((to_node, to_pin)) =
                    self.resolve_boundary(NodeId(to.node_id), &to.pin_id, false)
                else {
                    return Task::none();
                };
                self.disconnect_edge(from_node, from_pin, to_node, to_pin);
                #[cfg(not(target_arch = "wasm32"))]
                return self.reconcile_runtime();
            }
            Message::ConnectRefused { from, to } => {
                // The widget says which pair, never why: it only knows that
                // `can_connect` said no. Re-running the rules here is what
                // turns that into a sentence, and it is the same call, so the
                // reason cannot disagree with the refusal.
                if let Some(reason) = self.refusal_sentence(&from, &to) {
                    self.hint = Some((reason, iced::time::Instant::now()));
                }
            }
            Message::OpenGraph(raw_id) => {
                // Only a real container can be opened; a stale button of a
                // deleted container must not open a tab with nothing in it.
                let target = NodeId(raw_id);
                if !self
                    .nodes
                    .get(&target)
                    .is_some_and(|node| node.is_container)
                {
                    return Task::none();
                }
                if self.workspace.show_graph(target) {
                    return Task::none();
                }
                self.pending_graph_focus = Some(target);
                // A running graph's views are its runner's tabs, shared with
                // every editor; without one, this window keeps its own.
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(key) = self.runner_of(target).map(RunnerKey::new)
                    && self.workspace.attached(&key)
                {
                    self.send_topology(
                        &key,
                        zeughaus_mux::TopologyCommand::OpenGraph { graph: raw_id },
                    );
                    return Task::none();
                }
                self.local_views.push(target);
            }
            Message::AutoLayout => {
                // Only this graph, and only by the wires it shows: an edge into
                // a subgraph is drawn on the container, so that is the endpoint
                // the layout has to rank by.
                let nodes: Vec<NodeId> = self
                    .node_order
                    .iter()
                    .filter(|id| {
                        self.nodes
                            .get(id)
                            .is_some_and(|node| node.parent == self.current_graph)
                    })
                    .copied()
                    .collect();
                let edges: Vec<(NodeId, NodeId)> = self
                    .edges
                    .iter()
                    .filter_map(|edge| {
                        let (from, _) = self.view_endpoint(
                            self.current_graph,
                            edge.from_node,
                            &edge.from_pin,
                            true,
                        )?;
                        let (to, _) = self.view_endpoint(
                            self.current_graph,
                            edge.to_node,
                            &edge.to_pin,
                            false,
                        )?;
                        Some((from, to))
                    })
                    .collect();
                for (id, position) in auto_layout(&nodes, &edges) {
                    if let Some(node) = self.nodes.get_mut(&id) {
                        node.position = position;
                    }
                    // A layout is a move like a drag: every window shows it.
                    #[cfg(not(target_arch = "wasm32"))]
                    self.push_move(id, position.x, position.y);
                }
            }
            Message::GroupMoved { node_ids, delta } => {
                // Fires once on drag release; persist the new positions.
                for raw_id in &node_ids {
                    let id = NodeId(*raw_id);
                    if let Some(node) = self.nodes.get_mut(&id) {
                        node.position =
                            Point::new(node.position.x + delta.x, node.position.y + delta.y);
                    }
                    #[cfg(not(target_arch = "wasm32"))]
                    if let Some(node) = self.nodes.get(&id) {
                        let (x, y) = (node.position.x, node.position.y);
                        self.push_move(id, x, y);
                    }
                }
            }
            Message::SelectionChanged(sel) => {
                self.selected = sel.into_iter().map(NodeId).collect();
            }
            Message::CloneNodes(ids) => {
                // Only the nodes the user selected: a container's contents come
                // with it, and cloning a selected child of a selected container
                // as well would duplicate it twice.
                let roots: Vec<NodeId> = ids
                    .iter()
                    .map(|raw_id| NodeId(*raw_id))
                    .filter(|id| self.nodes.contains_key(id))
                    .collect();
                let nested: HashSet<NodeId> =
                    roots.iter().flat_map(|id| self.descendants(*id)).collect();
                for root in roots {
                    if nested.contains(&root) {
                        continue;
                    }
                    self.clone_subtree(root, Vector::new(30.0, 30.0));
                }
            }
            Message::DeleteNodes(ids) => {
                // Edits still owed by a node that is about to go are dropped,
                // not committed: pushing a parameter onto a row the very next
                // reducer call deletes is work for nothing.
                #[cfg(not(target_arch = "wasm32"))]
                for raw_id in &ids {
                    let mut doomed = self.descendants(NodeId(*raw_id));
                    doomed.push(NodeId(*raw_id));
                    for id in doomed {
                        self.pending.take(id);
                    }
                }
                // What every other node still owes does go, before the store
                // changes shape underneath it.
                self.flush_pending();
                for raw_id in &ids {
                    let id = NodeId(*raw_id);
                    // The store deletes a container's contents with it, so this
                    // window has to as well or it would keep nodes no graph
                    // contains any more. One reducer call: the recursion lives
                    // in the module.
                    #[cfg(not(target_arch = "wasm32"))]
                    self.push_delete(id);
                    let parent = self.nodes.get(&id).map(|node| node.parent);
                    let mut doomed = self.descendants(id);
                    doomed.push(id);
                    for id in doomed {
                        if self.renaming.as_ref().is_some_and(|r| r.node == id) {
                            self.renaming = None;
                        }
                        self.nodes.remove(&id);
                        self.node_order.retain(|n| *n != id);
                        // Before the edges go: every wire that touched this
                        // node carried particles nobody can draw any more.
                        #[cfg(not(target_arch = "wasm32"))]
                        for edge in self
                            .edges
                            .iter()
                            .filter(|e| e.from_node == id || e.to_node == id)
                        {
                            self.runtime.particles.remove(&edge.id);
                        }
                        self.edges.retain(|e| e.from_node != id && e.to_node != id);
                        self.instances.remove(&id);
                        self.node_settings.remove(&id);
                        self.setting_errors.remove(&id);
                        #[cfg(not(target_arch = "wasm32"))]
                        self.runtime
                            .rejection_seq
                            .retain(|(node, _), _| *node != id);
                        self.cameras.remove(&id);
                    }
                    // A deleted boundary node is a pin its container loses.
                    if let Some(parent) = parent {
                        self.refresh_container_pins(parent);
                    }
                }
                // Once, after every doomed node is gone: rebuilding the index
                // inside the loop made deleting a large container O(nodes x
                // edges).
                self.reindex_edges();
                // A deleted Display node's feed has nobody left to draw it.
                #[cfg(not(target_arch = "wasm32"))]
                return self.reconcile_runtime();
            }
            Message::CameraChanged {
                graph,
                position,
                zoom,
            } => {
                self.cameras.insert(NodeId(graph), (position, zoom));
            }
            // Palette
            Message::TogglePalette => {
                // Current before it is shown; see `rebuild_palette`.
                #[cfg(not(target_arch = "wasm32"))]
                self.rebuild_palette();
                self.palette_open = !self.palette_open;
                if self.palette_open {
                    self.palette_input.clear();
                    self.palette_selected = 0;
                    return iced_palette::focus_input();
                }
            }
            Message::PaletteInput(input) => {
                self.palette_input = input;
                self.palette_selected = 0;
            }
            Message::PaletteSelect(idx) => {
                self.palette_selected = idx;
                if let Some(msg) = self.palette_confirm() {
                    return self.update(msg);
                }
            }
            Message::PaletteConfirm => {
                if let Some(msg) = self.palette_confirm() {
                    return self.update(msg);
                }
            }
            Message::PaletteCancel => {
                self.palette_close();
            }
            Message::PaletteNavigate(idx) => {
                self.palette_selected = idx;
            }
            Message::SpawnNode { type_id } => {
                if self.current_graph == NodeId(0) {
                    self.hint = Some((
                        "focus a graph pane to add nodes".to_owned(),
                        iced::time::Instant::now(),
                    ));
                    return Task::none();
                }
                let pos = self.viewport_center();
                self.spawn_node(&type_id, pos);
            }
            Message::SetTheme(name) => {
                // A name nothing answers to is a palette entry from a list
                // this window no longer holds, or a hand-written file: the
                // theme on screen stays rather than snapping to a default.
                if let Some(theme) = self.themes.iter().find(|theme| theme.name() == name) {
                    self.theme = theme.clone();
                    #[cfg(not(target_arch = "wasm32"))]
                    self.save_prefs();
                }
            }
            Message::NodeSettingChanged {
                node_id,
                key,
                value,
            } => {
                let id = NodeId(node_id);
                // What the store still holds. The commit compares against it
                // rather than against the previous keystroke, which is what
                // makes a run of characters one change.
                let was = self.setting_or_default(id, &key);
                #[cfg(not(target_arch = "wasm32"))]
                self.pending.touch(id, &key, &was, Instant::now());
                self.apply_setting(id, &key, value);
                // Renaming a boundary renames its container's pin. View-local,
                // so it does not wait.
                if key == "name" {
                    self.refresh_boundary_owner(id);
                }
                // Without a store there is nothing to hold back.
                #[cfg(target_arch = "wasm32")]
                self.settle_relations(id, &key, &was);
            }
            Message::RenameStart(raw) => {
                let id = NodeId(raw);
                if let Some(node) = self.nodes.get(&id) {
                    // A rename open on another node loses its draft: there is
                    // one field, and it follows the latest press.
                    self.renaming = Some(Rename {
                        node: id,
                        draft: node.display_name.clone(),
                    });
                    return Task::batch([
                        iced::widget::operation::focus(RENAME_INPUT),
                        iced::widget::operation::select_all(RENAME_INPUT),
                    ]);
                }
            }
            Message::RenameInput(input) => {
                if let Some(rename) = &mut self.renaming {
                    rename.draft = input;
                }
            }
            Message::RenameCommit => {
                let Some(Rename { node: id, draft }) = self.renaming.take() else {
                    return Task::none();
                };
                // A node deleted meanwhile, locally or by another window, takes
                // its rename with it.
                let Some(type_id) = self.nodes.get(&id).map(|node| node.type_id.clone()) else {
                    return Task::none();
                };
                // Nothing typed means the name the node was born with.
                let trimmed = draft.trim();
                let name = if trimmed.is_empty() {
                    self.catalog_name(&type_id)
                } else {
                    trimmed.to_owned()
                };
                if let Some(node) = self.nodes.get_mut(&id)
                    && node.display_name != name
                {
                    node.display_name.clone_from(&name);
                    #[cfg(not(target_arch = "wasm32"))]
                    self.push_rename(id, name);
                }
            }
            Message::Escape => {
                self.palette_close();
                self.renaming = None;
                self.workspace.renaming_group = None;
                self.workspace.renaming_tab = None;
                self.workspace.drag = None;
            }
            Message::NodeTriggered { node_id } => {
                // The press has to reach the one process that executes this
                // node's graph, which is never this one. Pushed straight to
                // that runner, so it works from any window, local or remote.
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let Some(endpoint) = self
                        .link_of(NodeId(node_id))
                        .map(|(_, link)| link.endpoint.clone())
                    else {
                        self.hint = Some((
                            "no runner executes this graph".to_owned(),
                            iced::time::Instant::now(),
                        ));
                        return Task::none();
                    };
                    return Task::perform(feed::trigger(endpoint, node_id), |result| {
                        if let Err(e) = result {
                            eprintln!("[trigger] {e}");
                        }
                        Message::Tick
                    });
                }
                // The wasm editor has no sync layer, so it has nobody to ask.
                #[cfg(target_arch = "wasm32")]
                let _ = node_id;
            }
            Message::NodeResized { node_id, size } => {
                // View-local, so no reducer: a size is what THIS user wants to
                // see, not part of the shared graph.
                self.node_sizes.insert(NodeId(node_id), size);
                // A drag only re-requests when it crosses a ladder tier; see
                // `feed::requested_box`.
                #[cfg(not(target_arch = "wasm32"))]
                return self.reconcile_runtime();
            }
            Message::WindowResized { size } => {
                // Only the palette reads it, and only when it spawns a node.
                self.window_size = size;
                #[cfg(not(target_arch = "wasm32"))]
                return iced::window::latest()
                    .and_then(iced::window::is_maximized)
                    .map(Message::WindowMaximized);
            }
            Message::WindowOpened { id, size } => {
                let scale = iced::window::scale_factor(id).map(Message::WindowRescaled);
                return Task::batch([self.update(Message::WindowResized { size }), scale]);
            }
            Message::WindowRescaled(scale_factor) => self.scale_factor = scale_factor,
            Message::Tick => {
                // Re-rendering advances the widget's animation clock; the one
                // piece of state that ages on its own is the hint.
                self.expire_hint();
            }
            Message::SyncPoll => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    // The editor's only clock while it is idle, so this is
                    // where a run of settings edits that has gone quiet
                    // reaches the store.
                    // The reconnect, on this window's own clock: nothing
                    // rebuilds the connection behind the editor's back.
                    if let Some(store) = &mut self.stdb {
                        store.poll();
                    }
                    self.commit_settled();
                    self.expire_hint();
                    self.drain_sync();
                    // Also where the runtime endpoint is noticed. A runtime
                    // announces its address by updating its own `runtime` row,
                    // and the sync layer reports inserts and deletes of that
                    // table but not updates -- so there is no event to wait for,
                    // and the client cache is read instead.
                    return self.reconcile_runtime();
                }
            }
            Message::CloseRequested => {
                // The last thing this window does. A settings edit is held
                // back for the debounce, so closing right after typing has to
                // push it before the process ends or the store never hears it.
                self.flush_pending();
                // The runner has to see this editor leave. A process that
                // exits with its QUIC connections open is, to the peer, one
                // that stopped answering: the control lease it held stays
                // attached until the idle timeout, and the next editor on
                // this machine types into a shell that will not take its
                // keys. A clean close is a CONNECTION_CLOSE, bounded by
                // weida's shutdown budget, and only then the exit.
                #[cfg(not(target_arch = "wasm32"))]
                {
                    return Task::perform(crate::transport::shutdown(), |()| Message::Exit);
                }
                #[cfg(target_arch = "wasm32")]
                return iced::exit();
            }
            // The window is undecorated, so the moves a system titlebar makes
            // are asked for here. `latest` because this process has one
            // window and never learns its id otherwise.
            Message::TitlebarPress => {
                self.titlebar_press = Some(self.titlebar_cursor);
            }
            Message::TitlebarMove(position) => {
                self.titlebar_cursor = position;
                if let Some(origin) = self.titlebar_press
                    && origin.distance(position) >= TITLEBAR_DRAG_THRESHOLD
                {
                    self.titlebar_press = None;
                    return iced::window::latest().and_then(iced::window::drag);
                }
            }
            Message::TitlebarRelease => self.titlebar_press = None,
            // A pointer that leaves the bar with the button still held is a
            // fast drag, not a click.
            Message::TitlebarExit => {
                if self.titlebar_press.take().is_some() {
                    return iced::window::latest().and_then(iced::window::drag);
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            Message::WindowResize(direction) => {
                return iced::window::latest()
                    .and_then(move |id| iced::window::drag_resize(id, direction));
            }
            #[cfg(not(target_arch = "wasm32"))]
            Message::WindowMinimize => {
                return iced::window::latest().and_then(|id| iced::window::minimize(id, true));
            }
            Message::WindowMaximize => {
                // The second click of a double-click is also a press: it must
                // not become a drag once the window is maximized.
                self.titlebar_press = None;
                return iced::window::latest().and_then(iced::window::toggle_maximize);
            }
            #[cfg(not(target_arch = "wasm32"))]
            Message::WindowMaximized(maximized) => {
                self.window_maximized = maximized;
            }
            #[cfg(not(target_arch = "wasm32"))]
            Message::WindowFocused(focused) => {
                self.window_focused = focused;
            }
            // Ends the runtime rather than closing the window: a closed
            // window leaves the event loop spinning with nothing to draw.
            #[cfg(not(target_arch = "wasm32"))]
            Message::Exit => return iced::exit(),
            // The same clean close as `CloseRequested`, with a restore file
            // written first and an `exec` instead of the exit.
            #[cfg(unix)]
            Message::Restart => {
                self.flush_pending();
                let path = restore::path();
                match restore::write(&path, &self.restore_state()) {
                    Ok(()) => {
                        return Task::perform(crate::transport::shutdown(), move |()| {
                            Message::RestartExec(path)
                        });
                    }
                    Err(e) => {
                        self.last_error =
                            format!("restart failed: cannot write {}: {e}", path.display());
                    }
                }
            }
            #[cfg(unix)]
            Message::RestartExec(path) => {
                let e = crate::restart::exec(&path);
                self.last_error = format!("restart failed: {e}");
            }
            Message::CopySessionId => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    if let Some(id) = &self.session_id {
                        self.last_error = format!("session token copied: {id}");
                        return iced::clipboard::write(id.clone());
                    }
                    self.last_error =
                        "no session (start `spacetime start` to host one)".to_string();
                }
            }
            // One of a runner's own terminals, shown in a tab or killed.
            // The answer is the next workspace snapshot, so nothing about the
            // structure is applied here.
            Message::AttachTerminal(key, terminal) => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    self.workspace.expect_new_tab(&key);
                    self.send_topology(
                        &key,
                        zeughaus_mux::TopologyCommand::AttachTerminal {
                            terminal,
                            target: zeughaus_mux::AttachTarget::NewTab,
                        },
                    );
                }
                #[cfg(target_arch = "wasm32")]
                let _ = (key, terminal);
            }
            Message::CloseTerminal(key, terminal) => {
                #[cfg(not(target_arch = "wasm32"))]
                self.send_topology(
                    &key,
                    zeughaus_mux::TopologyCommand::CloseTerminal { terminal },
                );
                #[cfg(target_arch = "wasm32")]
                let _ = (key, terminal);
            }
            Message::HoldRunner(key, held) => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let Some(endpoint) = self.runtime.links.get(&key).map(|l| l.endpoint.clone())
                    else {
                        self.last_error = "no such runner to hold".to_string();
                        return Task::none();
                    };
                    return Task::perform(feed::hold(endpoint, held), Message::HoldReplied);
                }
                #[cfg(target_arch = "wasm32")]
                let _ = (key, held);
            }
            // Said where the palette says everything else it did: a hold is
            // worth one line, and the run count is what the next one changes.
            #[cfg(not(target_arch = "wasm32"))]
            Message::HoldReplied(reply) => {
                self.last_error = match reply {
                    Ok(reply) if reply.held => {
                        format!("runner held; {} run(s) still live", reply.live_runs)
                    }
                    Ok(reply) => format!("runner released; {} run(s) live", reply.live_runs),
                    Err(e) => e,
                };
            }
            // File dialogs are native-only (rfd). On wasm these are no-ops;
            // persistence goes through the SpacetimeDB store instead.
            Message::SaveGraph => {
                #[cfg(all(feature = "remote", not(target_arch = "wasm32")))]
                if self.headless {
                    return self.refuse_file_dialog();
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let doc = self.to_document();
                    return Task::perform(
                        async move {
                            let file = rfd::AsyncFileDialog::new()
                                .set_title("Save Graph")
                                .add_filter("Zeughaus Graph", &["zgh"])
                                .add_filter("JSON", &["json"])
                                .save_file()
                                .await;
                            if let Some(handle) = file {
                                let json = serde_json::to_string_pretty(&doc).unwrap_or_default();
                                let _ = handle.write(json.as_bytes()).await;
                            }
                        },
                        |()| Message::PaletteCancel, // no-op after save
                    );
                }
            }
            Message::LoadGraph => {
                #[cfg(all(feature = "remote", not(target_arch = "wasm32")))]
                if self.headless {
                    return self.refuse_file_dialog();
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    return Task::perform(
                        async {
                            let file = rfd::AsyncFileDialog::new()
                                .set_title("Load Graph")
                                .add_filter("Zeughaus Graph", &["zgh"])
                                .add_filter("JSON", &["json"])
                                .pick_file()
                                .await;
                            if let Some(handle) = file {
                                let bytes = handle.read().await;
                                if let Ok(doc) = serde_json::from_slice::<GraphDocument>(&bytes) {
                                    return Some(doc);
                                }
                            }
                            None
                        },
                        |doc| {
                            if let Some(d) = doc {
                                Message::GraphLoaded(d)
                            } else {
                                Message::PaletteCancel // no-op on cancel
                            }
                        },
                    );
                }
            }
            Message::GraphLoaded(doc) => {
                self.load_document(doc);
                // `load_document` stopped every feed; this reopens what the new
                // document asks for.
                #[cfg(not(target_arch = "wasm32"))]
                return self.reconcile_runtime();
            }
            #[cfg(not(target_arch = "wasm32"))]
            Message::FeedFrame(frame) => {
                self.apply_feed_frame(frame);
            }
            #[cfg(not(target_arch = "wasm32"))]
            Message::Traffic(epoch, traffic) => {
                self.apply_traffic(epoch, traffic);
            }
            #[cfg(not(target_arch = "wasm32"))]
            Message::Mux(epoch, event) => {
                return self.apply_mux(epoch, event);
            }
            #[cfg(not(target_arch = "wasm32"))]
            Message::Terminal(epoch, terminal, event) => {
                return self.apply_terminal(epoch, terminal, event);
            }
            #[cfg(not(target_arch = "wasm32"))]
            Message::RowPage(epoch, terminal, page) => {
                self.apply_row_page(epoch, terminal, page);
            }
            #[cfg(not(target_arch = "wasm32"))]
            Message::TerminalAction(pane, action) => {
                return self.apply_terminal_action(pane, action);
            }
        }
        Task::none()
    }

    pub fn view(&self) -> Element<'_, Message, Theme> {
        // Borderless window: the edge grips restore the resize borders the
        // system decorations would have provided. The browser has no window
        // to resize.
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.window_maximized {
                return self.workspace_view();
            }
            stack![self.workspace_view(), resize_frame()].into()
        }
        #[cfg(target_arch = "wasm32")]
        self.workspace_view()
    }

    fn window_radius(&self) -> f32 {
        #[cfg(not(target_arch = "wasm32"))]
        if !self.window_maximized {
            return 10.0;
        }
        0.0
    }

    /// The window: the titlebar with the tab strip in it or a sidebar under
    /// it, the tab in front, the status line, and what a drag carries.
    fn workspace_view(&self) -> Element<'_, Message, Theme> {
        let strip = self.tab_strip();
        let (in_titlebar, sidebar) = match self.workspace.placement {
            iced_tabs::Placement::Top => (Some(strip), None),
            iced_tabs::Placement::Left => (None, Some(strip)),
        };

        let front = self.runner_tab_view();
        let body: Element<'_, Message, Theme> = match sidebar {
            Some(sidebar) => row![sidebar, front]
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
            None => front,
        };
        let body = container(body)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|theme: &Theme| container::Style {
                background: Some(theme.extended().background.base.color.into()),
                ..Default::default()
            });
        let body: Element<'_, Message, Theme> = if self.palette_open {
            stack![body, self.palette_overlay()].into()
        } else {
            body.into()
        };

        let window =
            column![self.titlebar(in_titlebar), body, self.status_bar()].height(Length::Fill);
        // A chip at the pointer naming what is being dragged.
        match self.workspace.drag.as_ref().filter(|drag| drag.active) {
            Some(drag) => {
                let chip = container(text(self.drag_label(&drag.source)).size(12))
                    .padding([2, 8])
                    .style(|theme: &Theme| container::Style {
                        background: Some(theme.chrome().into()),
                        border: iced::Border {
                            color: theme.extended().primary.base.color,
                            width: 1.0,
                            radius: 4.0.into(),
                        },
                        text_color: Some(theme.extended().background.base.text),
                        ..Default::default()
                    });
                stack![
                    window,
                    iced::widget::pin(chip)
                        .x(drag.cursor.x + 12.0)
                        .y(drag.cursor.y + 8.0)
                ]
                .into()
            }
            None => window.into(),
        }
    }

    /// What a drag chip says: the dragged tab's, group's or pane's title.
    fn drag_label(&self, source: &workspace::DragSource) -> String {
        let snapshot = |key: &RunnerKey| self.workspace.section(key).map(|s| &s.snapshot);
        match source {
            workspace::DragSource::Tab(tab) => snapshot(&tab.runner)
                .and_then(|s| s.tabs().find(|t| t.id == tab.tab))
                .map(|t| t.title.clone()),
            workspace::DragSource::Group(key, group) => snapshot(key)
                .and_then(|s| s.groups().find(|g| g.id == *group))
                .map(|g| g.name.clone()),
            workspace::DragSource::Pane(pane) => self
                .workspace
                .surface_of(pane)
                .map(|surface| self.pane_title(&pane.runner, surface)),
        }
        .unwrap_or_default()
    }

    /// What a pane is called in its grip: its graph's or terminal's title.
    fn pane_title(&self, key: &RunnerKey, surface: Surface) -> String {
        match surface {
            Surface::Graph(graph) => self
                .nodes
                .get(&NodeId(graph))
                .map_or_else(|| "Graph".to_owned(), |node| node.display_name.clone()),
            Surface::Empty => "Empty".to_owned(),
            Surface::Terminal(terminal) => {
                #[cfg(not(target_arch = "wasm32"))]
                if let Some((view, _)) = self.terminal_view(key, terminal) {
                    let guard = view.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(title) = guard.as_ref().map(|v| v.title.clone())
                        && !title.is_empty()
                    {
                        return title;
                    }
                }
                let _ = key;
                format!("terminal {}", terminal.0)
            }
        }
    }

    /// The tab tree: one section per runner, laid out for where the bar
    /// sits -- a row to live in the titlebar, or a column at the left edge.
    fn tab_strip(&self) -> Element<'_, Message, Theme> {
        use std::borrow::Cow;
        use workspace::{CollapseKey, Message as W};
        use zeughaus_mux::WorkspaceItem;

        let placement = self.workspace.placement;
        let left = placement == iced_tabs::Placement::Left;
        // A text field shrinks to nothing in a row that does not fill.
        let name_width = if left {
            Length::Fill
        } else {
            Length::Fixed(120.0)
        };
        let sections = self
            .workspace
            .sections()
            .iter()
            .map(|section| {
                let key = &section.key;
                let mut controls: Vec<(&'static str, Cow<'_, str>)> = Vec::new();
                if !section.synthetic() {
                    controls.push(("shell", Cow::Borrowed("+ Shell")));
                    controls.push(("graph", Cow::Borrowed("+ Graph")));
                    controls.push(("group", Cow::Borrowed("+ Group")));
                } else if key.as_str() == RunnerKey::LOCAL {
                    controls.push(("graph", Cow::Borrowed("+ Graph")));
                }
                let tab = |t| tab_entry(key, t);
                let items = section
                    .snapshot
                    .items
                    .iter()
                    .map(|item| match item {
                        WorkspaceItem::Tab(t) => iced_tabs::Item::Tab(tab(t)),
                        WorkspaceItem::Group(group) => {
                            let editor = self
                                .workspace
                                .renaming_group
                                .as_ref()
                                .filter(|(k, g, _)| k == key && *g == group.id)
                                .map(|(_, _, draft)| {
                                    iced::widget::text_input("Group name", draft)
                                        .id(GROUP_RENAME_INPUT)
                                        .size(12)
                                        .padding([1, 4])
                                        .width(name_width)
                                        .on_input(|text| {
                                            Message::Workspace(W::RenameGroupInput(text))
                                        })
                                        .on_submit(Message::Workspace(W::RenameGroupCommit))
                                        .into()
                                });
                            iced_tabs::Item::Group(iced_tabs::Group {
                                id: group.id,
                                label: Cow::Borrowed(group.name.as_str()),
                                color: workspace::rgba(group.color_rgba),
                                locked: group.locked,
                                collapsed: self
                                    .workspace
                                    .collapsed
                                    .contains(&CollapseKey::Group(key.clone(), group.id)),
                                tabs: group.tabs.iter().map(tab).collect(),
                                editor,
                            })
                        }
                    })
                    .collect();
                iced_tabs::Section {
                    id: key.clone(),
                    label: Cow::Borrowed(section.label.as_str()),
                    collapsed: self
                        .workspace
                        .collapsed
                        .contains(&CollapseKey::Section(key.clone())),
                    controls,
                    items,
                }
            })
            .collect();
        let marker = self
            .workspace
            .drag
            .as_ref()
            .filter(|drag| drag.active)
            .and_then(|drag| drag.marker.clone());
        let handlers = iced_tabs::Handlers {
            on_close: Box::new(|tab| Message::Workspace(W::CloseTab(tab))),
            on_toggle_section: Box::new(|key| Message::Workspace(W::ToggleSection(key))),
            on_toggle_group: Box::new(|key, group| Message::Workspace(W::ToggleGroup(key, group))),
            on_control: Box::new(|key, control| match control {
                "graph" => Message::NewGraph(key),
                "group" => Message::Workspace(W::NewGroup(key)),
                _ => Message::Workspace(W::NewShell(key)),
            }),
            on_press_tab: Box::new(|tab| Message::Workspace(W::PressTab(tab))),
            on_press_group: Box::new(|key, group| Message::Workspace(W::PressGroup(key, group))),
            on_hover: Box::new(|target| Message::Workspace(W::Hover(target))),
            on_rename_group: Box::new(|key, group| {
                Message::Workspace(W::RenameGroupStart(key, group))
            }),
            on_rename_tab: Box::new(|tab| Message::Workspace(W::RenameTabStart(tab))),
            on_cycle_color: Box::new(|key, group| {
                Message::Workspace(W::CycleGroupColor(key, group))
            }),
            on_dissolve_group: Box::new(|key, group| {
                Message::Workspace(W::DissolveGroup(key, group))
            }),
        };
        let renaming = self.workspace.renaming_tab.as_ref().map(|(tab, draft)| {
            let field: Element<'_, Message, Theme> = iced::widget::text_input("Tab name", draft)
                .id(TAB_RENAME_INPUT)
                .size(12)
                .padding([1, 4])
                .width(name_width)
                .on_input(|text| Message::Workspace(W::RenameTabInput(text)))
                .on_submit(Message::Workspace(W::RenameTabCommit))
                .into();
            (tab.clone(), field)
        });
        let tree = iced_tabs::tree(
            sections,
            self.workspace.active_tab().cloned(),
            placement,
            marker,
            renaming,
            handlers,
        );
        // Leaving the bar in the middle of a drag leaves no target in it.
        let tree = mouse_area(tree).on_exit(Message::Workspace(W::LeftBar));

        match placement {
            iced_tabs::Placement::Top => container(tree)
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
            // The same chrome the titlebar has, so a selected tab reads as the
            // content reaching into the bar, as it does on top.
            iced_tabs::Placement::Left => container(tree)
                .width(SIDEBAR_WIDTH)
                .height(Length::Fill)
                .style(|theme: &Theme| container::Style {
                    background: Some(theme.chrome().into()),
                    ..Default::default()
                })
                .into(),
        }
    }

    /// The window's own titlebar, drawn because the system's is turned off:
    /// the corner button that moves the tab bar between the top and the left
    /// edge, the tab strip when it sits on top, and the window buttons.
    /// Everything that is not a control drags the window; a double-click
    /// there maximizes it.
    fn titlebar<'a>(
        &'a self,
        tabs: Option<Element<'a, Message, Theme>>,
    ) -> Element<'a, Message, Theme> {
        let toggle = button(text("\u{2261}").size(14))
            .padding([2, 8])
            .style(|theme: &Theme, status| button::text(theme.base(), status))
            .on_press(Message::Workspace(workspace::Message::TogglePlacement));
        let mut controls = row![toggle]
            .spacing(4)
            .align_y(iced::Alignment::Center)
            .width(Length::Fill)
            .height(Length::Fill);
        controls = match tabs {
            Some(tabs) => controls.push(tabs),
            None => controls.push(space().width(Length::Fill)),
        };
        // The browser's window is the tab it runs in; only native windows
        // have anything to minimize.
        #[cfg(not(target_arch = "wasm32"))]
        {
            let corner = self.window_radius();
            controls = controls
                .push(window_button(
                    "\u{2013}",
                    Message::WindowMinimize,
                    |theme, status| button::text(theme.base(), status),
                ))
                .push(window_button(
                    "\u{25A1}",
                    Message::WindowMaximize,
                    |theme, status| button::text(theme.base(), status),
                ))
                .push(window_button(
                    "\u{00D7}",
                    Message::CloseRequested,
                    move |theme, status| {
                        let style = danger_on_hover(theme.base(), status, button::text);
                        // It sits in the window's corner, flush with both
                        // edges, so its hover is rounded where the window is.
                        button::Style {
                            border: iced::Border {
                                radius: iced::border::top_right(corner),
                                ..style.border
                            },
                            ..style
                        }
                    },
                ));
        }

        // Text and spacers do not capture the mouse, so the drag region
        // behind them stays reachable everywhere but on a control.
        let drag = mouse_area(space().width(Length::Fill).height(Length::Fill))
            .on_press(Message::TitlebarPress)
            .on_release(Message::TitlebarRelease)
            .on_move(Message::TitlebarMove)
            .on_exit(Message::TitlebarExit)
            .on_double_click(Message::WindowMaximize);
        container(
            stack![drag, controls]
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .height(TITLEBAR_HEIGHT)
        // Nothing on the right: the close button reaches the window's edge.
        .padding(iced::Padding {
            left: 4.0,
            ..iced::Padding::ZERO
        })
        .style(|theme: &Theme| container::Style {
            background: Some(theme.chrome().into()),
            border: iced::Border {
                radius: iced::border::top(self.window_radius()),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
    }

    /// The tab in front: its split panes, bare. Splitting and closing are
    /// palette commands on the focused pane.
    ///
    /// In the sidebar layout, a tab with more than one pane gives each a
    /// grip to drag it by; while a drag is on, every pane is a drop target
    /// whose side under the pointer lights up.
    fn runner_tab_view(&self) -> Element<'_, Message, Theme> {
        let Some((section, tab)) = self.workspace.active() else {
            return unavailable(
                "No tab",
                "Open a shell or a graph from a section of the tab bar.",
            );
        };
        let key = section.key.clone();
        let grips = tab.pane_count() > 1 && !section.synthetic();
        let drag = self.workspace.drag.as_ref().filter(|drag| drag.active);
        let lit = drag.and_then(|drag| match &drag.target {
            Some(DropTarget::Pane(pane, side)) => Some((pane.pane, *side)),
            _ => None,
        });
        pane_grid::PaneGrid::new(&tab.panes, move |pane, surface, _maximized| {
            let pane_ref = tab.pane_id(pane).map(|pane| PaneRef {
                runner: key.clone(),
                pane,
            });
            let body: Element<'_, Message, Theme> = match surface {
                Surface::Graph(graph) => self.graph_view(NodeId(*graph)),
                Surface::Empty => unavailable(
                    "Empty pane",
                    "Its surface could not be restored. Close it or split it again.",
                ),
                Surface::Terminal(terminal) => {
                    self.terminal_pane(pane_ref.clone(), &key, *terminal)
                }
            };
            // Always a stack with the body first: iced keeps a child's state
            // by its index, so the overlay coming and going with a drag
            // leaves the terminal selection and the graph's state alone.
            let mut layers = iced::widget::Stack::new().push(body);
            if let (Some(pane_ref), Some(_)) = (&pane_ref, drag) {
                let side = lit.filter(|(p, _)| *p == pane_ref.pane).map(|(_, s)| s);
                layers = layers.push(drop_overlay(pane_ref.clone(), side));
            }
            let body: Element<'_, Message, Theme> = layers.into();
            let content = pane_grid::Content::new(body);
            match pane_ref.filter(|_| grips) {
                Some(pane_ref) => {
                    content.title_bar(pane_grip(self.pane_title(&key, *surface), pane_ref))
                }
                None => content,
            }
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .spacing(2)
        .on_click(|pane| Message::Workspace(workspace::Message::ActivatePane(pane)))
        .on_resize(8, |event| {
            Message::Workspace(workspace::Message::Resize(event))
        })
        .into()
    }

    /// The one line at the bottom of every tab: the graph's size, the
    /// runtime and mux state, and either a hint the user just earned or the
    /// worst error the runtime reports. Workspace-wide, because "reconnecting"
    /// is as true in a terminal tab as in the graph.
    fn status_bar(&self) -> Element<'_, Message, Theme> {
        // Node errors are not computed here -- they arrive with the
        // runtime's published state, like every other value.
        let error = {
            let node_error = self.node_error_summary();
            if node_error.is_empty() {
                self.last_error.clone()
            } else {
                node_error
            }
        };
        // A hint outranks a failing node for the few seconds it lasts: it is
        // the answer to something the user just did, and a graph almost always
        // has something failing in it, which would leave the answer unread.
        let hint = self.hint.as_ref().map(|(text, _)| text.as_str());
        let head = format!(
            "  {} nodes | {} edges{}",
            self.nodes.len(),
            self.edges.len(),
            self.runtime_text(),
        );
        let (status_text, error_color) = if let Some(hint) = hint {
            (format!("{head} | {hint}"), self.theme.hint())
        } else if !error.is_empty() {
            (format!("{head} | ERROR: {error}"), self.theme.error())
        } else {
            (head, self.theme.muted())
        };

        container(text(status_text).size(12).color(error_color))
            .width(Length::Fill)
            .padding(4.0)
            .style(|theme: &Theme| container::Style {
                background: Some(theme.chrome().into()),
                border: iced::Border {
                    radius: iced::border::bottom(self.window_radius()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .into()
    }

    /// A terminal pane in the browser editor: the topology is the same, the
    /// terminal is not there. No PTY, no native transport, and nothing the
    /// user can type into.
    #[cfg(target_arch = "wasm32")]
    fn terminal_pane(
        &self,
        _pane: Option<PaneRef>,
        _key: &RunnerKey,
        _terminal: zeughaus_mux::TerminalId,
    ) -> Element<'_, Message, Theme> {
        unavailable(
            "Terminal",
            "Terminals run on the runner and are not shown in the browser editor.",
        )
    }

    fn graph_view(&self, graph: NodeId) -> Element<'_, Message, Theme> {
        // NodeGraph is generic over the id vocabulary declared by `GraphIds`;
        // the renderer stays at its default.
        let mut ng: NodeGraph<'_, GraphIds, Message, Theme> = NodeGraph::new();
        let (position, zoom) = self.camera(graph);

        ng = ng
            .on_connect(|from, to| Message::EdgeConnected { from, to })
            .on_disconnect(|from, to| Message::EdgeDisconnected { from, to })
            .on_move(|delta, node_ids| Message::GroupMoved { node_ids, delta })
            .on_select(Message::SelectionChanged)
            .on_clone(Message::CloneNodes)
            .on_delete(Message::DeleteNodes)
            .on_camera(move |position, zoom| Message::CameraChanged {
                graph: graph.0,
                position,
                zoom,
            })
            .on_resize(|node_id, size| Message::NodeResized { node_id, size })
            .camera(position, zoom)
            .can_connect({
                // With a custom can_connect, iced_nodegraph stops enforcing pin
                // direction itself, so every rule lives here -- and in
                // `wire_refusal`, which is the same rules and the sentence for
                // each of them. What this closure adds is the lookup: the pin
                // declarations, the occupancy the widget reports and the
                // reachability over the real, flat edges.
                let nodes = &self.nodes;
                let edge_index = &self.edge_index;
                let converters = &self.converters;
                move |from, to| {
                    let from_id = NodeId(*from.node_id());
                    let to_id = NodeId(*to.node_id());
                    let pin = |node: NodeId, name: &str| {
                        nodes.get(&node)?.pin_defs.iter().find(|p| &*p.name == name)
                    };
                    let closes_cycle = |from_is_output: bool| {
                        let (source, target) = if from_is_output {
                            (from_id, to_id)
                        } else {
                            (to_id, from_id)
                        };
                        flow_reaches(edge_index, target, source)
                    };
                    wire_refusal(&Wire {
                        same_node: from_id == to_id,
                        from: pin(from_id, from.pin_id().as_str()),
                        to: pin(to_id, to.pin_id().as_str()),
                        from_free: input_not_occupied(from),
                        to_free: input_not_occupied(to),
                        closes_cycle: &closes_cycle,
                        converters,
                    })
                    .is_none()
                }
            })
            .on_connect_refused(|from, to| Message::ConnectRefused { from, to });

        for id in &self.node_order {
            if let Some(node) = self.nodes.get(id) {
                // One graph per pane: a node of another one is not drawn
                // here, and its wires are mapped onto the container that
                // holds it.
                if node.parent != graph {
                    continue;
                }
                let content = build_node_element(
                    &self.theme,
                    node,
                    NodeChrome {
                        display: self.display_values.get(id),
                        settings: self.node_settings.get(id),
                        errors: self.setting_errors.get(id),
                        failure: self.node_error(*id),
                        dim_mask: self.dim_mask(*id, node),
                        size: self.node_sizes.get(id).copied(),
                        is_container: node.is_container,
                        rename: self
                            .renaming
                            .as_ref()
                            .filter(|r| r.node == *id)
                            .map(|r| r.draft.as_str()),
                    },
                );
                // Per-node activity feedback: red marching-ants on error. There
                // is no "working" state to draw -- this process does not
                // execute, so a node is never mid-run here.
                let errored = self.node_error(*id).is_some();
                let node_widget = ng_node(node.id.0, node.position, content)
                    // The editor's selection is the one that counts: it is
                    // what a restart restores and what delete and clone act on.
                    .selected(self.selected.contains(id))
                    .resizable(is_display(&node.type_id))
                    .style(move |theme: &Theme, status| {
                        let base = default_node_style(theme.base(), status);
                        if errored {
                            return NodeStyle {
                                corner_radius: 8.0,
                                opacity: 0.88,
                                border_color: theme.error().into(),
                                border_pattern: Pattern::dashed(2.0, 6.0, 4.0).flow(25.0),
                                ..base
                            };
                        }
                        match status {
                            NodeStatus::Selected => NodeStyle {
                                corner_radius: 8.0,
                                opacity: 0.88,
                                border_color: theme.accent().into(),
                                border_pattern: Pattern::solid(2.5),
                                ..base
                            },
                            NodeStatus::Idle => NodeStyle {
                                corner_radius: 8.0,
                                opacity: 0.88,
                                ..base
                            },
                        }
                    })
                    .pin_style(
                        |theme: &Theme, pin: &PinInfo<'_, GraphIds>, _other, status| PinStyle {
                            color: pin.info().color.into(),
                            shape: pin.info().shape,
                            ..default_pin_style(theme.base(), status)
                        },
                    );
                ng = ng.push_node(node_widget);
            }
        }

        for edge in &self.edges {
            // Both ends mapped into this graph first: an edge into a subgraph is
            // drawn on the container's pin, and one that touches neither this
            // graph nor its containers is not drawn at all.
            let (Some((from_node, from_pin)), Some((to_node, to_pin))) = (
                self.view_endpoint(graph, edge.from_node, &edge.from_pin, true),
                self.view_endpoint(graph, edge.to_node, &edge.to_pin, false),
            ) else {
                continue;
            };

            // Edge color follows the source pin's type, dimmed while that output
            // carries nothing: an edge is only as live as the value on it.
            let edge_color = self
                .nodes
                .get(&from_node)
                .and_then(|n| n.pin_defs.iter().find(|p| &*p.name == from_pin.as_str()))
                .map(|p| pin_color(&self.theme, &p.ty))
                .unwrap_or_else(|| self.theme.muted());
            // Read on the real source pin, not the mapped one: a container has
            // no outputs of its own, so the value lives on the boundary node.
            let edge_color = if self
                .output_value(edge.from_node, edge.from_pin.as_str())
                .is_some()
            {
                edge_color
            } else {
                dim(edge_color)
            };

            // An edge reflects its source node's state: red marching-ants when
            // the source errored (broken data).
            let src_error = self.node_error(edge.from_node).is_some();

            // Transmission mode is decided by the target pin: a Trigger input
            // carries Events (animated flowing dash), a Sample input carries
            // State (calm solid line).
            let is_event = self
                .nodes
                .get(&to_node)
                .and_then(|n| n.pin_defs.iter().find(|p| &*p.name == to_pin.as_str()))
                .map(|p| p.pin_kind == PinKind::Trigger)
                .unwrap_or(false);

            let edge_widget = ng_edge(
                edge.id,
                PinRef::new(from_node.0, from_pin),
                PinRef::new(to_node.0, to_pin),
            )
            .style(move |theme: &Theme, status, _start, _end| {
                if src_error {
                    return EdgeStyle::error(theme.base(), status);
                }
                let base = default_edge_style(theme.base(), status);
                EdgeStyle {
                    stroke_color: edge_color.into(),
                    // Event edges flow; state edges stay solid.
                    pattern: if is_event {
                        Pattern::dashed(2.0, 7.0, 5.0).flow(20.0)
                    } else {
                        base.pattern
                    },
                    ..base
                }
            });
            // One dot per value the runtime delivered across this edge. Keyed
            // by edge id, so a wire that carries an unchanged value still shows
            // the traffic on it -- which the colour alone cannot.
            #[cfg(not(target_arch = "wasm32"))]
            let edge_widget = {
                let born = self.runtime.particles.get(&edge.id).into_iter().flatten();
                edge_widget.particles(born.map(move |born| {
                    particle(*born, PARTICLE_SPEED).style(move |theme: &Theme| ParticleStyle {
                        color: edge_color,
                        ..default_particle_style(theme.base())
                    })
                }))
            };
            ng = ng.push_edge(edge_widget);
        }

        container(ng)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    /// The command palette over whatever the window shows: it acts on the
    /// whole editor (tabs, panes, terminals, the graph), so it belongs to no
    /// single surface.
    fn palette_overlay(&self) -> Element<'_, Message, Theme> {
        // The palette widget is written against iced's own theme type, so
        // the overlay is handed the inner theme: one theme still decides
        // what it looks like, through the bridge rather than through a
        // second catalog.
        let palette_view: Element<'_, Message, Theme> = iced::widget::themer(
            Some(self.theme.base().clone()),
            palette::view(
                &self.palette_input,
                &self.palette_commands,
                self.palette_selected,
            ),
        )
        .into();
        container(palette_view)
            .width(Length::Fill)
            .padding(80.0)
            .align_x(iced::Alignment::Center)
            .into()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let events = iced::event::listen_with(|event, _status, id| {
            match event {
                Event::Window(iced::window::Event::Opened { size, .. }) => {
                    return Some(Message::WindowOpened { id, size });
                }
                Event::Window(iced::window::Event::Resized(size)) => {
                    return Some(Message::WindowResized { size });
                }
                Event::Window(iced::window::Event::Rescaled(scale_factor)) => {
                    return Some(Message::WindowRescaled(scale_factor));
                }
                #[cfg(not(target_arch = "wasm32"))]
                Event::Window(iced::window::Event::Focused) => {
                    return Some(Message::WindowFocused(true));
                }
                #[cfg(not(target_arch = "wasm32"))]
                Event::Window(iced::window::Event::Unfocused) => {
                    return Some(Message::WindowFocused(false));
                }
                _ => {}
            }
            if let Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = event {
                if is_palette_shortcut(&key, modifiers) {
                    return Some(Message::TogglePalette);
                }
                if let Some(step) = tab_step(&key, modifiers) {
                    return Some(Message::Workspace(workspace::Message::CycleTab(step)));
                }

                // Ctrl+S = Save, Ctrl+O = Load
                if (modifiers.control() || modifiers.command())
                    && let keyboard::Key::Character(ref c) = key
                {
                    match c.as_str() {
                        "s" => return Some(Message::SaveGraph),
                        "o" => return Some(Message::LoadGraph),
                        _ => {}
                    }
                }

                match key {
                    keyboard::Key::Named(keyboard::key::Named::Escape) => {
                        return Some(Message::Escape);
                    }
                    keyboard::Key::Named(keyboard::key::Named::Enter) => {
                        return Some(Message::PaletteConfirm);
                    }
                    _ => {}
                }
            }
            None
        });

        #[cfg_attr(target_arch = "wasm32", allow(unused_mut))]
        let mut subs = vec![events];
        subs.push(iced::window::close_requests().map(|_| Message::CloseRequested));

        // Always subscribed, in a window and headless alike: without the
        // handler, SIGUSR1's default action ends the process.
        #[cfg(unix)]
        subs.push(Subscription::run(crate::restart::sigusr1).map(|()| Message::Restart));

        // The library only auto-redraws for animated edges, not node borders.
        // While any node is in error, drive ~30fps redraws so its marching-ants
        // border animates. Native only: iced::time::every needs the tokio
        // executor feature.
        #[cfg(not(target_arch = "wasm32"))]
        if !self.failing_nodes().is_empty() {
            subs.push(
                iced::time::every(std::time::Duration::from_millis(33)).map(|_| Message::Tick),
            );
        }

        // A hint ages out on its own, so it needs a clock of its own: the
        // error tick only runs while a node is failing, and the sync poll only
        // while a store is connected.
        #[cfg(not(target_arch = "wasm32"))]
        if self.hint.is_some() {
            subs.push(
                iced::time::every(std::time::Duration::from_millis(250)).map(|_| Message::Tick),
            );
        }

        // Drain remote sync events on a steady cadence while connected.
        #[cfg(not(target_arch = "wasm32"))]
        if self.stdb.is_some() {
            subs.push(
                iced::time::every(std::time::Duration::from_millis(100)).map(|_| Message::SyncPoll),
            );
        }

        // A split drag holds its newest ratio back; without a clock the last
        // one of a gesture would never be sent, and the runner would keep a
        // ratio from the middle of the drag.
        #[cfg(not(target_arch = "wasm32"))]
        if self.workspace.resize_pending() {
            subs.push(
                iced::time::every(std::time::Duration::from_millis(100))
                    .map(|_| Message::Workspace(workspace::Message::FlushResize)),
            );
        }

        // The release of a press on a tab, a group header or a pane grip is
        // followed wherever it happens, over widgets that capture it too.
        // Always listened for: a fast click delivers the press and its
        // release in one batch, before the press has made any subscription
        // that depends on it. Without a press the release changes nothing.
        subs.push(iced::event::listen_with(
            |event, _status, _id| match event {
                Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left))
                | Event::Touch(iced::touch::Event::FingerLifted { .. }) => {
                    Some(Message::Workspace(workspace::Message::DragReleased))
                }
                Event::Touch(iced::touch::Event::FingerLost { .. }) => {
                    Some(Message::Workspace(workspace::Message::DragCancelled))
                }
                _ => None,
            },
        ));
        // The pointer is followed only while a press is held.
        if self.workspace.drag.is_some() {
            subs.push(iced::event::listen_with(
                |event, _status, _id| match event {
                    Event::Mouse(iced::mouse::Event::CursorMoved { position })
                    | Event::Touch(iced::touch::Event::FingerMoved { position, .. }) => {
                        Some(Message::Workspace(workspace::Message::DragMoved(position)))
                    }
                    Event::Keyboard(keyboard::Event::KeyPressed {
                        key: keyboard::Key::Named(keyboard::key::Named::Escape),
                        ..
                    }) => Some(Message::Workspace(workspace::Message::DragCancelled)),
                    _ => None,
                },
            ));
        }

        Subscription::batch(subs)
    }

    /// What every surface draws with. Cloned per frame, which is one atomic
    /// bump: the theme itself is shared.
    pub fn theme(&self) -> Theme {
        self.theme.clone()
    }
}

/// Ctrl+Space everywhere; on macOS also Cmd+Shift+P, because Ctrl+Space
/// switches the input source there by default.
pub(crate) fn is_palette_shortcut(key: &keyboard::Key, modifiers: keyboard::Modifiers) -> bool {
    is_toggle_shortcut(key, modifiers)
        || (cfg!(target_os = "macos")
            && modifiers.logo()
            && modifiers.shift()
            && !modifiers.control()
            && matches!(key, keyboard::Key::Character(c) if c.eq_ignore_ascii_case("p")))
}

/// Ctrl+PageDown and Ctrl+PageUp: the next and the previous tab, as in a
/// browser.
fn tab_step(key: &keyboard::Key, modifiers: keyboard::Modifiers) -> Option<isize> {
    if !modifiers.control() || modifiers.shift() || modifiers.alt() || modifiers.logo() {
        return None;
    }
    match key {
        keyboard::Key::Named(keyboard::key::Named::PageDown) => Some(1),
        keyboard::Key::Named(keyboard::key::Named::PageUp) => Some(-1),
        _ => None,
    }
}

/// The keys that belong to the window even while a terminal has the
/// keyboard: they never reach the child.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn is_app_shortcut(key: &keyboard::Key, modifiers: keyboard::Modifiers) -> bool {
    is_palette_shortcut(key, modifiers) || tab_step(key, modifiers).is_some()
}

/// A pane with nothing to draw: what it should hold, and why it does not.
/// Centred and plain, so an unreachable surface reads as a state and not as
/// a broken layout.
fn unavailable<'a>(what: &'a str, why: &'a str) -> Element<'a, Message, Theme> {
    container(column![text(what).size(18), text(why).size(12)].spacing(6))
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(iced::Alignment::Center)
        .align_y(iced::Alignment::Center)
        .into()
}

/// One tab of a section as the tab tree shows it.
fn tab_entry<'a>(
    key: &RunnerKey,
    tab: &'a zeughaus_mux::TabSnapshot,
) -> iced_tabs::Tab<'a, TabRef> {
    let mut entry = iced_tabs::Tab::new(
        TabRef {
            runner: key.clone(),
            tab: tab.id,
        },
        tab.title.as_str(),
    );
    if let Some(accent) = workspace::accent(tab) {
        entry = entry.accent(accent);
    }
    entry
}

/// The strip above a pane of a split tab: its title, and a grip that starts
/// dragging the pane.
fn pane_grip<'a>(title: String, pane: PaneRef) -> pane_grid::TitleBar<'a, Message, Theme> {
    let grip = mouse_area(text("\u{283F}").size(12))
        .on_press(Message::Workspace(workspace::Message::PressPane(pane)))
        .interaction(iced::mouse::Interaction::Grab);
    let strip = row![grip, text(title).size(11)]
        .spacing(6)
        .align_y(iced::Alignment::Center);
    pane_grid::TitleBar::new(
        container(strip)
            .height(PANE_GRIP_HEIGHT)
            .padding([0, 6])
            .align_y(iced::Alignment::Center),
    )
    .style(|theme: &Theme| container::Style {
        background: Some(theme.chrome().into()),
        text_color: Some(theme.muted()),
        ..Default::default()
    })
}

/// What covers a pane while something is dragged: it reports which side of
/// the pane the pointer is near, and lights that half up.
fn drop_overlay<'a>(
    pane: PaneRef,
    side: Option<zeughaus_mux::Side>,
) -> Element<'a, Message, Theme> {
    use zeughaus_mux::Side;
    iced::widget::responsive(move |size| {
        let lit: Element<'a, Message, Theme> = match side {
            None => space().into(),
            Some(side) => {
                let (width, height) = match side {
                    Side::Left | Side::Right => (Length::FillPortion(1), Length::Fill),
                    Side::Top | Side::Bottom => (Length::Fill, Length::FillPortion(1)),
                };
                let block =
                    container(space())
                        .width(width)
                        .height(height)
                        .style(|theme: &Theme| container::Style {
                            background: Some(
                                theme.extended().primary.base.color.scale_alpha(0.35).into(),
                            ),
                            ..Default::default()
                        });
                // Half of the pane: the lit block beside an empty one.
                match side {
                    Side::Left => row![block, space().width(Length::FillPortion(1))].into(),
                    Side::Right => row![space().width(Length::FillPortion(1)), block].into(),
                    Side::Top => column![block, space().height(Length::FillPortion(1))].into(),
                    Side::Bottom => column![space().height(Length::FillPortion(1)), block].into(),
                }
            }
        };
        let hover = pane.clone();
        mouse_area(container(lit).width(Length::Fill).height(Length::Fill))
            .on_move(move |at| {
                Message::Workspace(workspace::Message::HoverPane(
                    hover.clone(),
                    workspace::side_at(at, size),
                ))
            })
            .on_exit(Message::Workspace(workspace::Message::LeftPane(
                pane.clone(),
            )))
            .into()
    })
    .into()
}

/// A titlebar button: minimize, maximize, close. Flat, so the bar reads as
/// one strip and not as a row of boxes; as tall as the bar, so a hover fills
/// it from edge to edge.
#[cfg(not(target_arch = "wasm32"))]
fn window_button(
    glyph: &'static str,
    message: Message,
    style: impl Fn(&Theme, button::Status) -> button::Style + 'static,
) -> Element<'static, Message, Theme> {
    button(
        text(glyph)
            .size(13)
            .center()
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .width(TITLEBAR_HEIGHT)
    .height(Length::Fill)
    .padding(0)
    .style(style)
    .on_press(message)
    .into()
}

/// The style of a button that closes or deletes something: `rest` until the
/// pointer is on it, iced's danger style while hovered or pressed. Red at rest
/// would shout from every node and every titlebar.
fn danger_on_hover(
    theme: &iced::Theme,
    status: button::Status,
    rest: fn(&iced::Theme, button::Status) -> button::Style,
) -> button::Style {
    match status {
        button::Status::Hovered | button::Status::Pressed => button::danger(theme, status),
        button::Status::Active | button::Status::Disabled => rest(theme, status),
    }
}

/// Invisible hit zones along the window edges and corners that start a
/// native resize drag. An undecorated window has no system resize borders,
/// so the editor draws its own on top of the whole view.
#[cfg(not(target_arch = "wasm32"))]
fn resize_frame() -> Element<'static, Message, Theme> {
    use iced::mouse::Interaction;
    use iced::window::Direction;

    /// Thickness of the edge strips.
    const EDGE: f32 = 5.0;
    /// Reach of the corner grips along each edge.
    const CORNER: f32 = 16.0;

    fn grip(
        width: impl Into<Length>,
        height: impl Into<Length>,
        direction: Direction,
        cursor: Interaction,
    ) -> Element<'static, Message, Theme> {
        mouse_area(space().width(width).height(height))
            .on_press(Message::WindowResize(direction))
            .interaction(cursor)
            .into()
    }

    column![
        row![
            grip(
                CORNER,
                EDGE,
                Direction::NorthWest,
                Interaction::ResizingDiagonallyDown
            ),
            grip(
                Length::Fill,
                EDGE,
                Direction::North,
                Interaction::ResizingVertically
            ),
            grip(
                CORNER,
                EDGE,
                Direction::NorthEast,
                Interaction::ResizingDiagonallyUp
            ),
        ]
        .width(Length::Fill),
        row![
            column![
                grip(
                    EDGE,
                    CORNER,
                    Direction::NorthWest,
                    Interaction::ResizingDiagonallyDown
                ),
                grip(
                    EDGE,
                    Length::Fill,
                    Direction::West,
                    Interaction::ResizingHorizontally
                ),
                grip(
                    EDGE,
                    CORNER,
                    Direction::SouthWest,
                    Interaction::ResizingDiagonallyUp
                ),
            ]
            .height(Length::Fill),
            space().width(Length::Fill),
            column![
                grip(
                    EDGE,
                    CORNER,
                    Direction::NorthEast,
                    Interaction::ResizingDiagonallyUp
                ),
                grip(
                    EDGE,
                    Length::Fill,
                    Direction::East,
                    Interaction::ResizingHorizontally
                ),
                grip(
                    EDGE,
                    CORNER,
                    Direction::SouthEast,
                    Interaction::ResizingDiagonallyDown
                ),
            ]
            .height(Length::Fill),
        ]
        .width(Length::Fill)
        .height(Length::Fill),
        row![
            grip(
                CORNER,
                EDGE,
                Direction::SouthWest,
                Interaction::ResizingDiagonallyUp
            ),
            grip(
                Length::Fill,
                EDGE,
                Direction::South,
                Interaction::ResizingVertically
            ),
            grip(
                CORNER,
                EDGE,
                Direction::SouthEast,
                Interaction::ResizingDiagonallyDown
            ),
        ]
        .width(Length::Fill),
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}
