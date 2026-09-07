use std::collections::{HashMap, HashSet};
#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

use iced::keyboard;
use iced::widget::{button, column, container, image, pick_list, row, stack, text, text_input};
use iced::{Color, ContentFit, Element, Event, Length, Point, Subscription, Task, Theme, Vector};
use iced_nodegraph::{
    EdgeStyle, NodeGraph, NodeStatus, NodeStyle, Pattern, PinDirection as NgPinDirection, PinInfo,
    PinRef, PinShape, PinSide, PinStyle, default_edge_style, default_node_style, default_pin_style,
    edge as ng_edge, input_not_occupied, node as ng_node, node_header, node_pin,
};
// Particles are drawn from what the runtime delivered, and the wasm editor has
// no sync layer to hear it from.
#[cfg(not(target_arch = "wasm32"))]
use iced_nodegraph::{ParticleStyle, default_particle_style, particle};
use iced_palette::{get_filtered_command_index, is_toggle_shortcut};
#[cfg(not(target_arch = "wasm32"))]
use zeughaus_capture::CapturePlugin;
#[cfg(not(target_arch = "wasm32"))]
use zeughaus_core::occupancy_winner;
use zeughaus_core::{
    DomainPlugin, EdgeData, EdgeId, EdgeSemantic, GraphDocument, Image, NodeConfig, NodeData,
    NodeDefinition, NodeId, PinDefinition, PinDirection, PinKind, SettingDef, SettingKind, Ty,
    TypeConverters, Value, field_rows, renamed_field,
};
use zeughaus_flow::FlowPlugin;
use zeughaus_graph::GraphPlugin;
#[cfg(not(target_arch = "wasm32"))]
use zeughaus_llm::LlmPlugin;
use zeughaus_ml::MlPlugin;
use zeughaus_runtime::{Graph, GraphEdge, GraphExecutor, GraphNode};
use zeughaus_transform::TransformPlugin;

#[cfg(not(target_arch = "wasm32"))]
use crate::feed::{self, Endpoint, FeedKey, FeedSpec, FrameOrder};
use crate::message::{GraphIds, Message, PinLabel};
use crate::palette;

/// How fast a particle travels along its cable, in world units per second.
/// Fast enough to read as a message in flight, slow enough to be seen on a
/// short wire.
#[cfg(not(target_arch = "wasm32"))]
const PARTICLE_SPEED: f32 = 240.0;

/// Shortest gap between two particles on one edge: at most ten per second. A
/// 30 Hz source would otherwise smear into a solid line, which says less than
/// countable dots do.
#[cfg(not(target_arch = "wasm32"))]
const PARTICLE_MIN_GAP: std::time::Duration = std::time::Duration::from_millis(100);

/// Most particles kept per edge. A born time whose distance is past the cable
/// simply is not drawn, so the cap only bounds the memory a fast edge holds.
#[cfg(not(target_arch = "wasm32"))]
const PARTICLES_PER_EDGE: usize = 32;

pub struct EditorNode {
    pub id: NodeId,
    pub type_id: String,
    pub display_name: String,
    pub position: Point,
    pub pin_defs: Vec<PinDefinition>,
    pub settings: Vec<SettingDef>,
    /// The container node this node lives inside, `NodeId(0)` for the root
    /// graph. Only the editor knows about it: the executor stays flat.
    pub parent: NodeId,
    /// Whether this node type holds a subgraph, read off the catalog once when
    /// the node is inserted. A node type never changes, and `view` asked per
    /// drawn node per frame -- each answer a linear scan of the whole plugin
    /// catalog comparing strings.
    pub is_container: bool,
}

pub struct EditorEdge {
    pub id: EdgeId,
    pub from_node: NodeId,
    pub from_pin: PinLabel,
    pub to_node: NodeId,
    pub to_pin: PinLabel,
}

/// The edges on one node, by direction.
///
/// The index behind [`App::edge_index`]: without it every arriving value
/// scanned `App::edges` twice per node, which is O(nodes x edges) at the rate
/// a capture graph delivers.
#[derive(Default)]
struct NodeEdges {
    /// Edges leaving this node, each with the node at the other end -- so
    /// resolving one no longer means scanning every edge.
    outgoing: Vec<(EdgeId, NodeId)>,
    incoming: Vec<(EdgeId, NodeId)>,
    /// The nodes this one feeds through a *dataflow* edge.
    ///
    /// Relations are absent on purpose: two tables referencing each other is a
    /// legal schema, and this list is what answers "would this wire close a
    /// cycle".
    flow_out: Vec<NodeId>,
}

/// What a node shows inline: either the value rendered as text, or a decoded
/// image frame.
///
/// The image variant caches the `image::Handle` next to the pixels it was built
/// from. `Handle::from_rgba` mints a fresh id on every call, and a new id means
/// a new GPU upload -- for a 4K capture that is 33 MB per frame. Rebuilding the
/// handle only when the pixel buffer actually changed (pointer equality on the
/// shared frame) keeps a still frame at zero upload cost across redraws.
enum DisplayValue {
    Text(String),
    Frame {
        pixels: Arc<[u8]>,
        handle: image::Handle,
    },
}

/// One local edit on its way to the store.
///
/// The payload is owned and captured when the edit was made, not read again at
/// send time. That matters only for a replay after a reconnect: the store's
/// snapshot arrives on the same connection and may already have overwritten
/// this window's state with the older shared value, and re-reading state then
/// would send the store its own stale value back.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
enum Outbound {
    Node(NodeData),
    Params(NodeId, Vec<(String, String)>),
    Move(NodeId, f32, f32),
    Delete(NodeId),
    Connect(EdgeData),
    Disconnect(EdgeId),
}

/// Calls the reducer one queued edit means.
#[cfg(not(target_arch = "wasm32"))]
fn send_outbound(
    conn: &crate::module_bindings::DbConnection,
    edit: &Outbound,
) -> Result<(), String> {
    match edit {
        Outbound::Node(nd) => crate::sync::send_create_node(conn, nd),
        Outbound::Params(id, params) => crate::sync::send_set_params(conn, id.0, params),
        Outbound::Move(id, x, y) => crate::sync::send_move_node(conn, id.0, *x, *y),
        Outbound::Delete(id) => crate::sync::send_delete_node(conn, id.0),
        Outbound::Connect(e) => crate::sync::send_connect_edge(conn, e),
        Outbound::Disconnect(id) => crate::sync::send_disconnect_edge(conn, id.0),
    }
}

/// An editor edge as the store's row.
#[cfg(not(target_arch = "wasm32"))]
fn edge_data(e: &EditorEdge) -> EdgeData {
    EdgeData {
        id: e.id.0,
        from_node: e.from_node.0,
        from_pin: e.from_pin.to_string(),
        to_node: e.to_node.0,
        to_pin: e.to_pin.to_string(),
    }
}

/// One live feed: the task reading it, what it was asked for, and the newest
/// frame it produced.
///
/// The handle aborts on drop, which makes the map entry the feed's whole
/// lifetime: removing it here stops the task, and a feed can therefore not
/// outlive the node that wanted it. One that did would keep receiving 33 MB
/// frames for a node nobody can see.
#[cfg(not(target_arch = "wasm32"))]
struct LiveFeed {
    /// The tier this feed was opened with. A resize that snaps to the same
    /// ladder tier leaves it unchanged, which is what keeps a drag from
    /// restarting a video stream.
    tier: u32,
    /// Which generation of this feed the task belongs to.
    epoch: u64,
    /// Newest frame accepted, with the order it arrived in.
    latest: Option<(FrameOrder, Image)>,
    _task: iced::task::Handle,
}

pub struct App {
    // Editor state
    nodes: HashMap<NodeId, EditorNode>,
    node_order: Vec<NodeId>,
    edges: Vec<EditorEdge>,
    /// `edges` grouped by the nodes they touch, rebuilt whenever the edge set
    /// changes. What a node shows is read from the values on its own edges, and
    /// that question used to be answered by scanning every edge twice, for
    /// every node, on every arriving value.
    edge_index: HashMap<NodeId, NodeEdges>,
    selected: HashSet<NodeId>,
    camera_position: Point,
    camera_zoom: f32,
    /// Which container's contents are on screen; `NodeId(0)` is the root
    /// graph. Editor-local: what one window looks at is not shared state.
    current_graph: NodeId,
    /// Camera per graph, so stepping out of a subgraph returns to the view it
    /// was entered from instead of resetting.
    cameras: HashMap<NodeId, (Point, f32)>,

    // The editor's model of the graph, NOT a runtime. It owns the topology, the
    // pin definitions a variadic node grows (`sync_node_pins`) and the pin types
    // connection validation compares, so deleting it would take that metadata
    // with it. Executing the graph is the runner process's job alone.
    executor: GraphExecutor,
    plugins: Vec<Box<dyn DomainPlugin>>,
    catalog: Vec<NodeDefinition>,
    /// The palette's command list, built once from the catalog.
    ///
    /// The catalog is fixed at startup -- plugins are registered in `new` and
    /// never again -- so this used to be one `String` per catalog entry
    /// rebuilt on every redraw while the palette was open, i.e. per keystroke
    /// and per sync poll. Search filtering stays dynamic; it reads this list.
    palette_commands: Vec<iced_palette::Command<Message>>,
    /// Type converters built from the plugins. Shared with the executor; used
    /// here for connection validation so the editor and runtime agree on which
    /// type pairs may connect.
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

    // Const node text inputs (node_id -> current text)
    const_inputs: HashMap<NodeId, String>,

    // In-node text settings (node_id -> setting name -> current value)
    node_settings: HashMap<NodeId, HashMap<String, String>>,

    // Spawn offset counter (staggers new nodes so they don't overlap)
    spawn_counter: u32,

    // Status bar
    last_error: String,

    // Command palette state
    palette_open: bool,
    palette_input: String,
    palette_selected: usize,

    // The store connection, which rebuilds itself when the host goes away.
    // Held rather than a bare `DbConnection` because there is no such thing as
    // a connection a process can be handed once: a host restart or a dropped
    // packet ends it, and the editor used to keep editing against the corpse.
    #[cfg(not(target_arch = "wasm32"))]
    stdb: Option<crate::sync::Store>,
    // Edits that have not reached the store, in the order they were made.
    // Replayed when the connection comes back. Each entry carries the payload
    // as it was at the time of the edit, not a reference to state that the
    // reconnect's snapshot may have overwritten in the meantime.
    #[cfg(not(target_arch = "wasm32"))]
    outbox: Vec<Outbound>,
    // Reducer failures already logged, so a store that refuses every call does
    // not fill the terminal with one line per edit.
    #[cfg(not(target_arch = "wasm32"))]
    logged_sends: HashSet<String>,
    // Receiver for remote changes, drained on the SyncPoll timer.
    #[cfg(not(target_arch = "wasm32"))]
    sync_rx: Option<std::sync::mpsc::Receiver<crate::sync::SyncEvent>>,
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
    // Where the executing runtime last said it is reachable. Kept so a runner
    // that restarted on another port, or vanished, is detectable: every live
    // feed dialled the old address and has to be redialled.
    #[cfg(not(target_arch = "wasm32"))]
    endpoint: Option<Endpoint>,
    // Live video feeds, keyed by the source pin they carry rather than by the
    // Display node drawing it: two nodes watching one pin need the same frame,
    // so they share one feed.
    #[cfg(not(target_arch = "wasm32"))]
    feeds: HashMap<FeedKey, LiveFeed>,
    // Hands out feed generations. A replaced feed numbers its frames from
    // scratch, so the epoch -- not the sequence -- is what keeps a frame still
    // in flight from the old one out of the new one.
    #[cfg(not(target_arch = "wasm32"))]
    feed_epoch: u64,
    // Frames accepted since startup, for the status bar. A feed count alone
    // cannot be told from a stall; a number that climbs can.
    #[cfg(not(target_arch = "wasm32"))]
    frames_received: u64,
    // The task subscribed to the runtime's events. Aborts on drop, so replacing
    // it is how the editor stops listening to a runtime that moved.
    #[cfg(not(target_arch = "wasm32"))]
    traffic: Option<iced::task::Handle>,
    // Whether the event subscription is live. Values on screen are last-known
    // while it is not, which the status bar says rather than leaving the user
    // to guess.
    #[cfg(not(target_arch = "wasm32"))]
    traffic_live: bool,
    // Which subscription the traffic on screen came from. Bumped whenever the
    // task is replaced, so a message queued by the old one is recognizable.
    #[cfg(not(target_arch = "wasm32"))]
    traffic_epoch: u64,
    // The runtime's values as last reported, per node and pin. Kept beside the
    // executor because a value can arrive before the node row it belongs to.
    #[cfg(not(target_arch = "wasm32"))]
    remote_outputs: HashMap<NodeId, HashMap<String, Value>>,
    // The newest sequence applied per (node, pin). See `accept_output_seq`.
    #[cfg(not(target_arch = "wasm32"))]
    output_seq: HashMap<(NodeId, String), u64>,
    // Why each node is failing, as last reported by the runtime. The only path
    // a failure has to this window: the process that ran the node is not the
    // one drawing it, so before this the report existed nowhere but the
    // runtime's own log.
    #[cfg(not(target_arch = "wasm32"))]
    remote_errors: HashMap<NodeId, String>,
    // The newest sequence applied per node. Same guard as the outputs: the
    // error topic is its own stream, so a stale report can arrive last.
    #[cfg(not(target_arch = "wasm32"))]
    error_seq: HashMap<NodeId, u64>,
    // When each in-flight particle was born, per edge. One per delivered value,
    // which is why an unchanged value still animates: traffic, not state.
    #[cfg(not(target_arch = "wasm32"))]
    particles: HashMap<EdgeId, std::collections::VecDeque<iced::time::Instant>>,
    /// What a node said about a setting it refused, per setting key. Drawn
    /// under the field, so a rejected value does not sit there looking
    /// accepted.
    setting_errors: HashMap<NodeId, HashMap<String, String>>,
    /// Settings edits the store has not seen yet. See [`crate::pending`].
    #[cfg(not(target_arch = "wasm32"))]
    pending: crate::pending::PendingEdits,
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

        // Build the type-converter registry from builtins plus each plugin's
        // own converters, then share it with the executor.
        let converters = Arc::new({
            let mut c = TypeConverters::with_builtins();
            for p in &plugins {
                p.register_converters(&mut c);
            }
            c
        });
        let mut executor = GraphExecutor::new(Graph::new());
        executor.set_converters(converters.clone());

        // `session` is the join token, or None to host the default local
        // session. The session token is what the palette "Copy Session ID"
        // shares so a buddy can join.
        //
        // The first connection still has to succeed: a token naming a store
        // that is not there is a startup mistake, and retrying it forever
        // would only hide it. What is no longer fatal is LOSING it -- see
        // [`crate::sync::Store`]. A start with no store at all therefore falls
        // through to the local autosave, which is what makes an editor usable
        // before `spacetime start`.
        #[cfg(not(target_arch = "wasm32"))]
        let (stdb, sync_rx, session_id) = {
            let (uri, db, token) = match session {
                Some(token) => {
                    let (uri, db) = crate::sync::parse_token(&token);
                    (uri, db, token)
                }
                None => {
                    // Host: local server, default session, LAN-reachable token.
                    let token = format!(
                        "{}:{}/{}",
                        crate::sync::lan_ip(),
                        crate::sync::DEFAULT_PORT,
                        crate::sync::DEFAULT_SESSION
                    );
                    (
                        format!("http://127.0.0.1:{}", crate::sync::DEFAULT_PORT),
                        crate::sync::DEFAULT_SESSION.to_string(),
                        token,
                    )
                }
            };
            // Role::Viewer: this process edits and displays, it never executes,
            // so it must not register in the runtime table and be elected owner.
            match crate::sync::Store::open(&uri, &db, crate::sync::Role::Viewer) {
                Ok((store, rx)) => {
                    eprintln!("[stdb] session token: {token}");
                    (Some(store), Some(rx), Some(token))
                }
                Err(e) => {
                    eprintln!(
                        "[stdb] cannot reach {uri} / {db}: {e} -- editing locally \
                         (start it with `spacetime start` and restart to collaborate)"
                    );
                    (None, None, None)
                }
            }
        };

        let mut app = Self {
            nodes: HashMap::new(),
            node_order: Vec::new(),
            edges: Vec::new(),
            edge_index: HashMap::new(),
            selected: HashSet::new(),
            camera_position: Point::ORIGIN,
            camera_zoom: 1.0,
            current_graph: NodeId(0),
            cameras: HashMap::new(),
            executor,
            plugins,
            palette_commands: palette::build_commands(&catalog),
            catalog,
            converters,
            display_values: HashMap::new(),
            node_sizes: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            pending_edges: Vec::new(),
            const_inputs: HashMap::new(),
            node_settings: HashMap::new(),
            spawn_counter: 0,
            last_error: String::new(),
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
            // No feed until a runtime announces one and a Display node is wired
            // to a frame; both are discovered from the store, never assumed.
            #[cfg(not(target_arch = "wasm32"))]
            endpoint: None,
            #[cfg(not(target_arch = "wasm32"))]
            feeds: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            feed_epoch: 0,
            #[cfg(not(target_arch = "wasm32"))]
            frames_received: 0,
            #[cfg(not(target_arch = "wasm32"))]
            traffic: None,
            #[cfg(not(target_arch = "wasm32"))]
            traffic_live: false,
            #[cfg(not(target_arch = "wasm32"))]
            traffic_epoch: 0,
            #[cfg(not(target_arch = "wasm32"))]
            remote_outputs: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            output_seq: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            remote_errors: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            error_seq: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            particles: HashMap::new(),
            setting_errors: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            pending: crate::pending::PendingEdits::new(),
        };

