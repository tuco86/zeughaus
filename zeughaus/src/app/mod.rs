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
/// What the runtime reported: values, failures, feeds, particles.
mod runtime;
/// The shared store: rows in, edits out.
#[cfg(not(target_arch = "wasm32"))]
mod store;
/// The runner's terminals and the panes that show them.
#[cfg(not(target_arch = "wasm32"))]
mod terminal;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

use iced::keyboard;
use iced::widget::{button, column, container, pane_grid, row, stack, text};
use iced::{Color, Element, Event, Length, Point, Subscription, Task, Theme, Vector};
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
use zeughaus_transform::TransformPlugin;

use graph::{EditorEdge, EditorNode, NodeEdges, Wire, ancestry, flow_reaches, wire_refusal};
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
use crate::workspace::{self, Surface, Workspace};
// The browser editor draws no terminal, so the pane says so by name rather
// than by the title a terminal would have reported.
#[cfg(target_arch = "wasm32")]
use crate::workspace::surface_title;

/// How long a status-bar hint stays. Long enough to read one sentence after
/// the drop that produced it, short enough that it is gone before the next
/// thing the user tries.
const HINT_LIFETIME: std::time::Duration = std::time::Duration::from_secs(3);

pub struct App {
    workspace: Workspace,
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
    camera_position: Point,
    camera_zoom: f32,
    /// Which container's contents are on screen; `NodeId(0)` is the root
    /// graph. Editor-local: what one window looks at is not shared state.
    current_graph: NodeId,
    /// Camera per graph, so stepping out of a subgraph returns to the view it
    /// was entered from instead of resetting.
    cameras: HashMap<NodeId, (Point, f32)>,

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

    /// What the window is showing, in logical pixels. Reported on every
    /// resize; the initial value is the size `main` asks for, because no
    /// `Resized` event arrives until the window changes.
    window_size: iced::Size,

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
    /// The runner's terminal multiplexer, while one is reachable. `None`
    /// leaves the workspace graph-only and every terminal action refused.
    #[cfg(not(target_arch = "wasm32"))]
    mux: Option<MuxState>,
    /// Which attachment the events on screen came from. Bumped whenever the
    /// control task is replaced, so a message the old one queued is
    /// recognizable -- the same guard as `RuntimeView::traffic_epoch`.
    #[cfg(not(target_arch = "wasm32"))]
    mux_epoch: u64,
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