        // Restore the last local session. A no-op while syncing: the shared
        // graph arrives from the subscription instead.
        app.load_autosave();

        app
    }

    /// The nodes directly inside `parent`, in creation order.
    ///
    /// Order matters: it decides which boundary wins a duplicate pin name, and
    /// `node_order` is the only stable order the editor has.
    fn children(&self, parent: NodeId) -> impl Iterator<Item = &EditorNode> {
        self.node_order
            .iter()
            .filter_map(move |id| self.nodes.get(id))
            .filter(move |node| node.parent == parent)
    }

    /// Every node inside `parent`, at any depth.
    ///
    /// Deleting a container deletes its contents, so the local delete needs the
    /// whole subtree -- otherwise this window would keep nodes the store has
    /// already dropped.
    ///
    /// The walk is bounded even though a tree cannot loop: `parent` is an
    /// arbitrary column of the store's `node` table, so one hand-written row
    /// naming itself (or a pair naming each other) is a cycle this editor did
    /// not create and must survive.
    fn descendants(&self, parent: NodeId) -> Vec<NodeId> {
        let mut found = Vec::new();
        let mut seen: HashSet<NodeId> = HashSet::from([parent]);
        let mut stack = vec![parent];
        while let Some(current) = stack.pop() {
            for child in self.children(current) {
                if !seen.insert(child.id) {
                    continue;
                }
                found.push(child.id);
                stack.push(child.id);
            }
        }
        found
    }

    /// Whether this node type holds a subgraph, per the catalog.
    fn is_container(&self, type_id: &str) -> bool {
        self.catalog
            .iter()
            .find(|d| &*d.type_id == type_id)
            .is_some_and(|d| d.container)
    }

    /// The pin name a boundary node contributes to its container.
    ///
    /// The node's `name` setting, or the type's default when it is empty: a
    /// container's pin has to be called something even while the user is
    /// clearing the field.
    fn boundary_name(&self, child: NodeId) -> String {
        let typed_default = || {
            self.nodes
                .get(&child)
                .and_then(|node| node.settings.iter().find(|s| &*s.name == "name"))
                .map(|s| s.default.to_string())
                .unwrap_or_default()
        };
        self.node_settings
            .get(&child)
            .and_then(|s| s.get("name"))
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(typed_default)
    }

    /// Rebuilds a container's pins from the boundary nodes inside it.
    ///
    /// The container declares no pins of its own, so this is the only thing
    /// that gives it any: one input per `graph.input` child, one output per
    /// `graph.output` child, named by that child. A name already used on the
    /// same side is skipped -- two pins with one name would be one pin the user
    /// cannot tell apart, and the first child in creation order keeps it.
    fn refresh_container_pins(&mut self, container: NodeId) {
        let Some(node) = self.nodes.get(&container) else {
            return;
        };
        if !node.is_container {
            return;
        }
        let boundaries: Vec<(NodeId, bool)> = self
            .children(container)
            .filter_map(|child| match child.type_id.as_str() {
                "graph.input" => Some((child.id, true)),
                "graph.output" => Some((child.id, false)),
                _ => None,
            })
            .collect();
        let mut pins: Vec<PinDefinition> = Vec::with_capacity(boundaries.len());
        for (child, is_input) in boundaries {
            let name = self.boundary_name(child);
            let taken = pins.iter().any(|p| {
                &*p.name == name.as_str() && (p.direction == PinDirection::Input) == is_input
            });
            if name.is_empty() || taken {
                continue;
            }
            pins.push(if is_input {
                PinDefinition::input(name, Ty::Any, PinKind::Trigger)
            } else {
                PinDefinition::output(name, Ty::Any)
            });
        }
        if let Some(node) = self.nodes.get_mut(&container) {
            node.pin_defs = pins.clone();
        }
        if let Some(gn) = self.executor.graph.node_mut(container) {
            gn.pin_defs = pins;
        }
    }

    /// The real node an edge endpoint has to name in the store.
    ///
    /// A wire dropped on a container's pin belongs to the boundary node behind
    /// that pin: edges always connect real nodes, so the executor never has to
    /// know a container exists. `None` when the pin names no boundary, which is
    /// a wire that cannot be stored and is therefore dropped.
    fn resolve_boundary(
        &self,
        node: NodeId,
        pin: &PinLabel,
        is_source: bool,
    ) -> Option<(NodeId, PinLabel)> {
        if !self.nodes.get(&node)?.is_container {
            return Some((node, pin.clone()));
        }
        let wanted = if is_source {
            "graph.output"
        } else {
            "graph.input"
        };
        let child = self
            .children(node)
            .filter(|child| child.type_id == wanted)
            .find(|child| self.boundary_name(child.id) == pin.as_str())?;
        let inner_pin = if is_source { "out" } else { "in" };
        Some((child.id, PinLabel::from(inner_pin)))
    }

    /// Where an edge endpoint is drawn in the current graph.
    ///
    /// Identity for a node of this graph; a boundary node one level down is
    /// drawn on its container's pin instead, so a wire that crosses into a
    /// subgraph is one visible wire rather than a stub on each side. `None`
    /// means the endpoint is not visible here, and the edge is not drawn.
    fn view_endpoint(
        &self,
        node: NodeId,
        pin: &PinLabel,
        is_source: bool,
    ) -> Option<(NodeId, PinLabel)> {
        let editor_node = self.nodes.get(&node)?;
        if editor_node.parent == self.current_graph {
            return Some((node, pin.clone()));
        }
        let crosses = match editor_node.type_id.as_str() {
            "graph.output" => is_source && pin.as_str() == "out",
            "graph.input" => !is_source && pin.as_str() == "in",
            _ => false,
        };
        if !crosses {
            return None;
        }
        let container = editor_node.parent;
        if self.nodes.get(&container)?.parent != self.current_graph {
            return None;
        }
        Some((container, PinLabel::from(self.boundary_name(node).as_str())))
    }

    /// Rebuilds the pins of the container a boundary node belongs to. Called
    /// after anything that can change a boundary's name or existence.
    fn refresh_boundary_owner(&mut self, node: NodeId) {
        let Some(parent) = self.nodes.get(&node).map(|n| n.parent) else {
            return;
        };
        self.refresh_container_pins(parent);
    }

    fn spawn_node(&mut self, type_id: &str, position: Point) {
        // Stagger each new node so they don't pile up
        let offset = (self.spawn_counter % 10) as f32 * 30.0;
        self.spawn_counter += 1;
        let position = Point::new(position.x + offset, position.y + offset);
        self.spawn_node_into(type_id, position, self.current_graph);
    }

    /// Creates a node of `type_id` inside `parent`, seeded with the defaults
    /// its type declares. `None` when no plugin in this build provides the
    /// type.
    ///
    /// Separate from [`Self::spawn_node`] because a clone decides both the
    /// parent and the exact position: a copied subtree's children belong to
    /// the copied container, and staggering them would move them inside it.
    fn spawn_node_into(
        &mut self,
        type_id: &str,
        position: Point,
        parent: NodeId,
    ) -> Option<NodeId> {
        let exec = self.plugins.iter().find_map(|p| p.create_node(type_id))?;

        let pin_defs = exec.pin_definitions().to_vec();
        let setting_defs = exec.settings();
        let id = NodeId::next();
        let display_name = self
            .catalog
            .iter()
            .find(|d| &*d.type_id == type_id)
            .map(|d| d.display_name.to_string())
            .unwrap_or_else(|| type_id.to_string());

        self.executor.graph.add_node(GraphNode {
            id,
            type_id: type_id.to_string(),
            config: NodeConfig::default(),
            pin_defs: pin_defs.clone(),
            position: (position.x, position.y),
        });
        self.executor.register_node(id, exec);

        // Seed settings from their defaults and push them into the node so its
        // execution state matches what the widget shows. A node that refuses
        // its own default says so on the node rather than nowhere.
        if !setting_defs.is_empty() {
            let mut values = HashMap::new();
            for def in &setting_defs {
                let refusal = self
                    .executor
                    .set_parameter(id, &def.name, Value::new(def.default.to_string()))
                    .err();
                self.record_setting_error(id, &def.name, refusal);
                values.insert(def.name.to_string(), def.default.to_string());
            }
            self.node_settings.insert(id, values);
        }

        self.nodes.insert(
            id,
            EditorNode {
                id,
                type_id: type_id.to_string(),
                display_name,
                position,
                pin_defs,
                settings: setting_defs,
                parent,
                is_container: self.is_container(type_id),
            },
        );
        self.node_order.push(id);

        match type_id {
            "transform.const_f64" => {
                self.const_inputs.insert(id, "0".to_string());
            }
            "transform.const_bool" => {
                self.const_inputs.insert(id, "false".to_string());
            }
            "transform.const_string" => {
                self.const_inputs.insert(id, String::new());
            }
            _ => {}
        }

        // A boundary node spawned inside a container is a new pin on it.
        self.refresh_container_pins(parent);
        #[cfg(not(target_arch = "wasm32"))]
        self.derive_db_params(id);
        #[cfg(not(target_arch = "wasm32"))]
        self.push_node(id);
        self.autosave();
        Some(id)
    }

    /// Copies a node, what has been typed into it, and -- for a container --
    /// everything inside it. Returns the copy of `root`.
    ///
    /// Cloning used to be `spawn_node(type_id, position)`: same type, same
    /// place, default everything. A copy that comes up with default settings,
    /// and a copied container that comes up empty, look like the original and
    /// behave differently -- which is worse than either copying properly or
    /// refusing.
    ///
    /// Ids are remapped, so a wire between two copied nodes joins the copies.
    /// Relations are ordinary edges here and are copied like the rest. An edge
    /// with only one end inside the copied subtree is NOT copied: a second
    /// wire onto a single-slot input would be refused anyway, and for a
    /// relation, guessing which of the two schemas the user meant to reference
    /// is worse than leaving the wire to be drawn.
    fn clone_subtree(&mut self, root: NodeId, offset: Vector) -> Option<NodeId> {
        // `descendants` yields a parent before its own children, so the copy a
        // child is parented to always exists by the time it is created.
        let mut originals = vec![root];
        originals.extend(self.descendants(root));

        let mut copy_of: HashMap<NodeId, NodeId> = HashMap::new();
        for original in originals {
            let Some(node) = self.nodes.get(&original) else {
                continue;
            };
            let type_id = node.type_id.clone();
            let position = if original == root {
                Point::new(node.position.x + offset.x, node.position.y + offset.y)
            } else {
                node.position
            };
            // The root lands next to the original, in the graph on screen;
            // everything below it keeps its place inside the copied container.
            let into = if original == root {
                self.current_graph
            } else {
                match copy_of.get(&node.parent) {
                    Some(parent) => *parent,
                    None => continue,
                }
            };
            let Some(copy) = self.spawn_node_into(&type_id, position, into) else {
                continue;
            };
            copy_of.insert(original, copy);

            if let Some(value) = self.const_inputs.get(&original).cloned() {
                self.const_inputs.insert(copy, value);
            }
            if let Some(settings) = self.node_settings.get(&original).cloned() {
                for (key, value) in settings {
                    self.apply_setting_locally(copy, &key, value);
                }
                #[cfg(not(target_arch = "wasm32"))]
                self.push_params(copy);
            }
        }

        let internal: Vec<(NodeId, PinLabel, NodeId, PinLabel)> = self
            .edges
            .iter()
            .filter_map(|e| {
                Some((
                    *copy_of.get(&e.from_node)?,
                    e.from_pin.clone(),
                    *copy_of.get(&e.to_node)?,
                    e.to_pin.clone(),
                ))
            })
            .collect();
        for (from_node, from_pin, to_node, to_pin) in internal {
            self.connect_edge(from_node, from_pin, to_node, to_pin);
        }

        let mut copies: Vec<NodeId> = copy_of.values().copied().collect();
        copies.sort_unstable();

        // A container's pins are synthesized from its children's `name`
        // settings, and `spawn_node_into` built them while every copied
        // boundary still held its default name. Two copied `graph.input`s
        // therefore collapsed into one pin, and worse: the container's pins
        // carried the default names while `boundary_name` reported the copied
        // ones, so `resolve_boundary` matched nothing and a wire dropped on
        // the clone's pin was silently discarded.
        for copy in &copies {
            if self.nodes.get(copy).is_some_and(|n| n.is_container) {
                self.refresh_container_pins(*copy);
            }
        }

        // The derived parameters say where the copy sits and what is wired to
        // it; they are not the original's to inherit. `spawn_node_into`
        // derived them before the settings arrived, and the settings copy then
        // wrote the original's over them -- including a `relations` line for a
        // wire that was NOT copied, which had the runner create a foreign key
        // the graph does not show. Re-derived last, when the copied edges
        // exist and the answer is knowable.
        #[cfg(not(target_arch = "wasm32"))]
        for copy in &copies {
            self.derive_db_params(*copy);
            self.derive_db_dependents(*copy);
        }

        copy_of.get(&root).copied()
    }

    fn connect_edge(
        &mut self,
        from_node: NodeId,
        from_pin: PinLabel,
        to_node: NodeId,
        to_pin: PinLabel,
    ) {
        // Ignore exact duplicates (snap can re-fire on_connect for the same pair).
        if self.edges.iter().any(|e| {
            e.from_node == from_node
                && e.from_pin == from_pin
                && e.to_node == to_node
                && e.to_pin == to_pin
        }) {
            return;
        }

        // An input pin holds at most one edge. can_connect already rejects a
        // drop onto an occupied input, so there is no existing wire to remove
        // here - the new connection only reaches this point when the input is
        // free (or the same edge is being re-routed onto itself).

        let edge_id = EdgeId::next();
        self.executor.graph.add_edge(GraphEdge {
            id: edge_id,
            from_node,
            from_pin: from_pin.0.clone(),
            to_node,
            to_pin: to_pin.0.clone(),
            semantic: EdgeSemantic::default(),
        });
        self.edges.push(EditorEdge {
            id: edge_id,
            from_node,
            from_pin,
            to_node,
            to_pin,
        });
        self.reindex_edges();
        // Seed the new wire from the source's last published output so it shows
        // a value immediately instead of staying blank until the runtime's next
        // publish. Nothing is executed here: this process does not run nodes.
        self.executor.on_edge_added(edge_id);
        // Grow a variadic target (e.g. merge node) so the next empty input shows.
        self.resync_pins(to_node);
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(e) = self.edges.last().map(edge_data) {
            self.push_edge(e);
        }
        // A table wired into a `table` pin is the column list the target works
        // from, and a wire between two field pins is a foreign key on the
        // referencing side -- which may be either end.
        #[cfg(not(target_arch = "wasm32"))]
        self.derive_db_params(to_node);
        #[cfg(not(target_arch = "wasm32"))]
        self.derive_db_params(from_node);
        self.update_display_values();
        self.autosave();
    }

    /// After a node's connections change, recompute its pins if it is variadic
    /// (e.g. a merge node grows an input as the last one fills) and sync the
    /// editor's pin snapshot so the new/removed pin is drawn immediately.
    fn resync_pins(&mut self, node: NodeId) {
        if let Some(pins) = self.executor.sync_node_pins(node)
            && let Some(en) = self.nodes.get_mut(&node)
        {
            en.pin_defs = pins;
        }
    }

    /// Re-reads a node's own pin declaration after its parameters changed, and
    /// mirrors it into the editor's node.
    ///
    /// Distinct from [`App::resync_pins`], which grows a variadic node from
    /// what is wired to it: this one picks up a pin set the node derived from a
    /// setting, where nothing is connected yet.
    fn refresh_node_pins(&mut self, node: NodeId) {
        if let Some(pins) = self.executor.refresh_pins(node)
            && let Some(en) = self.nodes.get_mut(&node)
        {
            en.pin_defs = pins;
        }
    }

    fn disconnect_edge(
        &mut self,
        from_node: NodeId,
        from_pin: PinLabel,
        to_node: NodeId,
        to_pin: PinLabel,
    ) {
        if let Some(pos) = self.edges.iter().position(|e| {
            e.from_node == from_node
                && e.from_pin == from_pin
                && e.to_node == to_node
                && e.to_pin == to_pin
        }) {
            let edge = self.edges.remove(pos);
            self.reindex_edges();
            self.executor.disconnect_edge(edge.id);
            self.resync_pins(to_node);
            // The wire the particles were riding is gone; without this every
            // local rewiring leaks a queue nothing will ever draw again.
            #[cfg(not(target_arch = "wasm32"))]
            self.particles.remove(&edge.id);
            #[cfg(not(target_arch = "wasm32"))]
            self.push_edge_remove(edge.id);
            // A table pin that lost its wire is a column list that no longer
            // applies, and a field that lost one is a foreign key that is
            // gone -- from whichever end declared it.
            #[cfg(not(target_arch = "wasm32"))]
            self.derive_db_params(to_node);
            #[cfg(not(target_arch = "wasm32"))]
            self.derive_db_params(from_node);
            self.update_display_values();
            self.autosave();
        }
    }

    /// A node's setting value, or the default its type declares.
    fn setting_or_default(&self, node: NodeId, key: &str) -> String {
        let default = || {
            self.nodes
                .get(&node)
                .and_then(|n| n.settings.iter().find(|s| &*s.name == key))
                .map(|s| s.default.to_string())
                .unwrap_or_default()
        };
        self.node_settings
            .get(&node)
            .and_then(|s| s.get(key))
            .cloned()
            .unwrap_or_else(default)
    }

    /// Whether a node's setting is a field list, i.e. whether its rows name
    /// pins. Only then can an edit of it rename one.
    fn is_field_setting(&self, node: NodeId, key: &str) -> bool {
        self.nodes.get(&node).is_some_and(|n| {
            n.settings
                .iter()
                .any(|s| &*s.name == key && matches!(s.kind, SettingKind::Fields { .. }))
        })
    }

    /// Applies a setting to this window only: the recorded text, the local
    /// executor's copy of the node, and the pin set the node now declares.
    ///
    /// Immediate on purpose. What the user typed has to be on screen at the
    /// next frame; what the *store* learns is a separate question, answered by
    /// [`Self::commit_node`] once the typing stops.
    ///
    /// The node's refusal is kept and drawn under the field
    /// ([`Self::setting_error`]). This window applies every setting locally
    /// before pushing it, so the node has already said what is wrong with the
    /// text -- discarding that was why a rejected value sat in the field
    /// looking accepted while the node kept the old one.
    fn apply_setting_locally(&mut self, node: NodeId, key: &str, value: String) {
        self.node_settings
            .entry(node)
            .or_default()
            .insert(key.to_string(), value.clone());
        let refusal = self
            .executor
            .set_parameter(node, key, Value::new(value))
            .err();
        self.record_setting_error(node, key, refusal);
        // A setting can decide the node's pins (a table's columns are one), so
        // the widget re-reads what it now declares.
        self.refresh_node_pins(node);
    }

    /// Remembers, or clears, what a node said about one of its settings.
    fn record_setting_error(
        &mut self,
        node: NodeId,
        key: &str,
        refusal: Option<zeughaus_core::ZeughausError>,
    ) {
        match refusal {
            Some(error) => {
                self.setting_errors
                    .entry(node)
                    .or_default()
                    .insert(key.to_string(), error.to_string());
            }
            None => {
                if let Some(errors) = self.setting_errors.get_mut(&node) {
                    errors.remove(key);
                    if errors.is_empty() {
                        self.setting_errors.remove(&node);
                    }
                }
            }
        }
    }

    /// Settles the wires a settings change moved or orphaned.
    ///
    /// `was` is the value the store holds, so one name changed in place is one
    /// rename however many keystrokes produced it.
    fn settle_relations(&mut self, node: NodeId, key: &str, was: &str) {
        let renamed = self
            .is_field_setting(node, key)
            .then(|| renamed_field(was, &self.setting_or_default(node, key)))
            .flatten();
        match renamed {
            Some((old, new)) => self.rename_pin_edges(node, &old, &new),
            // A field that is gone takes its relations with it: a foreign key
            // lives on a field, and a wire to a pin the node no longer
            // declares is one nobody can see or delete.
            None => self.drop_orphaned_relations(node),
        }
    }

    /// Commits one node's held-back settings to the store: the wires they
    /// moved, the parameters they are derived into, and the row itself.
    #[cfg(not(target_arch = "wasm32"))]
    fn commit_node(&mut self, node: NodeId, owed: crate::pending::Owed) {
        // Sorted so a run of edits produces the same sequence of store calls
        // in every window that replays it.
        let mut keys: Vec<(String, String)> = owed.was.into_iter().collect();
        keys.sort();
        for (key, was) in keys {
            self.settle_relations(node, &key, &was);
        }
        // A database's path reaches its children, a table's columns reach
        // everything it feeds, and its name is what the tables referencing it
        // name in their foreign keys.
        self.derive_db_params(node);
        self.derive_db_dependents(node);
        self.push_params(node);
    }

    /// Commits every node whose settings have been quiet long enough. Called
    /// from the sync poll, which is the editor's only clock while it is idle.
    #[cfg(not(target_arch = "wasm32"))]
    fn commit_settled(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let due = self
            .pending
            .settled(Instant::now(), crate::pending::DEBOUNCE);
        for (node, owed) in due {
            self.commit_node(node, owed);
        }
    }

    /// Commits everything now, because something is about to read or change
    /// the shared graph and must not see a store the local view has outgrown.
    #[cfg(not(target_arch = "wasm32"))]
    fn flush_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        for (node, owed) in self.pending.drain() {
            self.commit_node(node, owed);
        }
    }

    /// No store, nothing to hold back.
    #[cfg(target_arch = "wasm32")]
    fn flush_pending(&mut self) {}

    /// Drops an edge from this window: the view, the executor graph and cache,
    /// and the particle queue that was riding it.
    fn forget_edge(&mut self, id: EdgeId) {
        self.edges.retain(|e| e.id != id);
        self.reindex_edges();
        self.executor.disconnect_edge(id);
        #[cfg(not(target_arch = "wasm32"))]
        self.particles.remove(&id);
    }

    /// Moves every wire on `node`'s pin `old` over to pin `new`.
    ///
    /// A renamed field keeps its relations. The pin is the same field under
    /// another name, and dropping a foreign key because the user fixed a typo
    /// would be the harshest possible reading of an edit.
    ///
    /// A wire is replaced rather than renamed: the store addresses an edge by
    /// id and has no way to change the pin it names, so the row is deleted and
    /// a new one inserted under a fresh [`EdgeId`]. An edge id is not identity
    /// here -- nothing outside the graph refers to one -- so this needs no
    /// reducer of its own and leaves the module schema alone.
    fn rename_pin_edges(&mut self, node: NodeId, old: &str, new: &str) {
        let affected: Vec<(EdgeId, NodeId, PinLabel, NodeId, PinLabel)> = self
            .edges
            .iter()
            .filter(|e| {
                (e.from_node == node && e.from_pin.as_str() == old)
                    || (e.to_node == node && e.to_pin.as_str() == old)
            })
            .map(|e| {
                (
                    e.id,
                    e.from_node,
                    e.from_pin.clone(),
                    e.to_node,
                    e.to_pin.clone(),
                )
            })
            .collect();
        let renamed = PinLabel(Arc::from(new));
        for (id, from_node, from_pin, to_node, to_pin) in affected {
            self.forget_edge(id);
            #[cfg(not(target_arch = "wasm32"))]
            self.push_edge_remove(id);

            let from_pin = if from_node == node && from_pin.as_str() == old {
                renamed.clone()
            } else {
                from_pin
            };
            let to_pin = if to_node == node && to_pin.as_str() == old {
                renamed.clone()
            } else {
                to_pin
            };
            let fresh = EdgeId::next();
            self.executor.graph.add_edge(GraphEdge {
                id: fresh,
                from_node,
                from_pin: Arc::clone(&from_pin.0),
                to_node,
                to_pin: Arc::clone(&to_pin.0),
                semantic: EdgeSemantic::default(),
            });
            self.edges.push(EditorEdge {
                id: fresh,
                from_node,
                from_pin,
                to_node,
                to_pin,
            });
            self.executor.on_edge_added(fresh);
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(e) = self.edges.last().map(edge_data) {
                self.push_edge(e);
            }
        }
        // Once, after every wire has been replaced: nothing in the loop reads
        // the index, and rebuilding it per wire made a rename O(wires x edges).
        self.reindex_edges();
    }

    /// What a node declares for this pin, if it declares it at all.
    ///
    /// `None` and "declared, but not a field" are different answers, and the
    /// difference decides whether a wire is a lost relation or a live one.
    fn pin_def(&self, node: NodeId, pin: &str) -> Option<&PinDefinition> {
        self.nodes
            .get(&node)?
            .pin_defs
            .iter()
            .find(|p| &*p.name == pin)
    }

    /// Whether a node declares this pin as a bidirectional field pin.
    ///
    /// Read off the pin declaration, never off a node type: what makes an edge
    /// a relation is the same fact here, in the runtime and in the runner.
    fn is_field_pin(&self, node: NodeId, pin: &str) -> bool {
        self.pin_def(node, pin)
            .is_some_and(|p| p.direction == PinDirection::Both)
    }

    /// Whether an edge is a relation: both its ends are field pins.
    ///
    /// Only the derivation of the `relations` parameter asks, and that reaches
    /// the runner through the store -- which the browser editor has no path to.
    #[cfg(not(target_arch = "wasm32"))]
    fn is_relation(&self, edge: &EditorEdge) -> bool {
        self.is_field_pin(edge.from_node, edge.from_pin.as_str())
            && self.is_field_pin(edge.to_node, edge.to_pin.as_str())
    }

    /// Drops every relation of `node` whose field pin the node no longer
    /// declares at all.
    ///
    /// Called after a setting reshaped a node's pins. A relation is recognised
    /// by its *other* end being a field pin; it is lost when the pin this end
    /// lands on is no longer declared. Both halves matter: an edge whose own
    /// end is a pin that still exists is not this node's lost relation, even
    /// when that pin is an ordinary input or output rather than a field --
    /// reading "not a field pin" as "gone" deleted live wires from the store
    /// the first time any setting on the node was edited.
    ///
    /// A relation without its field is nothing: it would stay in the store,
    /// invisible in every view, and reattach itself if a field of that name
    /// ever came back. Renaming a field therefore drops its relations, which
    /// is the honest reading of "that field is gone".
    fn drop_orphaned_relations(&mut self, node: NodeId) {
        let orphaned: Vec<EdgeId> = self
            .edges
            .iter()
            .filter(|e| {
                let (own, other) = match (e.from_node == node, e.to_node == node) {
                    (true, _) => (e.from_pin.as_str(), (e.to_node, e.to_pin.as_str())),
                    (_, true) => (e.to_pin.as_str(), (e.from_node, e.from_pin.as_str())),
                    _ => return false,
                };
                is_lost_relation(self.pin_def(node, own), self.pin_def(other.0, other.1))
            })
            .map(|e| e.id)
            .collect();
        for edge_id in orphaned {
            self.forget_edge(edge_id);
            #[cfg(not(target_arch = "wasm32"))]
            self.push_edge_remove(edge_id);
        }
    }

    /// Applies what the runtime reported: values, their absence, and the edges
    /// a value crossed.
    ///
    /// This is the only way a value ever reaches the editor -- it computes
    /// nothing itself -- and the only place particles are born.
    #[cfg(not(target_arch = "wasm32"))]
    fn apply_traffic(&mut self, epoch: u64, traffic: crate::feed::Traffic) {
        use crate::feed::Traffic;
        use zeughaus_samples::RuntimeEvent;

        // A task that has been replaced may still have messages queued: its
        // snapshot would clear the values the current runtime just delivered.
        // The epoch is which subscription asked for them.
        if epoch != self.traffic_epoch {
            return;
        }
        match traffic {
            Traffic::Snapshot(snapshot) => {
                // The whole set replaces the whole set: a pin the snapshot does
                // not name produced nothing, which is what the editor dims.
                self.remote_outputs.clear();
                self.output_seq.clear();
                // The failing nodes replace the failing nodes, for the same
                // reason: a node the snapshot does not name is not failing.
                self.remote_errors = snapshot
                    .errors
                    .into_iter()
                    .map(|row| (NodeId(row.node_id), row.message))
                    .collect();
                self.error_seq = self
                    .remote_errors
                    .keys()
                    .map(|id| (*id, snapshot.seq))
                    .collect();
                for row in snapshot.outputs {
                    let Some(value) = zeughaus_core::decode_scalar(&row.ty, &row.value) else {
                        continue;
                    };
                    let node = NodeId(row.node_id);
                    self.output_seq
                        .insert((node, row.pin.clone()), snapshot.seq);
                    self.remote_outputs
                        .entry(node)
                        .or_default()
                        .insert(row.pin, value);
                }
                for id in self.nodes.keys().copied().collect::<Vec<_>>() {
                    let outputs = self.remote_outputs.get(&id).cloned().unwrap_or_default();
                    self.executor.set_remote_outputs(id, outputs);
                }
                self.traffic_live = true;
                self.update_display_values();
            }
            Traffic::Event(RuntimeEvent::Output {
                seq,
                node_id,
                pin,
                ty,
                value,
            }) => {
                let node = NodeId(node_id);
                if !self.accept_output_seq(node, &pin, seq) {
                    return;
                }
                let Some(value) = zeughaus_core::decode_scalar(&ty, &value) else {
                    return;
                };
                self.remote_outputs
                    .entry(node)
                    .or_default()
                    .insert(pin, value);
                self.reapply_remote_outputs(node);
            }
            Traffic::Event(RuntimeEvent::OutputCleared { seq, node_id, pin }) => {
                let node = NodeId(node_id);
                if !self.accept_output_seq(node, &pin, seq) {
                    return;
                }
                if let Some(pins) = self.remote_outputs.get_mut(&node) {
                    pins.remove(&pin);
                }
                self.reapply_remote_outputs(node);
            }
            Traffic::Event(RuntimeEvent::Edge { edge_id, .. }) => {
                let now = iced::time::Instant::now();
                let particles = self.particles.entry(EdgeId(edge_id)).or_default();
                // A 30 Hz source would otherwise smear into a solid line, which
                // says less than a countable dot does.
                let too_soon = particles
                    .back()
                    .is_some_and(|born| now.duration_since(*born) < PARTICLE_MIN_GAP);
                if !too_soon {
                    particles.push_back(now);
                }
                while particles.len() > PARTICLES_PER_EDGE {
                    particles.pop_front();
                }
            }
            Traffic::Event(RuntimeEvent::NodeError {
                seq,
                node_id,
                message,
            }) => {
                let node = NodeId(node_id);
                if self.accept_error_seq(node, seq) {
                    self.remote_errors.insert(node, message);
                }
            }
            Traffic::Event(RuntimeEvent::NodeErrorCleared { seq, node_id }) => {
                let node = NodeId(node_id);
                if self.accept_error_seq(node, seq) {
                    self.remote_errors.remove(&node);
                }
            }
            Traffic::Lost => {
                self.traffic_live = false;
                // Values stay on screen as last-known, because the status bar
                // says so and a number nobody claims is still the last one
                // anybody claimed. An error is not like that: it is a claim
                // about a run, made by a process that is no longer there to
                // make it, and a red border with nothing behind it is worse
                // than none.
                self.remote_errors.clear();
                self.error_seq.clear();
            }
        }
    }

    /// Whether this report is newer than what was already applied for that pin.
    ///
    /// Pub/Sub messages travel on separate QUIC streams and the snapshot is
    /// fetched concurrently, so an older message can arrive last. Storing the
    /// sequence per pin is what keeps it from overwriting a fresh value.
    #[cfg(not(target_arch = "wasm32"))]
    fn accept_output_seq(&mut self, node: NodeId, pin: &str, seq: u64) -> bool {
        let key = (node, pin.to_owned());
        if self.output_seq.get(&key).is_some_and(|seen| *seen >= seq) {
            return false;
        }
        self.output_seq.insert(key, seq);
        true
    }

    /// The same guard for the error topic, keyed by node: an error report is
    /// about a whole run, not about one pin.
    #[cfg(not(target_arch = "wasm32"))]
    fn accept_error_seq(&mut self, node: NodeId, seq: u64) -> bool {
        if self.error_seq.get(&node).is_some_and(|seen| *seen >= seq) {
            return false;
        }
        self.error_seq.insert(node, seq);
        true
    }

    /// Why a node is failing, whoever noticed.
    ///
    /// The runtime's report wins: the process that RAN the node is the only
    /// one that can know why it failed. This window never executes, so its own
    /// executor holds only what it refused itself before the value ever left --
    /// a rejected setting -- which is the fallback and all a browser editor
    /// ever has.
    fn node_error(&self, node: NodeId) -> Option<&str> {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(message) = self.remote_errors.get(&node) {
            return Some(message.as_str());
        }
        self.executor.node_error(node)
    }

    /// Every failing node with its message, in node order so the status bar
    /// does not name a different one on every redraw.
    fn failing_nodes(&self) -> Vec<(NodeId, &str)> {
        let mut failing: Vec<(NodeId, &str)> = self
            .node_order
            .iter()
            .filter_map(|id| self.node_error(*id).map(|message| (*id, message)))
            .collect();
        failing.sort_by_key(|(id, _)| *id);
        failing
    }

    /// Pushes one node's current remote outputs into the executor and redraws.
    ///
    /// A node whose row has not arrived yet is remembered anyway: the value
    /// applies as soon as `apply_node_upsert` creates it.
    #[cfg(not(target_arch = "wasm32"))]
    fn reapply_remote_outputs(&mut self, node: NodeId) {
        if !self.nodes.contains_key(&node) {
            return;
        }
        let outputs = self.remote_outputs.get(&node).cloned().unwrap_or_default();
        self.executor.set_remote_outputs(node, outputs);
        self.update_displays_from(node);
    }

    /// The live store connection, or `None` while there is none.
    ///
    /// `None` is a state, not a failure: the host may be restarting, and this
    /// window keeps working on a graph it can no longer share.
    #[cfg(not(target_arch = "wasm32"))]
    fn store_conn(&self) -> Option<&crate::module_bindings::DbConnection> {
        self.stdb.as_ref().and_then(crate::sync::Store::conn)
    }

    /// Whether this window's edits are reaching the shared graph.
    #[cfg(not(target_arch = "wasm32"))]
    fn store_live(&self) -> bool {
        self.stdb.as_ref().is_some_and(crate::sync::Store::is_live)
    }

    /// Brings the event subscription and the live feeds in line with what the
    /// graph and the store now say.
    ///
    /// The one place anything is dialled, called after everything that can
    /// change the answer: a Display node's wire, its size, its existence, a
    /// graph reload, or where the runtime serves. A feed that is still wanted
    /// with the same request is left running -- restarting a video stream
    /// because the editor redrew would be a stutter the user can see.
    ///
    /// Stopping is by removal: every task handle aborts on drop.
    #[cfg(not(target_arch = "wasm32"))]
    fn reconcile_runtime(&mut self) -> Task<Message> {
        let announced = self
            .store_conn()
            .and_then(crate::sync::owner_endpoint)
            .map(Endpoint);
        // A runtime that moved, restarted or regenerated its identity
        // invalidates every address already dialled, so nothing survives it.
        let moved = announced != self.endpoint;
        if moved {
            self.endpoint = announced;
        }
        let mut tasks = Vec::new();
        // Also restarted when the endpoint is unchanged but no task is running:
        // the subscription is what carries every value, so a missing one is not
        // something to wait out.
        if moved || (self.traffic.is_none() && self.endpoint.is_some()) {
            self.traffic = None;
            self.traffic_live = false;
            // Anything the aborted task still has queued belongs to the
            // subscription that is being replaced.
            self.traffic_epoch += 1;
            self.remote_outputs.clear();
            self.output_seq.clear();
            // A failure belongs to the runtime that reported it; a different
            // one has not run anything yet.
            self.remote_errors.clear();
            self.error_seq.clear();
            // A particle in flight belongs to the runtime that sent it; the new
            // one has not delivered anything yet.
            self.particles.clear();
            // Whatever is on screen came from a runtime that is no longer
            // there, and the next snapshot is what replaces it.
            for id in self.nodes.keys().copied().collect::<Vec<_>>() {
                self.executor.set_remote_outputs(id, HashMap::new());
            }
            self.update_display_values();
            if let Some(endpoint) = self.endpoint.clone() {
                let epoch = self.traffic_epoch;
                let (task, handle) = Task::run(feed::events(endpoint), move |traffic| {
                    Message::Traffic(epoch, traffic)
                })
                .abortable();
                self.traffic = Some(handle.abort_on_drop());
                tasks.push(task);
            }
        }
        // With nothing serving frames, a live feed would be reading a dead
        // stream and the frame it left behind is not what the graph shows.
        let wanted = match self.endpoint {
            None => HashMap::new(),
            Some(_) => self.wanted_feeds(),
        };
        // A feed's request is fixed for its lifetime, so a new tier is a new
        // feed rather than a renegotiation.
        let lost = self.retain_feeds(|key, live| !moved && wanted.get(key) == Some(&live.tier));
        if lost {
            self.update_display_values();
        }
        let Some(endpoint) = self.endpoint.clone() else {
            return Task::batch(tasks);
        };

        for (key, tier) in wanted {
            if self.feeds.contains_key(&key) {
                continue;
            }
            self.feed_epoch += 1;
            let epoch = self.feed_epoch;
            let spec = FeedSpec {
                endpoint: endpoint.clone(),
                key: key.clone(),
                epoch,
                tier,
            };
            let (task, handle) = Task::run(feed::frames(spec), Message::FeedFrame).abortable();
            self.feeds.insert(
                key,
                LiveFeed {
                    tier,
                    epoch,
                    latest: None,
                    _task: handle.abort_on_drop(),
                },
            );
            tasks.push(task);
        }
        Task::batch(tasks)
    }

    /// Which feeds the graph asks for, and at what ladder tier.
    ///
    /// Only image-typed source pins: a scalar already arrives as a runtime
    /// event, and streaming it as frames as well would be a second, slower
    /// path to the same number.
    #[cfg(not(target_arch = "wasm32"))]
    fn wanted_feeds(&self) -> HashMap<FeedKey, u32> {
        let frame_ty = Ty::of::<Image>();
        let mut wanted: HashMap<FeedKey, u32> = HashMap::new();
        for (id, node) in &self.nodes {
            if !is_display(&node.type_id) {
                continue;
            }
            // A Display node has exactly one input, so its single incoming edge
            // names the pin it draws.
            let Some(edge) = self.edges.iter().find(|e| e.to_node == *id) else {
                continue;
            };
            let carries_frame = self
                .nodes
                .get(&edge.from_node)
                .and_then(|src| src.pin_defs.iter().find(|p| p.name == edge.from_pin.0))
                .is_some_and(|p| p.ty == frame_ty);
            if !carries_frame {
                continue;
            }
            // Sized by what the node body draws, not by the frame's native size:
            // the runtime holds the frame, so it is the side that can cheaply
            // produce the half megabyte a preview needs instead of shipping
            // 33 MB for it.
            let tier =
                feed::requested_tier(self.node_sizes.get(id).copied().unwrap_or(DISPLAY_SIZE));
            let key = FeedKey {
                node_id: edge.from_node.0,
                pin: Arc::clone(&edge.from_pin.0),
            };
            wanted
                .entry(key)
                .and_modify(|shared| *shared = feed::widen(*shared, tier))
                .or_insert(tier);
        }
        wanted
    }

    /// Drops every feed `keep` rejects, returning whether any of them was
    /// showing a frame -- that frame just left the screen, so the caller has to
    /// rebuild the display values.
    #[cfg(not(target_arch = "wasm32"))]
    fn retain_feeds(&mut self, mut keep: impl FnMut(&FeedKey, &LiveFeed) -> bool) -> bool {
        let mut lost = false;
        self.feeds.retain(|key, live| {
            let stay = keep(key, live);
            lost |= !stay && live.latest.is_some();
            stay
        });
        lost
    }

    /// The frame a Display node currently shows, if a feed is delivering one.
    #[cfg(not(target_arch = "wasm32"))]
    fn feed_frame(&self, node_id: NodeId) -> Option<&Image> {
        if self.feeds.is_empty() {
            return None;
        }
        if !self
            .nodes
            .get(&node_id)
            .is_some_and(|n| is_display(&n.type_id))
        {
            return None;
        }
        let edge = self.edges.iter().find(|e| e.to_node == node_id)?;
        let key = FeedKey {
            node_id: edge.from_node.0,
            pin: Arc::clone(&edge.from_pin.0),
        };
        self.feeds
            .get(&key)?
            .latest
            .as_ref()
            .map(|(_, image)| image)
    }

    /// Adopts a frame from a feed, if it is still the frame to show.
    ///
    /// Two are refused: one from a superseded generation (a resize or a
    /// reconnect opened a new feed, whose numbering restarts, so the epoch is
    /// what separates them), and one whose sequence is not newer than what is
    /// already on screen.
    #[cfg(not(target_arch = "wasm32"))]
    fn apply_feed_frame(&mut self, frame: crate::feed::Frame) {
        let Some(live) = self.feeds.get_mut(&frame.key) else {
            return;
        };
        if live.epoch != frame.epoch {
            return;
        }
        let image = frame.image.clone();
        live.latest = Some((frame.order, frame.image));
        self.frames_received += 1;

        // Only the nodes drawing this feed change, so the whole display map is
        // not rebuilt: at 30 frames a second that would re-render every other
        // node's value text 30 times a second for nothing.
        let drawing: Vec<NodeId> = self
            .edges
            .iter()
            .filter(|e| e.from_node.0 == frame.key.node_id && e.from_pin.0 == frame.key.pin)
            .map(|e| e.to_node)
            .filter(|id| self.nodes.get(id).is_some_and(|n| is_display(&n.type_id)))
            .collect();
        for id in drawing {
            let display = self.frame_display(id, &image);
            self.display_values.insert(id, display);
        }
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
            text.push_str(if self.traffic_live {
                " | live"
            } else {
                " | reconnecting"
            });
        }
        if !self.feeds.is_empty() {
            let feeds = self.feeds.len();
            let plural = if feeds == 1 { "" } else { "s" };
            text.push_str(&format!(
                " | {feeds} feed{plural}, {} frames",
                self.frames_received
            ));
        }
        text
    }

    // The wasm editor has no sync layer, so it has no runtime to report on.
    #[cfg(target_arch = "wasm32")]
    fn runtime_text(&self) -> String {
        String::new()
    }

    /// The status bar's error text: one failing node's message, plus a count
    /// when several failed. Empty while every node is fine.
    fn node_error_summary(&self) -> String {
        let failing = self.failing_nodes();
        let Some((id, message)) = failing.first() else {
            return String::new();
        };
        let name = self
            .nodes
            .get(id)
            .map_or("node", |n| n.display_name.as_str());
        match failing.len() - 1 {
            0 => format!("{name}: {message}"),
            more => format!("{name}: {message} (+{more} more)"),
        }
    }

    // Local-file persistence is native-only. On wasm the graph lives in the
    // SpacetimeDB central store (wired in a later phase), so these are no-ops.
    #[cfg(not(target_arch = "wasm32"))]
    fn autosave(&self) {
        let doc = self.to_document();
        if let Ok(json) = serde_json::to_string_pretty(&doc) {
            let _ = std::fs::write(Self::autosave_path(), json);
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn autosave(&self) {}

    #[cfg(not(target_arch = "wasm32"))]
    fn autosave_path() -> PathBuf {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));
        exe_dir.join("zeughaus_autosave.zgh")
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn load_autosave(&mut self) {
        // When syncing, the SpacetimeDB store is the source of truth: start
        // empty and adopt the shared graph from the subscription instead of the
        // local autosave (which would conflict with remote ids).
        if self.stdb.is_some() {
            return;
        }
        let path = Self::autosave_path();
        if let Ok(json) = std::fs::read_to_string(&path)
            && let Ok(doc) = serde_json::from_str::<GraphDocument>(&json)
        {
            self.load_document(doc);
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn load_autosave(&mut self) {}

    /// Rebuilds [`Self::edge_index`] from the current edge set.
    ///
    /// Called from every place that adds or removes an edge. Rebuilding the
    /// whole thing is O(edges) and happens on a user action; the index exists
    /// to keep the O(nodes x edges) scan out of the path a value takes, which
    /// runs at frame rate.
    fn reindex_edges(&mut self) {
        let mut index: HashMap<NodeId, NodeEdges> = HashMap::new();
        for edge in &self.edges {
            let from = edge.from_node;
            let to = edge.to_node;
            index.entry(from).or_default().outgoing.push((edge.id, to));
            index.entry(to).or_default().incoming.push((edge.id, from));
            // The same rule the runtime applies (`Graph::is_dataflow`): a wire
            // with a field pin at either end declares a relationship, carries
            // nothing, and is no dependency.
            let relation = self.is_field_pin(from, edge.from_pin.as_str())
                || self.is_field_pin(to, edge.to_pin.as_str());
            if !relation {
                index.entry(from).or_default().flow_out.push(to);
            }
        }
        self.edge_index = index;
    }

    /// What one node shows inline: the value on one of its own edges, or the
    /// frame its feed delivered.
    ///
    /// A node shows what it produced; a sink (no outgoing edges) shows what it
    /// received, which is what makes the Display node work. Image frames reuse
    /// the previous `image::Handle` when the pixels are literally the same
    /// buffer, so an unchanged frame is not re-uploaded to the GPU.
    fn display_for(&self, node_id: NodeId) -> Option<DisplayValue> {
        // A frame from a feed outranks the edge cache, because for an image
        // pin the edge cache is empty by design: pixels never travel through
        // the store, so the feed is the only thing that has the frame.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(frame) = self.feed_frame(node_id) {
            return Some(self.frame_display(node_id, frame));
        }
        let edges = self.edge_index.get(&node_id)?;
        let value = edges
            .outgoing
            .iter()
            .find_map(|(id, _)| self.executor.edge_value(*id))
            .or_else(|| {
                edges
                    .incoming
                    .iter()
                    .find_map(|(id, _)| self.executor.edge_value(*id))
            })?;
        Some(match value.downcast_ref::<Image>() {
            Some(frame) => self.frame_display(node_id, frame),
            None => DisplayValue::Text(value.to_string()),
        })
    }

    /// Recomputes what every node shows.
    ///
    /// For the paths that change the whole picture at once: a graph loaded, a
    /// snapshot adopted, a runtime gone. A single delivered value uses
    /// [`Self::update_displays_from`] instead.
    fn update_display_values(&mut self) {
        let mut next: HashMap<NodeId, DisplayValue> = HashMap::new();
        for &node_id in self.nodes.keys() {
            if let Some(display) = self.display_for(node_id) {
                next.insert(node_id, display);
            }
        }
        self.display_values = next;
    }

    /// Recomputes only the displays a value delivered by `node` can change:
    /// the node itself, and whatever its outgoing edges reach.
    ///
    /// Nothing further downstream: a node deeper in the chain shows what it
    /// produced, and that arrives as its own delivery. This is called once per
    /// output event, which on a capture graph is 30 times a second. Only the
    /// sync layer delivers values, so only the native editor needs it.
    #[cfg(not(target_arch = "wasm32"))]
    fn update_displays_from(&mut self, node: NodeId) {
        let mut touched: Vec<NodeId> = vec![node];
        if let Some(edges) = self.edge_index.get(&node) {
            touched.extend(edges.outgoing.iter().map(|(_, target)| *target));
        }
        for id in touched {
            match self.display_for(id) {
                Some(display) => {
                    self.display_values.insert(id, display);
                }
                None => {
                    self.display_values.remove(&id);
                }
            }
        }
    }

    /// A frame's inline display, reusing the cached handle while the pixel
    /// buffer is unchanged (see [`DisplayValue`] for why the id matters).
    fn frame_display(&self, node_id: NodeId, frame: &Image) -> DisplayValue {
        let pixels = Arc::clone(frame.rgba());
        if let Some(DisplayValue::Frame {
            pixels: old,
            handle,
        }) = self.display_values.get(&node_id)
            && Arc::ptr_eq(old, &pixels)
        {
            return DisplayValue::Frame {
                pixels,
                handle: handle.clone(),
            };
        }
        let handle = image::Handle::from_rgba(frame.width(), frame.height(), pixels.to_vec());
        DisplayValue::Frame { pixels, handle }
    }

    /// One bit per pin (in `node.pin_defs` order), set when that pin is an
    /// output whose last run produced no value.
    ///
    /// Reads `output_value`, not the edge cache: the cache keeps whatever last
    /// crossed the wire, while `output_value` answers "did this run produce it",
    /// which is the distinction the user needs to see -- an `error` pin that
    /// stays dim means nothing failed.
    fn dim_mask(&self, id: NodeId, node: &EditorNode) -> u64 {
        node.pin_defs
            .iter()
            .take(64)
            .enumerate()
            .filter(|(_, p)| p.direction == PinDirection::Output)
            .filter(|(_, p)| self.executor.output_value(id, &p.name).is_none())
            .fold(0u64, |mask, (i, _)| mask | 1 << i)
    }

    // Used by native persistence (autosave / file dialogs); on wasm it will be
    // used by the SpacetimeDB sync layer in a later phase.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    fn to_document(&self) -> GraphDocument {
        let nodes = self
            .node_order
            .iter()
            .filter_map(|id| {
                let node = self.nodes.get(id)?;
                let mut params: Vec<(String, String)> = self
                    .const_inputs
                    .get(id)
                    .map(|v| vec![("value".to_string(), v.clone())])
                    .unwrap_or_default();
                if let Some(settings) = self.node_settings.get(id) {
                    params.extend(settings.iter().map(|(k, v)| (k.clone(), v.clone())));
                }
                Some(NodeData {
                    id: node.id.0,
                    type_id: node.type_id.clone(),
                    display_name: node.display_name.clone(),
                    x: node.position.x,
                    y: node.position.y,
                    params,
                    parent: node.parent.0,
                })
            })
            .collect();

        let edges = self
            .edges
            .iter()
            .map(|e| EdgeData {
                id: e.id.0,
                from_node: e.from_node.0,
                from_pin: e.from_pin.to_string(),
                to_node: e.to_node.0,
                to_pin: e.to_pin.to_string(),
            })
            .collect();

        GraphDocument { nodes, edges }
    }

    /// Instantiates one node from its serialized data (explicit id, position,
    /// params), registering it in the executor and editor state. Shared by
    /// document loading and remote sync apply. No-op on a duplicate id or an
    /// unknown node type.
    fn insert_node_from_data(&mut self, node_data: &NodeData) {
        let type_id = &node_data.type_id;
        let Some(mut exec) = self.plugins.iter().find_map(|p| p.create_node(type_id)) else {
            return;
        };
        let id = NodeId(node_data.id);
        if self.nodes.contains_key(&id) {
            return;
        }
        let pin_defs = exec.pin_definitions().to_vec();
        let setting_defs = exec.settings();
        let position = Point::new(node_data.x, node_data.y);

        let is_const = type_id.starts_with("transform.const_");
        let setting_names: HashSet<&str> = setting_defs.iter().map(|d| &*d.name).collect();

        // Apply saved parameters. Const nodes parse their typed value;
        // setting-bearing nodes restore each named string setting.
        for (name, value_str) in &node_data.params {
            match type_id.as_str() {
                "transform.const_f64" => {
                    if let Ok(f) = value_str.parse::<f64>() {
                        let _ = exec.set_parameter(name, Value::new(f));
                    }
                }
                "transform.const_bool" => {
                    let b = value_str == "true" || value_str == "1";
                    let _ = exec.set_parameter(name, Value::new(b));
                }
                "transform.const_string" => {
                    let _ = exec.set_parameter(name, Value::new(value_str.clone()));
                }
                _ => {}
            }
            if is_const {
                self.const_inputs.insert(id, value_str.clone());
            } else if setting_names.contains(name.as_str()) {
                let _ = exec.set_parameter(name, Value::new(value_str.clone()));
                self.node_settings
                    .entry(id)
                    .or_default()
                    .insert(name.clone(), value_str.clone());
            }
        }

        self.executor.graph.add_node(GraphNode {
            id,
            type_id: type_id.clone(),
            config: NodeConfig::default(),
            pin_defs: pin_defs.clone(),
            position: (position.x, position.y),
        });
        self.executor.register_node(id, exec);

        self.nodes.insert(
            id,
            EditorNode {
                id,
                type_id: type_id.clone(),
                display_name: node_data.display_name.clone(),
                position,
                pin_defs,
                settings: setting_defs,
                parent: NodeId(node_data.parent),
                is_container: self.is_container(&node_data.type_id),
            },
        );
        self.node_order.push(id);
    }

    fn load_document(&mut self, doc: GraphDocument) {
        // Clear current state
        self.nodes.clear();
        self.node_order.clear();
        self.edges.clear();
        self.edge_index.clear();
        self.const_inputs.clear();
        self.node_settings.clear();
        self.display_values.clear();
        // Every feed was opened for the outgoing document's node ids; nothing
        // here is known to still be wanted, so they all stop. The reconcile that
        // follows the load reopens whatever the new document asks for.
        #[cfg(not(target_arch = "wasm32"))]
        self.feeds.clear();
        // A fresh executor loses the converter registry, which `sync_node_pins`
        // and the pin-type bookkeeping still need; hand it back immediately.
        self.executor = GraphExecutor::new(Graph::new());
        self.executor.set_converters(self.converters.clone());

        // Advance the id counters past every restored id so newly spawned
        // nodes/edges cannot collide with loaded ones. A collision would push
        // the same id twice and the graph would render/drag it doubled.
        if let Some(max_node) = doc.nodes.iter().map(|n| n.id).max() {
            NodeId::bump_above(max_node);
        }
        if let Some(max_edge) = doc.edges.iter().map(|e| e.id).max() {
            EdgeId::bump_above(max_edge);
        }

        // Rebuild from document
        for node_data in &doc.nodes {
            self.insert_node_from_data(node_data);
        }
        // Only now: a container's pins come from its children, and a saved
        // document lists them in whatever order it pleases. Refreshing during
        // the loop would give a container that precedes its boundary nodes no
        // pins at all -- and the edges below would then have nothing to attach
        // to.
        let containers: Vec<NodeId> = self
            .node_order
            .iter()
            .filter(|id| self.nodes.get(id).is_some_and(|node| node.is_container))
            .copied()
            .collect();
        for container in containers {
            self.refresh_container_pins(container);
        }

        // Rebuild edges
        for edge_data in &doc.edges {
            let edge_id = EdgeId(edge_data.id);
            let from_pin: Arc<str> = Arc::from(edge_data.from_pin.as_str());
            let to_pin: Arc<str> = Arc::from(edge_data.to_pin.as_str());

            self.executor.graph.add_edge(GraphEdge {
                id: edge_id,
                from_node: NodeId(edge_data.from_node),
                from_pin: from_pin.clone(),
                to_node: NodeId(edge_data.to_node),
                to_pin: to_pin.clone(),
                semantic: EdgeSemantic::default(),
            });

            self.edges.push(EditorEdge {
                id: edge_id,
                from_node: NodeId(edge_data.from_node),
                from_pin: PinLabel(from_pin),
                to_node: NodeId(edge_data.to_node),
                to_pin: PinLabel(to_pin),
            });
        }
        self.reindex_edges();

        // Nothing is executed: a loaded graph shows values only once the
        // runtime publishes them.
        self.update_display_values();
    }

    fn viewport_center(&self) -> Point {
        let screen_center = Point::new(640.0, 400.0);
        Point::new(
            screen_center.x / self.camera_zoom - self.camera_position.x,
            screen_center.y / self.camera_zoom - self.camera_position.y,
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
        if observes_store(&message) {
            self.flush_pending();
        }
        match message {
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
                    if let Some(gn) = self.executor.graph.node_mut(id) {
                        gn.position = (position.x, position.y);
                    }
                    // A layout is a move like a drag: every window shows it.
                    #[cfg(not(target_arch = "wasm32"))]
                    self.push_move(id, position.x, position.y);
                }
                self.autosave();
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
                self.autosave();
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
                            self.particles.remove(&edge.id);
                        }
                        self.edges.retain(|e| e.from_node != id && e.to_node != id);
                        self.executor.remove_node(id);
                        self.const_inputs.remove(&id);
                        self.node_settings.remove(&id);
                        self.setting_errors.remove(&id);
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
                self.autosave();
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
            Message::ConstValueChanged { node_id, value } => {
                let id = NodeId(node_id);
                self.const_inputs.insert(id, value.clone());
                // The parameter still goes into the editor's model so pin types
                // and the widget agree; the runtime learns it from the store.
                let node_type = self.nodes.get(&id).map(|n| n.type_id.as_str());
                match node_type {
                    Some("transform.const_f64") => {
                        if let Ok(f) = value.parse::<f64>() {
                            let _ = self.executor.set_parameter(id, "value", Value::new(f));
                        }
                    }
                    Some("transform.const_bool") => {
                        let b = value == "true" || value == "1";
                        let _ = self.executor.set_parameter(id, "value", Value::new(b));
                    }
                    Some("transform.const_string") => {
                        let _ = self.executor.set_parameter(id, "value", Value::new(value));
                    }
                    _ => {}
                }
                #[cfg(not(target_arch = "wasm32"))]
                self.push_params(id);
                self.autosave();
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
                self.apply_setting_locally(id, &key, value);
                // Renaming a boundary renames its container's pin. View-local,
                // so it does not wait.
                if key == "name" {
                    self.refresh_boundary_owner(id);
                }
                // Without a store there is nothing to hold back.
                #[cfg(target_arch = "wasm32")]
                self.settle_relations(id, &key, &was);
                // No flush before this: `autosave` writes the local document,
                // which `apply_setting_locally` has just brought up to date.
                // Flushing here would defeat the whole point, since every
                // keystroke autosaves.
                self.autosave();
            }
            Message::NodeTriggered { node_id } => {
                // The press has to reach the one process that executes, which
                // is never this one. Pushed straight to that runtime, so it
                // works from any window, local or remote.
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(endpoint) = self.endpoint.clone() {
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
                // View-local, so no reducer and no autosave: a size is what THIS
                // user wants to see, not part of the shared graph.
                self.node_sizes.insert(NodeId(node_id), size);
                // A drag only re-requests when it crosses a ladder tier; see
                // `feed::requested_box`.
                #[cfg(not(target_arch = "wasm32"))]
                return self.reconcile_runtime();
            }
            Message::Tick => {
                // No-op: re-rendering advances the widget's animation clock.
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
                // back for the debounce, so closing right after typing used to
                // drop it from the store silently -- and the autosave that
                // does hold it is not read while a store exists.
                self.flush_pending();
                self.autosave();
                // Ends the runtime rather than closing the window: a closed
                // window leaves the event loop spinning with nothing to draw.
                return iced::exit();
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
                self.autosave();
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
        }
        Task::none()
    }

    pub fn view(&self) -> Element<'_, Message> {
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
                // direction itself, so we must validate it here: exactly one
                // output and one input, distinct nodes, compatible types --
                // or, between two field pins, a relation.
                let nodes = &self.nodes;
                let edge_index = &self.edge_index;
                let converters = &self.converters;
                move |from, to| {
                    let from_id = NodeId(*from.node_id());
                    let to_id = NodeId(*to.node_id());
                    if from_id == to_id {
                        return false;
                    }
                    let (Some(f), Some(t)) = (nodes.get(&from_id), nodes.get(&to_id)) else {
                        return false;
                    };
                    let from_pin = f
                        .pin_defs
                        .iter()
                        .find(|p| &*p.name == from.pin_id().as_str());
                    let to_pin = t.pin_defs.iter().find(|p| &*p.name == to.pin_id().as_str());
                    let (Some(fp), Some(tp)) = (from_pin, to_pin) else {
                        return false;
                    };
                    // Two field pins are a relation: it carries nothing, so
                    // there is no direction to respect and no occupancy to
                    // check -- one primary key is referenced by many. Only the
                    // type has to agree, which keeps a field pin from
                    // swallowing an unrelated bidirectional pin.
                    if fp.direction == PinDirection::Both && tp.direction == PinDirection::Both {
                        return fp.ty == tp.ty;
                    }
                    // Anything else must be one output and one input, so a
                    // field pin wired to a data pin is refused: a value has
                    // nowhere to go on a pin that is not an endpoint of flow.
                    let opposite = (fp.direction == PinDirection::Output
                        && tp.direction == PinDirection::Input)
                        || (fp.direction == PinDirection::Input
                            && tp.direction == PinDirection::Output);
                    if !opposite {
                        return false;
                    }
                    // Single-slot inputs: reject a second edge here instead of
                    // silently dropping the existing one on connect. The widget
                    // excludes the edge being dragged from occupancy, so
                    // re-routing a wire back onto its own input still passes.
                    if !input_not_occupied(from) || !input_not_occupied(to) {
                        return false;
                    }
                    // Converters are directional (output type -> input type), so
                    // resolve which side is the output before checking.
                    let (out_pin, in_pin, source, target) = if fp.direction == PinDirection::Output
                    {
                        (fp, tp, from_id, to_id)
                    } else {
                        (tp, fp, to_id, from_id)
                    };
                    // A wire whose target already feeds its source closes a
                    // cycle. The runtime survives one now -- it runs the
                    // acyclic part -- but a component that quietly stops is
                    // not what the user asked for, and the drop is the one
                    // moment where saying no needs no explanation.
                    //
                    // Over the real edges, which are flat: a cycle that runs
                    // through a subgraph boundary is a cycle the executor sees,
                    // whatever the canvas draws it as.
                    if flow_reaches(edge_index, target, source) {
                        return false;
                    }
                    converters.compatible(&out_pin.ty, &in_pin.ty)
                }
            });

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
                        const_input: self.const_inputs.get(id).map(|s| s.as_str()),
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
                .executor
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
            let edge_widget =
                edge_widget.particles(self.particles.get(&edge.id).into_iter().flatten().map(
                    move |born| {
                        particle(*born, PARTICLE_SPEED).style(move |theme| ParticleStyle {
                            color: edge_color,
                            ..default_particle_style(theme)
                        })
                    },
                ));
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

        // Status bar. A failing node outranks the last UI notice: broken data is
        // the more urgent thing to say. Node errors are not computed here -- they
        // arrive with the runtime's published state, like every other value.
        let error = {
            let node_error = self.node_error_summary();
            if node_error.is_empty() {
                self.last_error.clone()
            } else {
                node_error
            }
        };
        let status_text = if error.is_empty() {
            format!(
                "  {} nodes | {} edges{}",
                self.nodes.len(),
                self.edges.len(),
                self.runtime_text(),
            )
        } else {
            format!(
                "  {} nodes | {} edges{} | ERROR: {error}",
                self.nodes.len(),
                self.edges.len(),
                self.runtime_text(),
            )
        };

        let error_color = if error.is_empty() {
            Color::from_rgb(0.5, 0.5, 0.5)
        } else {
            Color::from_rgb(0.9, 0.3, 0.3)
        };

        let status_bar = container(text(status_text).size(12).color(error_color))
            .width(Length::Fill)
            .padding(4.0)
            .style(|_theme: &Theme| container::Style {
                background: Some(Color::from_rgb(0.1, 0.1, 0.12).into()),
                ..Default::default()
            });

        column![self.breadcrumb(), graph_view, status_bar].into()
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

        // Drain remote sync events on a steady cadence while connected.
        #[cfg(not(target_arch = "wasm32"))]
        if self.stdb.is_some() {
            subs.push(
                iced::time::every(std::time::Duration::from_millis(100)).map(|_| Message::SyncPoll),
            );
        }

        Subscription::batch(subs)
    }

    pub fn theme(&self) -> Theme {
        Theme::Dark
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl App {
    /// Serializes one node's current data (id, type, position, params) for a
    /// reducer call.
    fn node_data(&self, id: NodeId) -> Option<NodeData> {
        let node = self.nodes.get(&id)?;
        let mut params: Vec<(String, String)> = self
            .const_inputs
            .get(&id)
            .map(|v| vec![("value".to_string(), v.clone())])
            .unwrap_or_default();
        if let Some(settings) = self.node_settings.get(&id) {
            params.extend(settings.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        Some(NodeData {
            id: id.0,
            type_id: node.type_id.clone(),
            display_name: node.display_name.clone(),
            x: node.position.x,
            y: node.position.y,
            params,
            parent: node.parent.0,
        })
    }

    // Send: local edits -> reducers. Guarded by `applying_remote` so a change
    // applied from the store does not echo back as a new reducer call.
    //
    // Every one of them goes through [`Self::dispatch`], which tries the store
    // and keeps the edit when it cannot: a reducer call that failed used to be
    // a log line nobody reads, so a session's worth of work vanished the
    // moment the host went away.
    fn push_node(&mut self, id: NodeId) {
        if let Some(nd) = self.node_data(id) {
            self.dispatch(Outbound::Node(nd));
        }
    }

    fn push_params(&mut self, id: NodeId) {
        if let Some(nd) = self.node_data(id) {
            self.dispatch(Outbound::Params(id, nd.params));
        }
    }

    fn push_move(&mut self, id: NodeId, x: f32, y: f32) {
        self.dispatch(Outbound::Move(id, x, y));
    }

    fn push_delete(&mut self, id: NodeId) {
        self.dispatch(Outbound::Delete(id));
    }

    fn push_edge(&mut self, e: EdgeData) {
        self.dispatch(Outbound::Connect(e));
    }

    fn push_edge_remove(&mut self, id: EdgeId) {
        self.dispatch(Outbound::Disconnect(id));
    }

    /// Sends one edit to the store, or keeps it until the store is back.
    ///
    /// Silent while a remote change is being applied: that would echo the
    /// change straight back as a new reducer call.
    fn dispatch(&mut self, edit: Outbound) {
        if self.applying_remote {
            return;
        }
        let sent = self
            .stdb
            .as_ref()
            .and_then(crate::sync::Store::conn)
            .map(|conn| send_outbound(conn, &edit));
        match sent {
            Some(Ok(())) => {}
            Some(Err(e)) => {
                // Once per distinct message: a store that refuses every call
                // would otherwise print one line per keystroke.
                if self.logged_sends.insert(e.clone()) {
                    eprintln!("[stdb] {e}");
                }
                self.outbox.push(edit);
            }
            // No connection at all: nothing to say that the status bar is not
            // already saying.
            None => self.outbox.push(edit),
        }
    }

    /// Replays what the store never received, oldest first.
    ///
    /// Order is the whole point: a node has to exist before an edge names it,
    /// and a delete has to come after the create it undoes. An edit that fails
    /// again stays queued, and everything after it stays behind it.
    fn flush_outbox(&mut self) {
        if self.outbox.is_empty() {
            return;
        }
        let Some(conn) = self.stdb.as_ref().and_then(crate::sync::Store::conn) else {
            return;
        };
        let queued = std::mem::take(&mut self.outbox);
        let total = queued.len();
        let mut kept: Vec<Outbound> = Vec::new();
        for edit in queued {
            if !kept.is_empty() {
                kept.push(edit);
                continue;
            }
            if let Err(e) = send_outbound(conn, &edit) {
                eprintln!("[stdb] {e} (keeping {} edits)", total - kept.len());
                kept.push(edit);
            }
        }
        if kept.is_empty() {
            eprintln!("[stdb] {total} local edit(s) reached the store");
        }
        self.outbox = kept;
    }

    // Receive: drain queued remote events and apply them to the editor.

    /// Settles which wire owns a single-slot input pin, deleting the losers
    /// here, in the executor and in the store. Returns whether `arriving` won.
    ///
    /// Only the remote path needs it: a local connect cannot land on an
    /// occupied input (`can_connect` rejects it), but two windows can each
    /// draw one without seeing the other. The verdict comes from
    /// [`occupancy_winner`] rather than from arrival order, so every window
    /// and every runner keeps the same wire -- ordering by arrival used to
    /// leave each window with whichever row reached it last, and a window
    /// opened afterwards with a third answer.
    ///
    /// The deletion is pushed even while a remote change is being applied.
    /// That guard is there to stop an echo, and this is not one: the row
    /// deleted is a different edge than the one that arrived. Every window
    /// that sees both rows issues the same delete and the reducer is
    /// idempotent, so agreeing is cheap and leaving the row is not -- nothing
    /// would ever remove it and it would outlive every view that dropped it.
    fn resolve_input_occupancy(&mut self, arriving: EdgeId, to_node: NodeId, to_pin: &str) -> bool {
        let mut contenders: Vec<EdgeId> = self
            .edges
            .iter()
            .filter(|e| e.to_node == to_node && e.to_pin.as_str() == to_pin)
            .map(|e| e.id)
            .collect();
        if contenders.is_empty() {
            return true;
        }
        contenders.push(arriving);
        let winner = occupancy_winner(contenders.iter().copied()).expect("contenders is not empty");
        for loser in contenders.into_iter().filter(|id| *id != winner) {
            self.forget_edge(loser);
            // Not through `dispatch`: that is silent while a remote change is
            // being applied, which is right for an echo and wrong here -- the
            // row deleted is a different edge than the one that arrived. If
            // the store is gone the loser's row goes with it anyway.
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(conn) = self.store_conn() {
                let _ = crate::sync::send_disconnect_edge(conn, loser.0);
            }
        }
        winner == arriving
    }

    fn drain_sync(&mut self) {
        let mut events = Vec::new();
        if let Some(rx) = &self.sync_rx {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }
        if events.is_empty() {
            return;
        }
        self.applying_remote = true;
        for ev in events {
            self.apply_sync_event(ev);
        }
        // The node an edge was waiting for may have been in this very batch.
        self.resolve_pending_edges();
        self.applying_remote = false;
        // After the guard, not during: deriving a database node's parameters is
        // this editor's own decision and has to reach the store, which
        // `push_params` refuses while a remote change is being applied. It is
        // also the whole batch that decides the answer -- the table a node
        // reads its columns from may have arrived in it.
        self.derive_all_db_params();
        // A batch that added an edge changed what every node it touches shows:
        // the value was already known, the wire to carry it was not. Nothing
        // else refreshes the display map on the remote path, so a graph built
        // by another window used to sit there with the wires drawn and the
        // bodies empty until the next value arrived.
        self.update_display_values();
    }

    fn apply_sync_event(&mut self, ev: crate::sync::SyncEvent) {
        use crate::sync::SyncEvent;
        match ev {
            SyncEvent::NodeUpsert(nd) => self.apply_node_upsert(nd),
            SyncEvent::NodeRemove(id) => self.apply_node_remove(NodeId(id)),
            SyncEvent::EdgeInsert(ed) => self.apply_edge_insert(ed),
            SyncEvent::EdgeRemove(id) => self.apply_edge_remove(EdgeId(id)),
            SyncEvent::RuntimesChanged => self.apply_runtimes_changed(),
            // The batch may have brought this editor's first look at the
            // runtime table, which is where the endpoint to dial comes from.
            SyncEvent::SubscriptionApplied => {}
            // The store is back: whatever this window edited while it was gone
            // has not reached it, and now can.
            SyncEvent::Connected => self.flush_outbox(),
            // Said in the status bar rather than here: `Store::is_live` is the
            // authority, and it answers without waiting for an event.
            SyncEvent::Disconnected => {}
        }
    }

    /// Re-reads whether anything is executing the graph.
    ///
    /// The editor is not a candidate -- it registers as `Role::Viewer` and never
    /// appears in the runtime table -- so this is purely informational. It is
    /// still the difference between a live number and a stale one, which is the
    /// one thing a user must not have to guess about.
    fn apply_runtimes_changed(&mut self) {
        if let Some(conn) = self.store_conn() {
            self.runtimes = crate::sync::runtime_count(conn);
        }
    }

    fn apply_node_upsert(&mut self, nd: NodeData) {
        let id = NodeId(nd.id);
        if self.nodes.contains_key(&id) {
            let pos = Point::new(nd.x, nd.y);
            if let Some(en) = self.nodes.get_mut(&id) {
                en.position = pos;
            }
            if let Some(gn) = self.executor.graph.node_mut(id) {
                gn.position = (pos.x, pos.y);
            }
            self.apply_params(id, &nd.type_id, &nd.params);
        } else {
            self.insert_node_from_data(&nd);
            // A value can arrive before the node row it belongs to: this is
            // where the one that was waiting is applied.
            #[cfg(not(target_arch = "wasm32"))]
            self.reapply_remote_outputs(id);
        }
        // A boundary node arriving from another window is a pin its container
        // gains here, and a renamed one is a pin that changed name.
        self.refresh_container_pins(NodeId(nd.parent));
        // The database parameters this node needs are derived once the whole
        // batch has been applied (`derive_all_db_params`): they have to reach
        // the store, and `push_params` is silent while a remote change is
        // being applied.
    }

    /// The relations a table's fields declare, one `field -> table.field` per
    /// line, as the `relations` parameter carries them.
    ///
    /// Only the referencing side of each relation writes a line; which side
    /// that is follows from [`relation_references_to`].
    ///
    /// Sorted, because this text is an output the runner compares: a set that
    /// reordered itself would look like a change on every pass.
    #[cfg(not(target_arch = "wasm32"))]
    fn relations_of(&self, node: NodeId) -> String {
        let mut lines: Vec<String> = Vec::new();
        for edge in &self.edges {
            if !self.is_relation(edge) {
                continue;
            }
            let to_is_key = relation_references_to(edge.from_pin.as_str(), edge.to_pin.as_str());
            let (referencing, referenced) = if to_is_key {
                (
                    (edge.from_node, &edge.from_pin),
                    (edge.to_node, &edge.to_pin),
                )
            } else {
                (
                    (edge.to_node, &edge.to_pin),
                    (edge.from_node, &edge.from_pin),
                )
            };
            if referencing.0 != node {
                continue;
            }
            let table = self.setting_or_default(referenced.0, "name");
            if table.is_empty() {
                continue;
            }
            lines.push(format!(
                "{} -> {}.{}",
                referencing.1.as_str(),
                table,
                referenced.1.as_str()
            ));
        }
        lines.sort();
        lines.dedup();
        lines.join("\n")
    }

    /// Fills in the parameters a database node cannot know by itself: which
    /// file it works on, the columns of the table it is wired to, and the
    /// relations a table's fields declare.
    ///
    /// Derived rather than typed twice. A node inside a `db.database` works on
    /// that database, an insert wired to a table has that table's columns, and
    /// a wire between two field pins is a foreign key -- restating any of them
    /// by hand is a chance for the two to disagree. They are ordinary
    /// parameters from the runner's point of view, so nothing on that side has
    /// to know they were derived, and no wire between fields has to reach it.
    #[cfg(not(target_arch = "wasm32"))]
    fn derive_db_params(&mut self, node: NodeId) {
        let Some(type_id) = self.nodes.get(&node).map(|n| n.type_id.clone()) else {
            return;
        };
        if !type_id.starts_with("db.") {
            return;
        }
        let mut derived: Vec<(String, String)> = Vec::new();

        let parent = self.nodes.get(&node).map(|n| n.parent).unwrap_or(NodeId(0));
        // An empty path is the honest answer for a node outside a database:
        // the node reports that instead of creating a file somewhere.
        let path = if self
            .nodes
            .get(&parent)
            .is_some_and(|p| p.type_id == "db.database")
        {
            self.setting_or_default(parent, "path")
        } else {
            String::new()
        };
        derived.push((zeughaus_db::DB_PATH.to_string(), path));

        if matches!(type_id.as_str(), "db.insert" | "db.query") {
            let source = self
                .edges
                .iter()
                .find(|e| e.to_node == node && e.to_pin.as_str() == "table")
                .map(|e| e.from_node)
                .filter(|from| {
                    self.nodes
                        .get(from)
                        .is_some_and(|n| n.type_id == "db.table")
                });
            let columns = source
                .map(|table| self.setting_or_default(table, "columns"))
                .unwrap_or_default();
            derived.push(("columns".to_string(), columns));
        }

        if type_id == "db.table" {
            derived.push((zeughaus_db::RELATIONS.to_string(), self.relations_of(node)));
        }

        let mut changed = false;
        for (key, value) in derived {
            if self
                .node_settings
                .get(&node)
                .and_then(|s| s.get(&key))
                .is_some_and(|current| *current == value)
            {
                continue;
            }
            self.node_settings
                .entry(node)
                .or_default()
                .insert(key.clone(), value.clone());
            let _ = self.executor.set_parameter(node, &key, Value::new(value));
            changed = true;
        }
        if changed {
            // The column list decides an insert's pins, and the store carries
            // the parameter to the runner, which derives nothing itself.
            self.refresh_node_pins(node);
            self.push_params(node);
        }
    }

    /// Re-derives the database parameters of every node that depends on this
    /// one: the children of a database, everything a table feeds, and every
    /// table it is in a relation with (whose foreign key names this one).
    #[cfg(not(target_arch = "wasm32"))]
    fn derive_db_dependents(&mut self, node: NodeId) {
        let Some(type_id) = self.nodes.get(&node).map(|n| n.type_id.clone()) else {
            return;
        };
        let dependents: Vec<NodeId> = match type_id.as_str() {
            "db.database" => self.children(node).map(|child| child.id).collect(),
            "db.table" => self
                .edges
                .iter()
                .filter(|e| {
                    (e.from_node == node && e.to_pin.as_str() == "table") || self.is_relation(e)
                })
                .filter_map(|e| match (e.from_node == node, e.to_node == node) {
                    (true, _) => Some(e.to_node),
                    (_, true) => Some(e.from_node),
                    _ => None,
                })
                .collect(),
            _ => return,
        };
        for dependent in dependents {
            self.derive_db_params(dependent);
        }
    }

    /// Re-derives every database node's parameters.
    ///
    /// Cheap and idempotent: a node whose derived values are unchanged writes
    /// nothing. Called once per applied batch, because a batch can move any of
    /// the three things the answer depends on -- the node's parent, a
    /// database's path, a table's columns.
    #[cfg(not(target_arch = "wasm32"))]
    fn derive_all_db_params(&mut self) {
        let database_nodes: Vec<NodeId> = self
            .node_order
            .iter()
            .filter(|id| {
                self.nodes
                    .get(id)
                    .is_some_and(|node| node.type_id.starts_with("db."))
            })
            .copied()
            .collect();
        for node in database_nodes {
            self.derive_db_params(node);
        }
    }

    fn apply_params(&mut self, id: NodeId, type_id: &str, params: &[(String, String)]) {
        let is_const = type_id.starts_with("transform.const_");
        for (name, value_str) in params {
            // A key this window still owes the store is one the user is
            // typing: the shared value for it is older than what is on screen
            // (usually this window's own echo, one debounce behind), and
            // adopting it snaps the field back mid-word and then pushes the
            // snapped-back text. The remote value is taken for that key the
            // next time the row arrives with nothing owed.
            #[cfg(not(target_arch = "wasm32"))]
            if self.pending.owes(id, name) {
                continue;
            }
            match type_id {
                "transform.const_f64" => {
                    if let Ok(f) = value_str.parse::<f64>() {
                        let _ = self.executor.set_parameter(id, name, Value::new(f));
                    }
                }
                "transform.const_bool" => {
                    let b = value_str == "true" || value_str == "1";
                    let _ = self.executor.set_parameter(id, name, Value::new(b));
                }
                "transform.const_string" => {
                    let _ = self
                        .executor
                        .set_parameter(id, name, Value::new(value_str.clone()));
                }
                _ => {
                    let _ = self
                        .executor
                        .set_parameter(id, name, Value::new(value_str.clone()));
                }
            }
            if is_const {
                self.const_inputs.insert(id, value_str.clone());
            } else {
                self.node_settings
                    .entry(id)
                    .or_default()
                    .insert(name.clone(), value_str.clone());
            }
        }
        // A setting can decide a node's pins; the widget has to draw the set
        // the node now declares, not the one it was created with.
        self.refresh_node_pins(id);
    }

    fn apply_node_remove(&mut self, id: NodeId) {
        let parent = self.nodes.get(&id).map(|node| node.parent);
        self.nodes.remove(&id);
        self.node_order.retain(|n| *n != id);
        #[cfg(not(target_arch = "wasm32"))]
        for edge in self
            .edges
            .iter()
            .filter(|e| e.from_node == id || e.to_node == id)
        {
            self.particles.remove(&edge.id);
        }
        self.edges.retain(|e| e.from_node != id && e.to_node != id);
        self.reindex_edges();
        self.executor.remove_node(id);
        self.const_inputs.remove(&id);
        self.node_settings.remove(&id);
        self.setting_errors.remove(&id);
        // A value whose producer is gone is not a value any more, and node ids
        // are never reused, so nothing can inherit it. Same for what this
        // window still owed the store: the row it would have updated is gone.
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.pending.take(id);
            self.remote_outputs.remove(&id);
            self.output_seq.retain(|(node, _), _| *node != id);
            self.remote_errors.remove(&id);
            self.error_seq.remove(&id);
        }
        // A removed boundary node is a pin its container loses; a removed
        // container is a graph nobody can be looking at any more.
        if let Some(parent) = parent {
            self.refresh_container_pins(parent);
        }
        if self.current_graph == id {
            self.current_graph = NodeId(0);
        }
        self.cameras.remove(&id);
    }

    fn apply_edge_insert(&mut self, ed: EdgeData) {
        let edge_id = EdgeId(ed.id);
        if self.edges.iter().any(|e| e.id == edge_id) {
            return;
        }
        let from_node = NodeId(ed.from_node);
        let to_node = NodeId(ed.to_node);
        // An edge naming a node this window does not have yet is kept, not
        // dropped: a subscription applies as one burst with no ordering between
        // tables, so edges routinely arrive before their nodes. Dropping them
        // is why a freshly opened editor showed 2 of 6 wires.
        if !self.nodes.contains_key(&from_node) || !self.nodes.contains_key(&to_node) {
            self.pending_edges.push(ed);
            return;
        }
        let from_pin: Arc<str> = Arc::from(ed.from_pin.as_str());
        let to_pin: Arc<str> = Arc::from(ed.to_pin.as_str());
        // A field pin is not a single-slot input: a primary key is referenced
        // by many, so a relation landing on it displaces nothing.
        if !self.is_field_pin(to_node, &to_pin)
            && !self.resolve_input_occupancy(edge_id, to_node, &to_pin)
        {
            // This wire lost the pin to one already there. It is gone from the
            // store by now, so there is nothing left to draw.
            return;
        }
        self.executor.graph.add_edge(GraphEdge {
            id: edge_id,
            from_node,
            from_pin: from_pin.clone(),
            to_node,
            to_pin: to_pin.clone(),
            semantic: EdgeSemantic::default(),
        });
        self.edges.push(EditorEdge {
            id: edge_id,
            from_node,
            from_pin: PinLabel(from_pin),
            to_node,
            to_pin: PinLabel(to_pin),
        });
        self.reindex_edges();
        self.executor.on_edge_added(edge_id);
        self.resync_pins(to_node);
    }

    fn apply_edge_remove(&mut self, id: EdgeId) {
        self.pending_edges.retain(|ed| ed.id != id.0);
        if let Some(pos) = self.edges.iter().position(|e| e.id == id) {
            let edge = self.edges.remove(pos);
            self.reindex_edges();
            self.executor.disconnect_edge(edge.id);
            self.resync_pins(edge.to_node);
            // The wire the particles were riding is gone.
            #[cfg(not(target_arch = "wasm32"))]
            self.particles.remove(&edge.id);
        }
    }

    /// Retries edges that named a node this window did not have yet. Called once
    /// per drained batch, because the node they were waiting for may have been
    /// in the same batch.
    fn resolve_pending_edges(&mut self) {
        if self.pending_edges.is_empty() {
            return;
        }
        for ed in std::mem::take(&mut self.pending_edges) {
            self.apply_edge_insert(ed);
        }
    }
}

/// Whether handling this message reads or changes the shared graph, and
/// therefore has to see the store the local view already shows.
///
/// The settings-editing messages are absent by design: holding them back from
/// the store until the typing stops is exactly what this list protects.
/// `SyncPoll` is absent too -- it commits what has settled instead, and
/// flushing there would make the delay meaningless.
fn observes_store(message: &Message) -> bool {
    matches!(
        message,
        Message::EdgeConnected { .. }
            | Message::EdgeDisconnected { .. }
            | Message::EnterGraph(_)
            | Message::AutoLayout
            | Message::GroupMoved { .. }
            | Message::CloneNodes(_)
            // `DeleteNodes` is missing on purpose: it flushes itself, after
            // dropping what the doomed nodes owed.
            | Message::SpawnNode { .. }
            | Message::NodeTriggered { .. }
            | Message::SaveGraph
            | Message::GraphLoaded(_)
    )
}

/// Default content size of a Display node before the user resizes it. Wide
/// enough that a downscaled 16:9 frame is recognizable.
const DISPLAY_SIZE: iced::Size = iced::Size::new(240.0, 150.0);

/// Width of every other node. Their content is text and pin rows, which do not
/// benefit from being resizable.
const NODE_WIDTH: f32 = 180.0;

/// Width of a node whose body holds a field row editor: a name field, a type
/// choice and a remove button side by side need more than a pin label does.
const FIELDS_NODE_WIDTH: f32 = 260.0;

/// The Display node: the one node type whose body shows data rather than pin
/// rows. That makes it the only one worth resizing, and the only one that asks
/// the runtime for a video feed.
fn is_display(type_id: &str) -> bool {
    type_id == "transform.display"
}

/// Halves a color's brightness, marking a pin or edge that currently carries no
/// value. Scaling the channels (rather than the alpha) keeps the hue readable
/// against both the canvas and a node body.
fn dim(c: Color) -> Color {
    Color {
        r: c.r * 0.5,
        g: c.g * 0.5,
        b: c.b * 0.5,
        a: c.a,
    }
}

/// Whether pin `index` is marked dim in a [`App::dim_mask`]. Pins beyond the
/// mask's 64 slots are treated as live -- a node with that many outputs has no
/// meaningful "empty pin" story anyway.
fn is_dim(mask: u64, index: usize) -> bool {
    index < 64 && mask & (1 << index) != 0
}

/// What the node said about this setting, drawn under the field.
///
/// Always a widget, empty when there is nothing to say: the node body's child
/// count has to stay the same between redraws, or iced's widget state is
/// matched against the wrong element.
fn setting_refusal<'a>(
    errors: Option<&'a HashMap<String, String>>,
    def: &'a SettingDef,
) -> iced::widget::Text<'a, Theme> {
    let message = errors
        .and_then(|e| e.get(&*def.name))
        .map(String::as_str)
        .unwrap_or("");
    text(message).size(10).color(Color::from_rgb(0.9, 0.4, 0.4))
}

/// Whether `start` can reach `goal` by following dataflow edges of `index`.
///
/// What makes a wire a cycle: a wire from `goal` to `start` closes one exactly
/// when this is true. Only `flow_out` is followed, so a relation between two
/// table fields is not a path -- two tables referencing each other is a legal
/// schema, and the runtime already keeps such an edge out of execution.
///
/// The executor now runs the acyclic part of a graph that has a cycle, so this
/// is not the last line of defence. It is the only place where refusing needs
/// no explanation: at the drop, nothing has happened yet.
fn flow_reaches(index: &HashMap<NodeId, NodeEdges>, start: NodeId, goal: NodeId) -> bool {
    let mut seen: HashSet<NodeId> = HashSet::from([start]);
    let mut stack = vec![start];
    while let Some(node) = stack.pop() {
        let Some(edges) = index.get(&node) else {
            continue;
        };
        for next in &edges.flow_out {
            if *next == goal {
                return true;
            }
            if seen.insert(*next) {
                stack.push(*next);
            }
        }
    }
    false
}

/// The containers between the root graph and `graph`, outermost first.
///
/// `parent_of` answers what a node's parent is, or `None` for a node this
/// window does not have. The walk is bounded by a visited set: `parent` is an
/// arbitrary column of the store's `node` table, so a single hand-written row
/// naming itself -- or a pair naming each other -- used to make `view` loop
/// forever pushing breadcrumb entries. A cycle stops the walk where it closes,
/// and the trail shows the part of it that is a path.
fn ancestry(graph: NodeId, parent_of: impl Fn(NodeId) -> Option<NodeId>) -> Vec<NodeId> {
    let mut trail = Vec::new();
    let mut seen: HashSet<NodeId> = HashSet::new();
    let mut current = graph;
    while current != NodeId(0) && seen.insert(current) {
        let Some(parent) = parent_of(current) else {
            break;
        };
        trail.push(current);
        current = parent;
    }
    trail.reverse();
    trail
}