        Self {
            workspace: Workspace::new(),
            nodes: HashMap::new(),
            node_order: Vec::new(),
            edges: Vec::new(),
            edge_index: HashMap::new(),
            instances: HashMap::new(),
            selected: HashSet::new(),
            camera_position: Point::ORIGIN,
            camera_zoom: 1.0,
            current_graph: NodeId(0),
            cameras: HashMap::new(),
            plugins,
            palette_commands: palette::build_commands(&catalog),
            catalog,
            converters,
            display_values: HashMap::new(),
            node_sizes: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            pending_edges: Vec::new(),
            node_settings: HashMap::new(),
            window_size: crate::WINDOW_SIZE,
            last_error: String::new(),
            hint: None,
            palette_open: false,
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
            // No mux until a runtime announces where it serves; the
            // workspace stays graph-only until one attaches.
            #[cfg(not(target_arch = "wasm32"))]
            mux: None,
            #[cfg(not(target_arch = "wasm32"))]
            mux_epoch: 0,
        }
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
    fn send_topology(&mut self, _command: zeughaus_mux::TopologyCommand) {
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
            1 => " | runtime connected".to_string(),
            // The store elects the lowest `seq`; the spares are standby.
            n => format!(" | runtime connected (1 of {n} executing)"),
        };
        // Whether values are arriving, not just whether a runner exists: the
        // event subscription is what carries them, and a dropped one leaves
        // every number on screen a leftover.
        if self.runtimes > 0 {
            text.push_str(if self.runtime.traffic_live {
                " | live"
            } else {
                " | reconnecting"
            });
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
        match self.mux.as_ref() {
            Some(mux) if mux.attached => text.push_str(" | mux: attached"),
            Some(_) => text.push_str(" | mux: reconnecting"),
            None if self.runtimes > 0 => text.push_str(" | no mux"),
            None => {}
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
        Point::new(
            self.window_size.width * 0.5 / self.camera_zoom - self.camera_position.x,
            self.window_size.height * 0.5 / self.camera_zoom - self.camera_position.y,
        )
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

    pub fn update(&mut self, message: Message) -> Task<Message> {
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
                let update = self.workspace.update(message);
                if let Some(hint) = update.hint {
                    self.hint = Some((hint.to_owned(), iced::time::Instant::now()));
                }
                // Structural changes are the runner's to make.
                for command in update.commands {
                    self.send_topology(command);
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
            Message::EnterGraph(raw_id) => {
                let target = NodeId(raw_id);
                if target == self.current_graph {
                    return Task::none();
                }
                // Only the root graph and a real container can be entered; a
                // stale breadcrumb of a deleted container must not strand the
                // view in a graph with nothing in it.
                let enterable = target == NodeId(0)
                    || self
                        .nodes
                        .get(&target)
                        .is_some_and(|node| node.is_container);
                if !enterable {
                    return Task::none();
                }
                self.cameras
                    .insert(self.current_graph, (self.camera_position, self.camera_zoom));
                self.current_graph = target;
                let (position, zoom) = self
                    .cameras
                    .get(&target)
                    .copied()
                    .unwrap_or((Point::ORIGIN, 1.0));
                self.camera_position = position;
                self.camera_zoom = zoom;
                // A selection from another graph is not visible here, and a
                // delete would act on nodes the user can no longer see.
                self.selected.clear();
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
                        let (from, _) = self.view_endpoint(edge.from_node, &edge.from_pin, true)?;
                        let (to, _) = self.view_endpoint(edge.to_node, &edge.to_pin, false)?;
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
                        // Looking into a graph that no longer exists shows
                        // nothing and offers no way out.
                        if self.current_graph == id {
                            self.current_graph = NodeId(0);
                        }
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
            Message::CameraChanged { position, zoom } => {
                self.camera_position = position;
                self.camera_zoom = zoom;
            }
            // Palette
            Message::TogglePalette => {
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
                let pos = self.viewport_center();
                self.spawn_node(&type_id, pos);
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
            Message::NodeTriggered { node_id } => {
                // The press has to reach the one process that executes, which
                // is never this one. Pushed straight to that runtime, so it
                // works from any window, local or remote.
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(endpoint) = self.runtime.endpoint.clone() {
                    return Task::perform(feed::trigger(endpoint, node_id), |result| {
                        if let Err(e) = result {
                            eprintln!("[trigger] {e}");
                        }
                        Message::Tick
                    });
                } else {
                    // The status bar already says "no runtime"; this names the
                    // press that went nowhere.
                    eprintln!("[trigger] no runtime");
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
            }
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
            // Ends the runtime rather than closing the window: a closed
            // window leaves the event loop spinning with nothing to draw.
            #[cfg(not(target_arch = "wasm32"))]
            Message::Exit => return iced::exit(),
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
            // File dialogs are native-only (rfd). On wasm these are no-ops;
            // persistence goes through the SpacetimeDB store instead.
            Message::SaveGraph => {
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

    pub fn view(&self) -> Element<'_, Message> {
        self.workspace_view()
    }

    fn workspace_view(&self) -> Element<'_, Message> {
        let tab_count = self.workspace.tabs().len();
        let tabs = self.workspace.tabs().iter().map(|tab| {
            let mut entry = iced_tabs::Tab::new(tab.id, tab.title.as_str()).closable(tab_count > 1);
            if let Some(group) = tab.group.as_deref() {
                entry = entry.group(group);
            }
            if let Some(accent) = workspace::accent(tab) {
                entry = entry.accent(accent);
            }
            entry
        });
        let tab_bar = iced_tabs::view(
            tabs,
            self.workspace.active_tab(),
            self.workspace.placement,
            |id| Message::Workspace(workspace::Message::ActivateTab(id)),
            |id| Message::Workspace(workspace::Message::CloseTab(id)),
        );

        let panes: Element<'_, Message> = match self.workspace.active() {
            // Only reachable between a snapshot and its rebuild, which does
            // not happen: a workspace always has its active tab built.
            None => container(text("No tab").size(14))
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
            Some(tab) => {
                let pane_count = tab.panes.len();
                let focused = self.workspace.focused_pane();
                pane_grid::PaneGrid::new(&tab.panes, move |pane, surface, _maximized| {
                    let id = tab.pane_id(pane);
                    let body: Element<'_, Message> = match surface {
                        Surface::Graph => self.graph_view(),
                        Surface::Empty => unavailable(
                            "Empty pane",
                            "Its surface could not be restored. Close it or split it again.",
                        ),
                        Surface::Terminal(terminal) => self.terminal_pane(id, *terminal),
                    };

                    // Structural changes are the runner's; with none
                    // attached the buttons are dead rather than a click that
                    // earns a refusal.
                    let attached = self.workspace.attached();
                    let mut controls = row![
                        split_button("H", attached, pane, zeughaus_mux::Axis::Horizontal),
                        split_button("V", attached, pane, zeughaus_mux::Axis::Vertical),
                    ]
                    .spacing(2);
                    // The graph pane is unique and the runner refuses to
                    // close it; offering the button would only earn a hint.
                    if pane_count > 1 && *surface != Surface::Graph {
                        let mut close = button(text("x").size(11)).padding([3, 6]);
                        if attached {
                            close = close
                                .on_press(Message::Workspace(workspace::Message::ClosePane(pane)));
                        }
                        controls = controls.push(close);
                    }
                    let controls: Element<'_, Message> = controls.into();
                    let title_color = if id.is_some() && id == focused {
                        Color::from_rgb(0.45, 0.7, 1.0)
                    } else {
                        Color::from_rgb(0.65, 0.65, 0.68)
                    };
                    let title = self.pane_title(*surface);
                    let title_bar =
                        pane_grid::TitleBar::new(text(title).size(12).color(title_color))
                            .controls(controls)
                            .padding([3, 5]);

                    pane_grid::Content::new(body).title_bar(title_bar)
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
        };

        let mut add_tab = button(text("+").size(14)).padding([4, 8]);
        if self.workspace.attached() {
            add_tab = add_tab.on_press(Message::Workspace(workspace::Message::NewTab));
        }
        let placement_label = match self.workspace.placement {
            iced_tabs::Placement::Top => "Tabs left",
            iced_tabs::Placement::Left => "Tabs top",
        };
        let toggle_placement = button(text(placement_label).size(11))
            .padding([5, 8])
            .on_press(Message::Workspace(workspace::Message::TogglePlacement));

        let status_bar = self.status_bar();
        match self.workspace.placement {
            iced_tabs::Placement::Top => column![
                row![tab_bar, add_tab, toggle_placement]
                    .spacing(4)
                    .align_y(iced::Alignment::Center),
                panes,
                status_bar,
            ]
            .height(Length::Fill)
            .into(),
            iced_tabs::Placement::Left => column![
                row![
                    column![row![add_tab, toggle_placement].spacing(4), tab_bar]
                        .width(180)
                        .height(Length::Fill),
                    panes,
                ]
                .width(Length::Fill)
                .height(Length::Fill),
                status_bar,
            ]
            .into(),
        }
    }

    /// The one line at the bottom of every tab: the graph's size, the
    /// runtime and mux state, and either a hint the user just earned or the
    /// worst error the runtime reports. Workspace-wide, because "reconnecting"
    /// is as true in a terminal tab as in the graph.
    fn status_bar(&self) -> Element<'_, Message> {
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
            (format!("{head} | {hint}"), Color::from_rgb(0.9, 0.75, 0.35))
        } else if !error.is_empty() {
            (
                format!("{head} | ERROR: {error}"),
                Color::from_rgb(0.9, 0.3, 0.3),
            )
        } else {
            (head, Color::from_rgb(0.5, 0.5, 0.5))
        };

        container(text(status_text).size(12).color(error_color))
            .width(Length::Fill)
            .padding(4.0)
            .style(|_theme: &Theme| container::Style {
                background: Some(Color::from_rgb(0.1, 0.1, 0.12).into()),
                ..Default::default()
            })
            .into()
    }

    #[cfg(target_arch = "wasm32")]
    fn pane_title(&self, surface: Surface) -> String {
        surface_title(surface).to_owned()
    }

    /// A terminal pane in the browser editor: the topology is the same, the
    /// terminal is not there. No PTY, no native transport, and nothing the
    /// user can type into.
    #[cfg(target_arch = "wasm32")]
    fn terminal_pane(
        &self,
        _pane: Option<zeughaus_mux::PaneId>,
        _terminal: zeughaus_mux::TerminalId,
    ) -> Element<'_, Message> {
        unavailable(
            "Terminal",
            "Terminals run on the runner and are not shown in the browser editor.",
        )
    }

    fn graph_view(&self) -> Element<'_, Message> {
        // NodeGraph is generic over the id vocabulary declared by `GraphIds`;
        // theme and renderer stay at their defaults.
        let mut ng: NodeGraph<'_, GraphIds, Message> = NodeGraph::new();

        ng = ng
            .on_connect(|from, to| Message::EdgeConnected { from, to })
            .on_disconnect(|from, to| Message::EdgeDisconnected { from, to })
            .on_move(|delta, node_ids| Message::GroupMoved { node_ids, delta })
            .on_select(Message::SelectionChanged)
            .on_clone(Message::CloneNodes)
            .on_delete(Message::DeleteNodes)
            .on_camera(|position, zoom| Message::CameraChanged { position, zoom })
            .on_resize(|node_id, size| Message::NodeResized { node_id, size })
            .camera(self.camera_position, self.camera_zoom)
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
                // One graph at a time: a node of another one is not drawn here,
                // and its wires are mapped onto the container that holds it.
                if node.parent != self.current_graph {
                    continue;
                }
                let content = build_node_element(
                    node,
                    NodeChrome {
                        display: self.display_values.get(id),
                        settings: self.node_settings.get(id),
                        errors: self.setting_errors.get(id),
                        failure: self.node_error(*id),
                        dim_mask: self.dim_mask(*id, node),
                        size: self.node_sizes.get(id).copied(),
                        is_container: node.is_container,
                    },
                );
                // Per-node activity feedback: red marching-ants on error. There
                // is no "working" state to draw -- this process does not
                // execute, so a node is never mid-run here.
                let errored = self.node_error(*id).is_some();
                let node_widget = ng_node(node.id.0, node.position, content)
                    .resizable(is_display(&node.type_id))
                    .style(move |theme, status| {
                        let base = default_node_style(theme, status);
                        if errored {
                            return NodeStyle {
                                corner_radius: 8.0,
                                opacity: 0.88,
                                border_color: Color::from_rgb(0.9, 0.25, 0.25).into(),
                                border_pattern: Pattern::dashed(2.0, 6.0, 4.0).flow(25.0),
                                ..base
                            };
                        }
                        match status {
                            NodeStatus::Selected => NodeStyle {
                                corner_radius: 8.0,
                                opacity: 0.88,
                                border_color: Color::from_rgb(0.3, 0.6, 1.0).into(),
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
                        |theme, pin: &PinInfo<'_, GraphIds>, _other, status| PinStyle {
                            color: pin.info().color.into(),
                            shape: pin.info().shape,
                            ..default_pin_style(theme, status)
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
                self.view_endpoint(edge.from_node, &edge.from_pin, true),
                self.view_endpoint(edge.to_node, &edge.to_pin, false),
            ) else {
                continue;
            };

            // Edge color follows the source pin's type, dimmed while that output
            // carries nothing: an edge is only as live as the value on it.
            let edge_color = self
                .nodes
                .get(&from_node)
                .and_then(|n| n.pin_defs.iter().find(|p| &*p.name == from_pin.as_str()))
                .map(|p| pin_color(&p.ty))
                .unwrap_or(Color::from_rgb(0.6, 0.6, 0.6));
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
            .style(move |theme, status, _start, _end| {
                if src_error {
                    return EdgeStyle::error(theme, status);
                }
                let base = default_edge_style(theme, status);
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
                    particle(*born, PARTICLE_SPEED).style(move |theme| ParticleStyle {
                        color: edge_color,
                        ..default_particle_style(theme)
                    })
                }))
            };
            ng = ng.push_edge(edge_widget);
        }

        let graph_area: Element<'_, Message> = container(ng)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();

        let graph_view = if self.palette_open {
            let palette_view = palette::view(
                &self.palette_input,
                &self.palette_commands,
                self.palette_selected,
            );
            let overlay = container(palette_view)
                .width(Length::Fill)
                .padding(80.0)
                .align_x(iced::Alignment::Center);
            stack![graph_area, overlay].into()
        } else {
            graph_area
        };

        column![self.breadcrumb(), graph_view].into()
    }

    /// The path from the root graph to what is on screen, each step a way back.
    ///
    /// The only way out of a subgraph: the canvas shows one graph at a time, so
    /// without this a container entered by mistake would be a dead end.
    fn breadcrumb(&self) -> Element<'_, Message> {
        let trail: Vec<(NodeId, String)> = ancestry(self.current_graph, |id| {
            self.nodes.get(&id).map(|node| node.parent)
        })
        .into_iter()
        .filter_map(|id| {
            self.nodes
                .get(&id)
                .map(|node| (id, node.display_name.clone()))
        })
        .collect();

        let mut row = row![].spacing(6.0).align_y(iced::Alignment::Center);
        if self.current_graph == NodeId(0) {
            row = row.push(text("root").size(12));
        } else {
            row = row.push(
                button(text("root").size(12))
                    .padding(2.0)
                    .on_press(Message::EnterGraph(0)),
            );
        }
        for (index, (id, name)) in trail.iter().enumerate() {
            row = row.push(text("/").size(12));
            let last = index + 1 == trail.len();
            row = if last {
                row.push(text(name.clone()).size(12))
            } else {
                row.push(
                    button(text(name.clone()).size(12))
                        .padding(2.0)
                        .on_press(Message::EnterGraph(id.0)),
                )
            };
        }
        container(row)
            .width(Length::Fill)
            .padding(4.0)
            .style(|_theme: &Theme| container::Style {
                background: Some(Color::from_rgb(0.1, 0.1, 0.12).into()),
                ..Default::default()
            })
            .into()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let events = iced::event::listen_with(|event, _status, _id| {
            if let Event::Window(iced::window::Event::Resized(size)) = event {
                return Some(Message::WindowResized { size });
            }
            if let Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = event {
                if is_toggle_shortcut(&key, modifiers) {
                    return Some(Message::TogglePalette);
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
                        return Some(Message::PaletteCancel);
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

        Subscription::batch(subs)
    }

    pub fn theme(&self) -> Theme {
        Theme::Dark
    }
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

/// One of a pane's split buttons, pressable only while a runner is attached:
/// splitting creates a terminal, and there is no terminal without a runner.
fn split_button<'a>(
    label: &'a str,
    attached: bool,
    pane: pane_grid::Pane,
    axis: zeughaus_mux::Axis,
) -> iced::widget::Button<'a, Message, Theme> {
    let split = button(text(label).size(11)).padding([3, 6]);
    if attached {
        split.on_press(Message::Workspace(workspace::Message::Split { pane, axis }))
    } else {
        split
    }
}