/// Everything the widget needs about one node besides the node itself: what it
/// shows, what has been typed into it, and what the node said about that.
struct NodeChrome<'a> {
    display: Option<&'a DisplayValue>,
    const_input: Option<&'a str>,
    settings: Option<&'a HashMap<String, String>>,
    /// What the node refused, per setting key.
    errors: Option<&'a HashMap<String, String>>,
    /// Why the node's last run failed, as the runtime reported it.
    failure: Option<&'a str>,
    dim_mask: u64,
    size: Option<iced::Size>,
    is_container: bool,
}

fn build_node_element<'a>(
    node: &'a EditorNode,
    chrome: NodeChrome<'a>,
) -> Element<'a, Message, Theme> {
    let NodeChrome {
        display,
        const_input,
        settings,
        errors,
        failure,
        dim_mask,
        size,
        is_container,
    } = chrome;
    let is_const = node.type_id.starts_with("transform.const_");
    let is_button = node.type_id == "flow.button";
    let is_display = is_display(&node.type_id);

    let mut items: Vec<Element<'_, Message, Theme>> = Vec::new();

    // A container's body is a way in. Its pins come from the boundary nodes
    // inside it, so without this the node would be a box with no purpose.
    if is_container {
        items.push(
            button(text("open").size(11))
                .padding(2.0)
                .on_press(Message::EnterGraph(node.id.0))
                .into(),
        );
    }

    if is_const {
        // Const nodes get an inline text input with output pin
        let input_text = const_input.unwrap_or("");
        let node_raw_id = node.id.0;
        let placeholder = match node.type_id.as_str() {
            "transform.const_f64" => "0",
            "transform.const_bool" => "false",
            "transform.const_string" => "text",
            _ => "",
        };
        let first_pin = node.pin_defs.first();
        let pin_tint = first_pin.map_or_else(|| pin_color(&Ty::Any), |p| pin_color(&p.ty));
        let pin_kind = first_pin.map(|p| p.pin_kind).unwrap_or(PinKind::Sample);

        let input_field = text_input(placeholder, input_text)
            .on_input(move |v| Message::ConstValueChanged {
                node_id: node_raw_id,
                value: v,
            })
            .size(13)
            .width(Length::Fill);

        let pin: Element<'_, Message, Theme> =
            node_pin(PinSide::Right, PinLabel::from("value"), input_field)
                .direction(NgPinDirection::Output)
                .info(PinVisual {
                    color: pin_tint,
                    shape: pin_shape(pin_kind),
                })
                .into();
        items.push(pin);
    } else if is_button {
        // The button IS the pin content: pressing it is the event this node
        // exists to produce, so there is nothing else worth showing.
        let node_raw_id = node.id.0;
        let first_pin = node.pin_defs.first();
        let tint = first_pin.map_or_else(|| pin_color(&Ty::Bool), |p| pin_color(&p.ty));
        let color = if is_dim(dim_mask, 0) { dim(tint) } else { tint };
        let press = button(text("Trigger").size(13).center())
            .on_press(Message::NodeTriggered {
                node_id: node_raw_id,
            })
            .width(Length::Fill);
        let pin: Element<'_, Message, Theme> =
            node_pin(PinSide::Right, PinLabel::from("out"), press)
                .direction(NgPinDirection::Output)
                .info(PinVisual {
                    color,
                    shape: pin_shape(PinKind::Trigger),
                })
                .into();
        items.push(pin);
    } else if is_display {
        // The Display node's whole body is its readout: the input pin wraps the
        // content instead of a label, which also anchors the pin at the left
        // edge's midpoint.
        let body: Element<'_, Message, Theme> = match display {
            Some(DisplayValue::Frame { handle, .. }) => image(handle.clone())
                .content_fit(ContentFit::Contain)
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
            Some(DisplayValue::Text(val)) => text(val.as_str())
                .size(14)
                .color(Color::from_rgb(0.9, 0.9, 0.5))
                .into(),
            None => text("").size(14).into(),
        };
        let pin_def = node.pin_defs.first();
        let name = pin_def.map_or_else(|| Arc::from("input"), |p| p.name.clone());
        let visual = PinVisual {
            color: pin_def.map_or_else(|| pin_color(&Ty::Any), |p| pin_color(&p.ty)),
            shape: pin_shape(pin_def.map(|p| p.pin_kind).unwrap_or(PinKind::Trigger)),
        };
        let pin: Element<'_, Message, Theme> = node_pin(
            PinSide::Left,
            PinLabel(name),
            container(body)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill),
        )
        .direction(NgPinDirection::Input)
        .info(visual)
        .into();
        items.push(pin);
    } else {
        // A title is the node's heading, so it comes before anything else.
        for def in &node.settings {
            if def.kind != SettingKind::Title {
                continue;
            }
            items.push(title_setting(node, def, setting_value(settings, def)));
            items.push(setting_refusal(errors, def).into());
        }
        // A field-list setting draws its own pins: each row IS a pin spanning
        // the node, so the plain pin loop below must not draw them a second
        // time.
        let mut fields: HashSet<&str> = HashSet::new();
        for def in &node.settings {
            let SettingKind::Fields { types } = &def.kind else {
                continue;
            };
            let current = setting_value(settings, def);
            fields.extend(field_rows(current).into_iter().map(|(name, _)| name));
            items.extend(field_setting(node, def, types, current, dim_mask));
            items.push(setting_refusal(errors, def).into());
        }

        for (index, pin_def) in node.pin_defs.iter().enumerate() {
            if fields.contains(&*pin_def.name) {
                continue;
            }
            let (side, direction) = pin_geometry(pin_def.direction);

            let tint = pin_color(&pin_def.ty);
            let visual = PinVisual {
                color: if is_dim(dim_mask, index) {
                    dim(tint)
                } else {
                    tint
                },
                shape: pin_shape(pin_def.pin_kind),
            };

            let pin: Element<'_, Message, Theme> = node_pin(
                side,
                PinLabel(pin_def.name.clone()),
                text(&*pin_def.name).size(12),
            )
            .direction(direction)
            .info(visual)
            .into();
            items.push(pin);
        }
    }

    // In-node text settings (e.g. LLM base_url/model/prompt). Each renders a
    // labeled text input that updates the node parameter on edit. The number of
    // settings is fixed per node type, so the widget tree stays stable. Titles
    // and field lists are already drawn above.
    for def in &node.settings {
        if matches!(def.kind, SettingKind::Title | SettingKind::Fields { .. }) {
            continue;
        }
        let node_raw_id = node.id.0;
        let key = def.name.clone();
        let current = setting_value(settings, def);

        let field = text_input(&def.placeholder, current)
            .on_input(move |v| Message::NodeSettingChanged {
                node_id: node_raw_id,
                key: key.to_string(),
                value: v,
            })
            .size(12)
            .width(Length::Fill);

        items.push(
            column![
                text(&*def.name)
                    .size(11)
                    .color(Color::from_rgb(0.6, 0.6, 0.6)),
                field,
                setting_refusal(errors, def)
            ]
            .spacing(1)
            .into(),
        );
    }

    // Always render a value display row to keep widget tree structure stable.
    // Empty text when no value -- prevents iced widget state downcast panics
    // caused by children count changing between view() calls. The Display and
    // Button nodes are exempt: their body already IS the value or the control.
    if !is_const && !is_display && !is_button {
        let value_text = match display {
            Some(DisplayValue::Text(val)) => val.as_str(),
            // A frame has no text form; the Display node is where it is shown.
            Some(DisplayValue::Frame { .. }) => "frame",
            None => "",
        };
        let prefix = if value_text.is_empty() { "" } else { "= " };
        items.push(
            row![
                text(prefix).size(12).color(Color::from_rgb(0.6, 0.6, 0.6)),
                text(value_text)
                    .size(13)
                    .color(Color::from_rgb(0.9, 0.9, 0.5))
            ]
            .spacing(2)
            .into(),
        );
    }

    // Why the last run failed, in the node that failed. The red border says
    // THAT something is wrong; only the text says what, and the status bar
    // shows one node's message at a time. Always a widget so the body's child
    // count stays stable between redraws.
    items.push(
        text(failure.unwrap_or(""))
            .size(10)
            .color(Color::from_rgb(0.95, 0.45, 0.45))
            .into(),
    );

    let body = column(items).spacing(4);
    let header = node_header(
        text(node.display_name.as_str())
            .size(14)
            .color(Color::from_rgb(0.92, 0.92, 0.95)),
        header_color(&node.type_id),
        8.0,
    );
    let inner = column![header, container(body).padding(6.0)];
    if is_display {
        let size = size.unwrap_or(DISPLAY_SIZE);
        container(inner)
            .width(size.width)
            .height(size.height)
            .into()
    } else if node
        .settings
        .iter()
        .any(|def| matches!(def.kind, SettingKind::Fields { .. }))
    {
        // A field row is three controls side by side; at the usual width they
        // would each be too narrow to read.
        container(inner).width(FIELDS_NODE_WIDTH).into()
    } else {
        container(inner).width(NODE_WIDTH).into()
    }
}

/// A setting's current value, or the default its type declares.
fn setting_value<'a>(
    settings: Option<&'a HashMap<String, String>>,
    def: &'a SettingDef,
) -> &'a str {
    settings
        .and_then(|m| m.get(&*def.name))
        .map(|s| s.as_str())
        .unwrap_or(&def.default)
}

/// Where a pin sits on the node, and what it may connect to.
///
/// A core `Both` pin becomes a [`PinSide::Row`] spanning the node: an edge
/// attaches on whichever border is nearer its other end, because what such an
/// edge declares is a relationship between the two nodes and not a value
/// travelling one way.
fn pin_geometry(direction: PinDirection) -> (PinSide, NgPinDirection) {
    match direction {
        PinDirection::Input => (PinSide::Left, NgPinDirection::Input),
        PinDirection::Output => (PinSide::Right, NgPinDirection::Output),
        PinDirection::Both => (PinSide::Row, NgPinDirection::Both),
    }
}

/// Whether an edge end is a relation the node has lost, given what each end's
/// node declares for the pin the edge lands on.
///
/// Two conditions, and both are load-bearing:
///
/// * the OTHER end is a field pin -- that is what made the edge a relation
///   rather than a wire carrying a value;
/// * this end's pin is not declared at all -- the field it sat on is gone.
///
/// A pin that is still declared, field or not, is not lost. Treating "not a
/// field pin" as "gone" is how an ordinary output wired to a table's field --
/// the shape every graph written before relations existed has -- got deleted
/// from the store the moment any setting on that node was touched.
fn is_lost_relation(own: Option<&PinDefinition>, other: Option<&PinDefinition>) -> bool {
    own.is_none() && other.is_some_and(|p| p.direction == PinDirection::Both)
}

/// The field name a relation treats as the key it points at.
#[cfg(not(target_arch = "wasm32"))]
const KEY_FIELD: &str = "id";

/// Whether the `to` end of a relation is the referenced one.
///
/// The end whose field is named `id` is referenced, and if neither is, the end
/// the wire was dropped on. `id` is already what the database nodes treat as
/// the key -- an `id:int` field is SQLite's rowid alias and an insert lets it
/// be assigned -- so a foreign key pointing at it needs no second declaration,
/// and dragging from either side gives the same schema. Both ends named `id`
/// falls back to the drop target, which is the only tie left to break.
///
/// Native only, like the `relations` parameter it decides: the database plugin
/// links SQLite and the browser editor cannot have it.
#[cfg(not(target_arch = "wasm32"))]
fn relation_references_to(from_pin: &str, to_pin: &str) -> bool {
    to_pin == KEY_FIELD || from_pin != KEY_FIELD
}

/// A [`SettingKind::Title`] setting: the node's name, editable in place.
fn title_setting<'a>(
    node: &'a EditorNode,
    def: &'a SettingDef,
    current: &'a str,
) -> Element<'a, Message, Theme> {
    let node_raw_id = node.id.0;
    let key = def.name.clone();
    text_input(&def.placeholder, current)
        .on_input(move |v| Message::NodeSettingChanged {
            node_id: node_raw_id,
            key: key.to_string(),
            value: v,
        })
        .size(14)
        .width(Length::Fill)
        .into()
}

/// A [`SettingKind::Fields`] setting: one row per field, plus a way to add one.
///
/// Each row is a pin spanning the node ([`PinSide::Row`]) whose content is the
/// field's editor -- name, type, remove. So an edge can attach to a field on
/// either border, which is what a relation between two tables is drawn as.
///
/// Every edit renders the whole value again and sends it as one
/// [`Message::NodeSettingChanged`]. The setting therefore stays the plain
/// `name:type` text that the store, the runner and the node's own parser
/// already speak, and this editor needs to know nothing about what the fields
/// mean.
fn field_setting<'a>(
    node: &'a EditorNode,
    def: &'a SettingDef,
    types: &'a [String],
    current: &'a str,
    dim_mask: u64,
) -> Vec<Element<'a, Message, Theme>> {
    let node_raw_id = node.id.0;
    let rows: Arc<Vec<(&'a str, &'a str)>> = Arc::new(field_rows(current));
    let options: Vec<&'a str> = types.iter().map(String::as_str).collect();
    let mut items: Vec<Element<'a, Message, Theme>> = Vec::with_capacity(rows.len() + 1);

    for (index, (name, ty)) in rows.iter().copied().enumerate() {
        let rename = {
            let (rows, key) = (Arc::clone(&rows), def.name.clone());
            text_input("field", name)
                .on_input(move |v| Message::NodeSettingChanged {
                    node_id: node_raw_id,
                    key: key.to_string(),
                    value: field_edit(&rows, index, Some((v.trim(), ty))),
                })
                .size(12)
                .width(Length::Fill)
        };
        let retype = {
            let (rows, key) = (Arc::clone(&rows), def.name.clone());
            pick_list(options.clone(), Some(ty), move |chosen: &str| {
                Message::NodeSettingChanged {
                    node_id: node_raw_id,
                    key: key.to_string(),
                    value: field_edit(&rows, index, Some((name, chosen))),
                }
            })
            .placeholder("type")
            .text_size(12)
            .padding(2.0)
        };
        // The row's one destructive control, and the one hardest to hit: at
        // 2 px of padding it was about 10 x 14 physical pixels with the type
        // list right against it, so a click a pixel too far left opened the
        // list instead of removing the field. Padding plus the row spacing
        // below keep the two apart.
        //
        // The last row keeps its button disabled: a table with no columns is
        // not a table, and removing it only trades the row for a red `no
        // fields: give the table at least one` that nothing but adding a
        // field back clears. A field is renamed or retyped in place, so the
        // click has nothing to offer.
        let remove = button(text("x").size(11))
            .padding([3.0, 6.0])
            .on_press_maybe((rows.len() > 1).then(|| Message::NodeSettingChanged {
                node_id: node_raw_id,
                key: def.name.to_string(),
                value: field_edit(&rows, index, None),
            }));

        // The pin the field declares, for its color and shape. A half-typed
        // row has none yet, and drawing it anyway is what keeps the widget
        // tree stable while the user types.
        let pin = node
            .pin_defs
            .iter()
            .enumerate()
            .find(|(_, p)| &*p.name == name);
        let tint = pin.map_or_else(|| pin_color(&Ty::Any), |(_, p)| pin_color(&p.ty));
        let visual = PinVisual {
            color: if pin.is_some_and(|(index, _)| is_dim(dim_mask, index)) {
                dim(tint)
            } else {
                tint
            },
            shape: pin_shape(pin.map_or(PinKind::Sample, |(_, p)| p.pin_kind)),
        };

        items.push(
            node_pin(
                PinSide::Row,
                PinLabel(Arc::from(name)),
                row![rename, retype, remove].spacing(4),
            )
            .direction(NgPinDirection::Both)
            .info(visual)
            .into(),
        );
    }

    items.push(
        button(text("add field").size(11))
            .padding(2.0)
            .on_press(Message::NodeSettingChanged {
                node_id: node_raw_id,
                key: def.name.to_string(),
                value: field_added(&rows, &options),
            })
            .into(),
    );
    items
}

/// The value these rows mean with row `index` replaced by `row`, or removed
/// when `row` is `None`.
fn field_edit(rows: &[(&str, &str)], index: usize, row: Option<(&str, &str)>) -> String {
    let mut out = rows.to_vec();
    match row {
        Some(row) if index < out.len() => out[index] = row,
        Some(row) => out.push(row),
        None if index < out.len() => {
            out.remove(index);
        }
        None => {}
    }
    out.iter()
        .map(|(name, ty)| format!("{name}:{ty}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The value these rows mean with one more row: the first offered type and a
/// name no existing row has, so adding two fields in a row does not produce
/// two pins with one id.
fn field_added(rows: &[(&str, &str)], types: &[&str]) -> String {
    let ty = types.first().copied().unwrap_or_default();
    let mut name = "field".to_string();
    let mut nth = 2;
    while rows.iter().any(|(taken, _)| *taken == name) {
        name = format!("field{nth}");
        nth += 1;
    }
    field_edit(rows, rows.len(), Some((&name, ty)))
}

/// Header background color per node category, mirroring the old content presets.
fn header_color(type_id: &str) -> Color {
    if type_id.starts_with("transform.const_") {
        Color::from_rgb(0.16, 0.30, 0.20) // input: green
    } else if type_id == "transform.display" {
        Color::from_rgb(0.32, 0.26, 0.10) // output: gold
    } else {
        Color::from_rgb(0.18, 0.20, 0.26) // process: neutral
    }
}

/// Per-pin visual data carried as the node graph's pin info. Two orthogonal
/// channels: `color` encodes the payload type, `shape` encodes the transmission
/// mode (Event vs State).
///
/// Public because it is `GraphIds::Payload`, and that vocabulary is named in
/// `Message`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PinVisual {
    color: Color,
    shape: PinShape,
}

/// Pin shape encodes the transmission mode: Event (Trigger) pins are squares,
/// State (Sample) pins are circles. `PinShape` offers exactly these two.
fn pin_shape(kind: PinKind) -> PinShape {
    match kind {
        PinKind::Trigger => PinShape::Square,
        PinKind::Sample => PinShape::Circle,
    }
}

/// Pin color per payload type. Scalars get a distinct hue; opaque and composite
/// types share the neutral fallback rather than a generated palette.
fn pin_color(ty: &Ty) -> Color {
    match ty {
        Ty::Float => Color::from_rgb(0.3, 0.8, 0.4),
        Ty::Str => Color::from_rgb(0.9, 0.7, 0.2),
        Ty::Bool => Color::from_rgb(0.3, 0.5, 0.9),
        Ty::Any => Color::from_rgb(0.7, 0.7, 0.7),
        Ty::Int | Ty::List(_) | Ty::Option(_) | Ty::Record(_) | Ty::Opaque(_) => {
            Color::from_rgb(0.6, 0.6, 0.6)
        }
    }
}

/// Horizontal distance between two ranks. One node is 180 wide (240 for a
/// Display), so this leaves a cable's worth of room between columns.
const LAYOUT_COLUMN: f32 = 320.0;

/// Vertical distance between two nodes of the same rank.
const LAYOUT_ROW: f32 = 160.0;

/// Where the first node goes; the same margin on both axes.
const LAYOUT_MARGIN: f32 = 40.0;

/// Positions for one graph: a column per depth, a row per node in it.
///
/// Free function taking ids and edges so the arithmetic can be tested without
/// an editor, and so the caller decides what "one graph" means -- the editor
/// passes the current graph's nodes and its *mapped* view edges, which is what
/// makes a subgraph lay out by the wires the user can see.
///
/// Rank is the longest path from a source, so a node sits to the right of
/// everything that feeds it. Within a rank, nodes are ordered by the mean
/// height of their already-placed predecessors (the barycenter, which is what
/// keeps cables from crossing), ties by id so the result is stable. A cycle has
/// no source and no longest path: the lowest remaining id is admitted as if its
/// incoming edges were not there, which is exactly "ignore the back edge", and
/// guarantees termination because every step places one node.
fn auto_layout(nodes: &[NodeId], edges: &[(NodeId, NodeId)]) -> Vec<(NodeId, Point)> {
    use std::collections::BTreeSet;

    let present: HashSet<NodeId> = nodes.iter().copied().collect();
    let wires: Vec<(NodeId, NodeId)> = edges
        .iter()
        .copied()
        .filter(|(from, to)| from != to && present.contains(from) && present.contains(to))
        .collect();

    let mut incoming: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    let mut outgoing: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for (from, to) in &wires {
        outgoing.entry(*from).or_default().push(*to);
        incoming.entry(*to).or_default().push(*from);
    }
    let mut pending: HashMap<NodeId, usize> = nodes
        .iter()
        .map(|id| (*id, incoming.get(id).map_or(0, Vec::len)))
        .collect();

    let mut ready: BTreeSet<u64> = pending
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(id, _)| id.0)
        .collect();
    let mut rank: HashMap<NodeId, usize> = nodes.iter().map(|id| (*id, 0)).collect();
    let mut placed: HashSet<NodeId> = HashSet::with_capacity(nodes.len());
    let mut order: Vec<NodeId> = Vec::with_capacity(nodes.len());

    while order.len() < nodes.len() {
        let next = match ready.iter().next().copied() {
            Some(next) => next,
            // Everything left is in a cycle. Admitting the lowest id ignores
            // its back edges instead of looping forever.
            None => nodes
                .iter()
                .filter(|id| !placed.contains(id))
                .map(|id| id.0)
                .min()
                .expect("nodes remain while the order is short"),
        };
        ready.remove(&next);
        let node = NodeId(next);
        if !placed.insert(node) {
            continue;
        }
        order.push(node);
        let depth = rank[&node];
        for target in outgoing.get(&node).into_iter().flatten() {
            if placed.contains(target) {
                continue;
            }
            let known = rank.entry(*target).or_default();
            *known = (*known).max(depth + 1);
            if let Some(count) = pending.get_mut(target) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    ready.insert(target.0);
                }
            }
        }
    }

    let mut columns: Vec<Vec<NodeId>> = Vec::new();
    for node in &order {
        let depth = rank[node];
        if columns.len() <= depth {
            columns.resize(depth + 1, Vec::new());
        }
        columns[depth].push(*node);
    }

    let mut positions: HashMap<NodeId, Point> = HashMap::with_capacity(nodes.len());
    for (depth, column) in columns.iter().enumerate() {
        let mut sorted: Vec<(Option<f32>, u64, NodeId)> = column
            .iter()
            .map(|node| {
                let heights: Vec<f32> = incoming
                    .get(node)
                    .into_iter()
                    .flatten()
                    .filter_map(|source| positions.get(source).map(|p| p.y))
                    .collect();
                let barycenter = if heights.is_empty() {
                    None
                } else {
                    Some(heights.iter().sum::<f32>() / heights.len() as f32)
                };
                (barycenter, node.0, *node)
            })
            .collect();
        // A node with nothing placed above it has no barycenter and goes last,
        // where it cannot push a wired node out of line.
        sorted.sort_by(|a, b| match (a.0, b.0) {
            (Some(left), Some(right)) => left
                .partial_cmp(&right)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.1.cmp(&b.1),
        });
        for (row, (_, _, node)) in sorted.into_iter().enumerate() {
            positions.insert(
                node,
                Point::new(
                    LAYOUT_MARGIN + depth as f32 * LAYOUT_COLUMN,
                    LAYOUT_MARGIN + row as f32 * LAYOUT_ROW,
                ),
            );
        }
    }

    let mut out: Vec<(NodeId, Point)> = positions.into_iter().collect();
    out.sort_by_key(|(node, _)| node.0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(positions: &[(NodeId, Point)], node: u64) -> Point {
        positions
            .iter()
            .find(|(id, _)| id.0 == node)
            .map(|(_, p)| *p)
            .unwrap_or_else(|| panic!("node {node} was not placed"))
    }

    /// The layout has to say something for every node, put each one to the
    /// right of what feeds it, and never place two in the same spot -- and a
    /// cycle has to come out the other side rather than hang.
    #[test]
    fn a_diamond_and_a_cycle_land_in_columns_without_overlap() {
        // 1 -> 2 -> 4, 1 -> 3 -> 4 (a diamond), plus 5 <-> 6 (a cycle) hanging
        // off 4.
        let nodes: Vec<NodeId> = (1..=6).map(NodeId).collect();
        let edges: Vec<(NodeId, NodeId)> = vec![
            (NodeId(1), NodeId(2)),
            (NodeId(1), NodeId(3)),
            (NodeId(2), NodeId(4)),
            (NodeId(3), NodeId(4)),
            (NodeId(4), NodeId(5)),
            (NodeId(5), NodeId(6)),
            (NodeId(6), NodeId(5)),
        ];

        let positions = auto_layout(&nodes, &edges);
        assert_eq!(positions.len(), nodes.len());

        // Columns by depth: the diamond's sides share one, its tip is past
        // both, and the longest path decides -- not the first path found.
        assert_eq!(at(&positions, 1).x, 40.0);
        assert_eq!(at(&positions, 2).x, 40.0 + 320.0);
        assert_eq!(at(&positions, 3).x, 40.0 + 320.0);
        assert_eq!(at(&positions, 4).x, 40.0 + 2.0 * 320.0);
        assert_eq!(at(&positions, 5).x, 40.0 + 3.0 * 320.0);
        assert_eq!(at(&positions, 6).x, 40.0 + 4.0 * 320.0);

        // The two sides of the diamond share a column, so they must not share
        // a row.
        assert_ne!(at(&positions, 2).y, at(&positions, 3).y);
        assert_eq!(at(&positions, 2).y, 40.0);
        assert_eq!(at(&positions, 3).y, 40.0 + 160.0);

        // Nothing lands on top of anything else.
        let mut spots: Vec<(u32, u32)> = positions
            .iter()
            .map(|(_, p)| (p.x.to_bits(), p.y.to_bits()))
            .collect();
        spots.sort_unstable();
        let unique = spots.len();
        spots.dedup();
        assert_eq!(spots.len(), unique, "two nodes were placed in one spot");
    }

    /// A graph with nothing but a cycle still lays out: every node is placed
    /// once, in a column, and the call returns.
    #[test]
    fn a_graph_that_is_only_a_cycle_still_terminates() {
        let nodes: Vec<NodeId> = (1..=3).map(NodeId).collect();
        let edges = vec![
            (NodeId(1), NodeId(2)),
            (NodeId(2), NodeId(3)),
            (NodeId(3), NodeId(1)),
        ];
        let positions = auto_layout(&nodes, &edges);
        assert_eq!(positions.len(), 3);
        assert_eq!(at(&positions, 1).x, 40.0);
        assert_eq!(at(&positions, 2).x, 40.0 + 320.0);
        assert_eq!(at(&positions, 3).x, 40.0 + 2.0 * 320.0);
    }

    /// An edge naming a node of another graph is not this graph's business:
    /// the editor passes mapped view edges, and anything unmapped is dropped.
    #[test]
    fn edges_to_nodes_outside_the_graph_are_ignored() {
        let nodes = vec![NodeId(1), NodeId(2)];
        let edges = vec![
            (NodeId(9), NodeId(1)),
            (NodeId(1), NodeId(2)),
            (NodeId(2), NodeId(2)),
        ];
        let positions = auto_layout(&nodes, &edges);
        assert_eq!(at(&positions, 1), Point::new(40.0, 40.0));
        assert_eq!(at(&positions, 2), Point::new(360.0, 40.0));
    }

    /// The row editor's whole contract: every edit is the setting's full text
    /// again, so the store, the runner and the node's parser keep speaking
    /// `name:type` and never learn there were widgets.
    #[test]
    fn editing_a_field_row_renders_the_whole_setting_again() {
        let text = "id:int\ncustomer_id:int\ntotal:float";
        let rows = field_rows(text);
        assert_eq!(
            rows,
            vec![("id", "int"), ("customer_id", "int"), ("total", "float")]
        );

        // Rename, retype, remove -- and appending past the end adds a row.
        assert_eq!(
            field_edit(&rows, 1, Some(("cust", "int"))),
            "id:int\ncust:int\ntotal:float"
        );
        assert_eq!(
            field_edit(&rows, 2, Some(("total", "str"))),
            "id:int\ncustomer_id:int\ntotal:str"
        );
        assert_eq!(field_edit(&rows, 1, None), "id:int\ntotal:float");
        assert_eq!(
            field_edit(&rows, 9, Some(("note", "str"))),
            "id:int\ncustomer_id:int\ntotal:float\nnote:str"
        );
    }

    /// A half-typed row is still a row: the field whose type the user has not
    /// chosen yet must not disappear from under the cursor.
    #[test]
    fn a_field_without_a_type_survives_as_a_row() {
        assert_eq!(
            field_rows("id:int\nname\n\n"),
            vec![("id", "int"), ("name", "")]
        );
    }

    /// Two added fields must not end up with one name: two pins with one id is
    /// a relation that could land on either.
    #[test]
    fn added_fields_get_names_no_row_already_has() {
        let types = ["int", "str"];
        assert_eq!(field_added(&field_rows(""), &types), "field:int");
        assert_eq!(
            field_added(&field_rows("field:int"), &types),
            "field:int\nfield2:int"
        );
        assert_eq!(
            field_added(&field_rows("field:int\nfield2:str"), &types),
            "field:int\nfield2:str\nfield3:int"
        );
    }

    /// Which end of a relation is referenced is a property of the fields, not
    /// of the drag: dragging `customer_id` onto `id` and dragging `id` onto
    /// `customer_id` must declare the same foreign key.
    #[test]
    fn the_id_end_of_a_relation_is_the_referenced_one() {
        assert!(relation_references_to("customer_id", "id"));
        assert!(!relation_references_to("id", "customer_id"));
        // Neither is a key: the end the wire was dropped on.
        assert!(relation_references_to("owner", "seq"));
        // Both are: the only tie left to break is the drop target.
        assert!(relation_references_to("id", "id"));
    }

    /// Reshaping a node's pins may only drop the relations whose field is
    /// actually gone. A wire on a pin that still exists stays -- deleting it
    /// would remove a live edge from the shared store, where nothing brings it
    /// back.
    #[test]
    fn only_a_relation_whose_field_is_gone_is_dropped() {
        let field = PinDefinition::field("customer_id", Ty::opaque("db.field"));
        let output = PinDefinition::output("table", Ty::opaque("db.table"));
        let input = PinDefinition::input("table", Ty::opaque("db.table"), PinKind::Sample);

        // The field this end sat on is gone, the other end is still a field:
        // a relation with nothing left to attach to.
        assert!(is_lost_relation(None, Some(&field)));

        // This end is still a field pin: the relation is intact.
        assert!(!is_lost_relation(Some(&field), Some(&field)));

        // This end is an ordinary output or input that the node still
        // declares. Not a relation of ours, and above all not ours to delete
        // -- this is the case that used to take live wires with it.
        assert!(!is_lost_relation(Some(&output), Some(&field)));
        assert!(!is_lost_relation(Some(&input), Some(&field)));

        // Nothing to do with relations at all: a dataflow wire whose pin
        // vanished for a moment while a setting was half-typed must survive.
        assert!(!is_lost_relation(None, Some(&output)));
        assert!(!is_lost_relation(None, None));
    }

    /// The breadcrumb walk has to terminate on a `parent` relation the editor
    /// did not build. `create_node` in the module takes an arbitrary parent,
    /// so one row naming itself froze `view` in an endless walk.
    #[test]
    fn the_ancestry_walk_ends_on_a_parent_cycle() {
        // 3 inside 2 inside 1 inside the root.
        let tree = |id: NodeId| match id.0 {
            3 => Some(NodeId(2)),
            2 => Some(NodeId(1)),
            1 => Some(NodeId(0)),
            _ => None,
        };
        assert_eq!(
            ancestry(NodeId(3), tree),
            vec![NodeId(1), NodeId(2), NodeId(3)]
        );
        assert!(ancestry(NodeId(0), tree).is_empty());

        // A node that is its own parent, and a pair that are each other's.
        let itself = |id: NodeId| Some(id);
        assert_eq!(ancestry(NodeId(7), itself), vec![NodeId(7)]);
        let pair = |id: NodeId| Some(NodeId(if id.0 == 4 { 5 } else { 4 }));
        assert_eq!(ancestry(NodeId(4), pair), vec![NodeId(5), NodeId(4)]);

        // A parent this window does not have stops the walk rather than
        // dropping the part of the trail that is known.
        assert!(ancestry(NodeId(9), |_| None).is_empty());
    }

    /// A wire is refused exactly when its target already feeds its source. A
    /// relation is not a path: two tables referencing each other is a legal
    /// schema, and the runtime keeps such an edge out of execution anyway.
    #[test]
    fn a_wire_closes_a_cycle_only_along_dataflow_edges() {
        let node = |flow_out: Vec<u64>| NodeEdges {
            outgoing: Vec::new(),
            incoming: Vec::new(),
            flow_out: flow_out.into_iter().map(NodeId).collect(),
        };
        // 1 -> 2 -> 3, and 4 alone.
        let index: HashMap<NodeId, NodeEdges> = HashMap::from([
            (NodeId(1), node(vec![2])),
            (NodeId(2), node(vec![3])),
            (NodeId(3), node(vec![])),
            (NodeId(4), node(vec![])),
        ]);

        // A wire from 3 back to 1 would close the chain: 1 reaches 3.
        assert!(flow_reaches(&index, NodeId(1), NodeId(3)));
        assert!(flow_reaches(&index, NodeId(1), NodeId(2)));
        // The other direction is the wire the user is allowed to draw.
        assert!(!flow_reaches(&index, NodeId(3), NodeId(1)));
        // Unrelated components, and a node nothing in the index mentions.
        assert!(!flow_reaches(&index, NodeId(4), NodeId(1)));
        assert!(!flow_reaches(&index, NodeId(9), NodeId(1)));

        // A graph that already has a cycle must not hang the walk -- one can
        // arrive from the store, which the executor now tolerates.
        let looped: HashMap<NodeId, NodeEdges> =
            HashMap::from([(NodeId(1), node(vec![2])), (NodeId(2), node(vec![1]))]);
        assert!(flow_reaches(&looped, NodeId(1), NodeId(2)));
        assert!(!flow_reaches(&looped, NodeId(1), NodeId(7)));

        // Relations are absent from `flow_out`, so a pair of tables wired to
        // each other is not a path at all.
        let relations: HashMap<NodeId, NodeEdges> = HashMap::from([
            (
                NodeId(1),
                NodeEdges {
                    outgoing: vec![(EdgeId(1), NodeId(2))],
                    incoming: vec![(EdgeId(2), NodeId(2))],
                    flow_out: Vec::new(),
                },
            ),
            (NodeId(2), node(vec![])),
        ]);
        assert!(!flow_reaches(&relations, NodeId(1), NodeId(2)));
    }
}
