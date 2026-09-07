use std::collections::{HashMap, HashSet};
#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;
use std::sync::Arc;

use iced::keyboard;
use iced::widget::{button, column, container, image, row, stack, text, text_input};
use iced::{Color, ContentFit, Element, Event, Length, Point, Subscription, Task, Theme};
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
use zeughaus_core::{
    DomainPlugin, EdgeData, EdgeId, EdgeSemantic, GraphDocument, Image, NodeConfig, NodeData,
    NodeDefinition, NodeId, PinDefinition, PinDirection, PinKind, SettingDef, Ty, TypeConverters,
    Value,
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
}

pub struct EditorEdge {
    pub id: EdgeId,
    pub from_node: NodeId,
    pub from_pin: PinLabel,
    pub to_node: NodeId,
    pub to_pin: PinLabel,
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

    // Live SpacetimeDB connection, always established on native startup. Held to
    // keep the background message loop alive and to call reducers on local edits.
    #[cfg(not(target_arch = "wasm32"))]
    stdb: Option<crate::module_bindings::DbConnection>,
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
    // When each in-flight particle was born, per edge. One per delivered value,
    // which is why an unchanged value still animates: traffic, not state.
    #[cfg(not(target_arch = "wasm32"))]
    particles: HashMap<EdgeId, std::collections::VecDeque<iced::time::Instant>>,
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
        // session. SpacetimeDB is required: connect on startup with no
        // local-only fallback, failing loudly if the server is absent. The
        // session token is what the palette "Copy Session ID" shares so a buddy
        // can join.
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
            let (conn, rx) =
                crate::sync::connect(&uri, &db, crate::sync::Role::Viewer).unwrap_or_else(|e| {
                    panic!(
                        "[stdb] cannot connect to {uri} / {db}: {e} (start it with `spacetime start`)"
                    )
                });
            eprintln!("[stdb] session token: {token}");
            (Some(conn), Some(rx), Some(token))
        };

        let mut app = Self {
            nodes: HashMap::new(),
            node_order: Vec::new(),
            edges: Vec::new(),
            selected: HashSet::new(),
            camera_position: Point::ORIGIN,
            camera_zoom: 1.0,
            current_graph: NodeId(0),
            cameras: HashMap::new(),
            executor,
            plugins,
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
            particles: HashMap::new(),
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
    fn descendants(&self, parent: NodeId) -> Vec<NodeId> {
        let mut found = Vec::new();
        let mut stack = vec![parent];
        while let Some(current) = stack.pop() {
            for child in self.children(current) {
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
        if !self.is_container(&node.type_id) {
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
        let type_id = &self.nodes.get(&node)?.type_id;
        if !self.is_container(type_id) {
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
        let exec = self.plugins.iter().find_map(|p| p.create_node(type_id));
        let Some(exec) = exec else {
            return;
        };

        // Stagger each new node so they don't pile up
        let offset = (self.spawn_counter % 10) as f32 * 30.0;
        self.spawn_counter += 1;
        let position = Point::new(position.x + offset, position.y + offset);

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
        // execution state matches what the widget shows.
        if !setting_defs.is_empty() {
            let mut values = HashMap::new();
            for def in &setting_defs {
                let _ =
                    self.executor
                        .set_parameter(id, &def.name, Value::new(def.default.to_string()));
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
                parent: self.current_graph,
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
        self.refresh_container_pins(self.current_graph);
        #[cfg(not(target_arch = "wasm32"))]
        self.derive_db_params(id);
        #[cfg(not(target_arch = "wasm32"))]
        self.push_node(id);
        self.autosave();
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
        // Seed the new wire from the source's last published output so it shows
        // a value immediately instead of staying blank until the runtime's next
        // publish. Nothing is executed here: this process does not run nodes.
        self.executor.on_edge_added(edge_id);
        // Grow a variadic target (e.g. merge node) so the next empty input shows.
        self.resync_pins(to_node);
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(e) = self.edges.last() {
            self.push_edge(e);
        }
        // A table wired into a `table` pin is the column list the target works
        // from.
        #[cfg(not(target_arch = "wasm32"))]
        self.derive_db_params(to_node);
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
            self.executor.disconnect_edge(edge.id);
            self.resync_pins(to_node);
            // The wire the particles were riding is gone; without this every
            // local rewiring leaks a queue nothing will ever draw again.
            #[cfg(not(target_arch = "wasm32"))]
            self.particles.remove(&edge.id);
            #[cfg(not(target_arch = "wasm32"))]
            self.push_edge_remove(edge.id);
            // A table pin that lost its wire is a column list that no longer
            // applies.
            #[cfg(not(target_arch = "wasm32"))]
            self.derive_db_params(to_node);
            self.update_display_values();
            self.autosave();
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
                self.remote_outputs.entry(node).or_default().insert(pin, value);
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
            Traffic::Lost => self.traffic_live = false,
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
        self.update_display_values();
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
            .stdb
            .as_ref()
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
        let mut errors = self.executor.errors();
        let Some((id, message)) = errors.next() else {
            return String::new();
        };
        let name = self
            .nodes
            .get(&id)
            .map_or("node", |n| n.display_name.as_str());
        match errors.count() {
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

    /// Recomputes what each node shows inline from the values on its edges, and
    /// from the video feeds for the nodes that have one.
    ///
    /// A node shows what it produced; a sink (no outgoing edges) shows what it
    /// received, which is what makes the Display node work. Image frames reuse
    /// the previous `image::Handle` when the pixels are literally the same
    /// buffer, so an unchanged frame is not re-uploaded to the GPU.
    fn update_display_values(&mut self) {
        let mut next: HashMap<NodeId, DisplayValue> = HashMap::new();
        for &node_id in self.nodes.keys() {
            // A frame from a feed outranks the edge cache, because for an image
            // pin the edge cache is empty by design: pixels never travel through
            // the store, so the feed is the only thing that has the frame.
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(frame) = self.feed_frame(node_id) {
                next.insert(node_id, self.frame_display(node_id, frame));
                continue;
            }
            let value = self
                .edges
                .iter()
                .filter(|e| e.from_node == node_id)
                .find_map(|e| self.executor.edge_value(e.id))
                .or_else(|| {
                    self.edges
                        .iter()
                        .filter(|e| e.to_node == node_id)
                        .find_map(|e| self.executor.edge_value(e.id))
                });
            let Some(value) = value else { continue };
            let display = match value.downcast_ref::<Image>() {
                Some(frame) => self.frame_display(node_id, frame),
                None => DisplayValue::Text(value.to_string()),
            };
            next.insert(node_id, display);
        }
        self.display_values = next;
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
            },
        );
        self.node_order.push(id);
    }

    fn load_document(&mut self, doc: GraphDocument) {
        // Clear current state
        self.nodes.clear();
        self.node_order.clear();
        self.edges.clear();
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
            .filter(|id| {
                self.nodes
                    .get(id)
                    .is_some_and(|node| self.is_container(&node.type_id))
            })
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
        let commands = palette::build_commands(&self.catalog);
        let original_idx =
            get_filtered_command_index(&self.palette_input, &commands, self.palette_selected)?;

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
                        .is_some_and(|node| self.is_container(&node.type_id));
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
                let positions: Vec<_> = ids
                    .iter()
                    .filter_map(|raw_id| {
                        let node = self.nodes.get(&NodeId(*raw_id))?;
                        Some((node.type_id.clone(), node.position))
                    })
                    .collect();
                for (type_id, pos) in positions {
                    self.spawn_node(&type_id, Point::new(pos.x + 30.0, pos.y + 30.0));
                }
            }
            Message::DeleteNodes(ids) => {
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
                self.node_settings
                    .entry(id)
                    .or_default()
                    .insert(key.clone(), value.clone());
                let _ = self.executor.set_parameter(id, &key, Value::new(value));
                // A setting can decide the node's pins (a table's columns are
                // one), so the widget re-reads what it now declares.
                self.refresh_node_pins(id);
                // Renaming a boundary renames its container's pin.
                if key == "name" {
                    self.refresh_boundary_owner(id);
                }
                // A database's path reaches its children, a table's columns
                // reach everything it feeds.
                #[cfg(not(target_arch = "wasm32"))]
                self.derive_db_dependents(id);
                #[cfg(not(target_arch = "wasm32"))]
                self.push_params(id);
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
                    self.drain_sync();
                    // Also where the runtime endpoint is noticed. A runtime
                    // announces its address by updating its own `runtime` row,
                    // and the sync layer reports inserts and deletes of that
                    // table but not updates -- so there is no event to wait for,
                    // and the client cache is read instead.
                    return self.reconcile_runtime();
                }
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
                // output and one input, distinct nodes, compatible types.
                let nodes = &self.nodes;
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
                    let (out_pin, in_pin) = if fp.direction == PinDirection::Output {
                        (fp, tp)
                    } else {
                        (tp, fp)
                    };
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
                let display = self.display_values.get(id);
                let const_input = self.const_inputs.get(id).map(|s| s.as_str());
                let settings = self.node_settings.get(id);
                let content = build_node_element(
                    node,
                    display,
                    const_input,
                    settings,
                    self.dim_mask(*id, node),
                    self.node_sizes.get(id).copied(),
                    self.is_container(&node.type_id),
                );
                // Per-node activity feedback: red marching-ants on error. There
                // is no "working" state to draw -- this process does not
                // execute, so a node is never mid-run here.
                let errored = self.executor.is_error(*id);
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
            let src_error = self.executor.is_error(edge.from_node);

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
            let edge_widget = edge_widget.particles(
                self.particles
                    .get(&edge.id)
                    .into_iter()
                    .flatten()
                    .map(move |born| {
                        particle(*born, PARTICLE_SPEED).style(move |theme| ParticleStyle {
                            color: edge_color,
                            ..default_particle_style(theme)
                        })
                    }),
            );
            ng = ng.push_edge(edge_widget);
        }

        let graph_area: Element<'_, Message> = container(ng)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();

        let graph_view = if self.palette_open {
            let commands = palette::build_commands(&self.catalog);
            let palette_view = palette::view(&self.palette_input, &commands, self.palette_selected);
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
        let mut trail = Vec::new();
        let mut current = self.current_graph;
        while current != NodeId(0) {
            let Some(node) = self.nodes.get(&current) else {
                break;
            };
            trail.push((current, node.display_name.clone()));
            current = node.parent;
        }
        trail.reverse();

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

        // The library only auto-redraws for animated edges, not node borders.
        // While any node is in error, drive ~30fps redraws so its marching-ants
        // border animates. Native only: iced::time::every needs the tokio
        // executor feature.
        #[cfg(not(target_arch = "wasm32"))]
        if self.executor.errors().next().is_some() {
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

    fn push_node(&self, id: NodeId) {
        if self.applying_remote {
            return;
        }
        if let (Some(conn), Some(nd)) = (&self.stdb, self.node_data(id)) {
            crate::sync::send_create_node(conn, &nd);
        }
    }

    fn push_params(&self, id: NodeId) {
        if self.applying_remote {
            return;
        }
        if let (Some(conn), Some(nd)) = (&self.stdb, self.node_data(id)) {
            crate::sync::send_set_params(conn, id.0, &nd.params);
        }
    }

    fn push_move(&self, id: NodeId, x: f32, y: f32) {
        if self.applying_remote {
            return;
        }
        if let Some(conn) = &self.stdb {
            crate::sync::send_move_node(conn, id.0, x, y);
        }
    }

    fn push_delete(&self, id: NodeId) {
        if self.applying_remote {
            return;
        }
        if let Some(conn) = &self.stdb {
            crate::sync::send_delete_node(conn, id.0);
        }
    }

    fn push_edge(&self, e: &EditorEdge) {
        if self.applying_remote {
            return;
        }
        if let Some(conn) = &self.stdb {
            crate::sync::send_connect_edge(
                conn,
                &EdgeData {
                    id: e.id.0,
                    from_node: e.from_node.0,
                    from_pin: e.from_pin.to_string(),
                    to_node: e.to_node.0,
                    to_pin: e.to_pin.to_string(),
                },
            );
        }
    }

    fn push_edge_remove(&self, id: EdgeId) {
        if self.applying_remote {
            return;
        }
        if let Some(conn) = &self.stdb {
            crate::sync::send_disconnect_edge(conn, id.0);
        }
    }

    // Receive: drain queued remote events and apply them to the editor.

    /// Removes every edge feeding the given input pin, from both the editor
    /// state and the executor graph/cache. Only the remote-apply path needs it:
    /// a local connect cannot land on an occupied input (`can_connect` rejects
    /// it), but a remote one can race one in.
    fn remove_edges_into(&mut self, to_node: NodeId, to_pin: &PinLabel) {
        let stale: Vec<EdgeId> = self
            .edges
            .iter()
            .filter(|e| e.to_node == to_node && e.to_pin == *to_pin)
            .map(|e| e.id)
            .collect();
        for edge_id in stale {
            self.edges.retain(|e| e.id != edge_id);
            self.executor.disconnect_edge(edge_id);
        }
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
        }
    }

    /// Re-reads whether anything is executing the graph.
    ///
    /// The editor is not a candidate -- it registers as `Role::Viewer` and never
    /// appears in the runtime table -- so this is purely informational. It is
    /// still the difference between a live number and a stale one, which is the
    /// one thing a user must not have to guess about.
    fn apply_runtimes_changed(&mut self) {
        if let Some(conn) = &self.stdb {
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

    /// Fills in the parameters a database node cannot know by itself: which
    /// file it works on, and the columns of the table it is wired to.
    ///
    /// Derived rather than typed twice. A node inside a `db.database` works on
    /// that database, and an insert wired to a table has that table's columns
    /// -- restating either by hand is a chance for the two to disagree. They
    /// are ordinary parameters from the runner's point of view, so nothing on
    /// that side has to know they were derived.
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
    /// one: the children of a database, and everything a table feeds.
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
                .filter(|e| e.from_node == node && e.to_pin.as_str() == "table")
                .map(|e| e.to_node)
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
        self.executor.remove_node(id);
        self.const_inputs.remove(&id);
        self.node_settings.remove(&id);
        // A value whose producer is gone is not a value any more, and node ids
        // are never reused, so nothing can inherit it.
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.remote_outputs.remove(&id);
            self.output_seq.retain(|(node, _), _| *node != id);
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
        self.remove_edges_into(to_node, &PinLabel(to_pin.clone()));
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
        self.executor.on_edge_added(edge_id);
        self.resync_pins(to_node);
    }

    fn apply_edge_remove(&mut self, id: EdgeId) {
        self.pending_edges.retain(|ed| ed.id != id.0);
        if let Some(pos) = self.edges.iter().position(|e| e.id == id) {
            let edge = self.edges.remove(pos);
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

/// Default content size of a Display node before the user resizes it. Wide
/// enough that a downscaled 16:9 frame is recognizable.
const DISPLAY_SIZE: iced::Size = iced::Size::new(240.0, 150.0);

/// Width of every other node. Their content is text and pin rows, which do not
/// benefit from being resizable.
const NODE_WIDTH: f32 = 180.0;

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

fn build_node_element<'a>(
    node: &'a EditorNode,
    display: Option<&'a DisplayValue>,
    const_input: Option<&'a str>,
    settings: Option<&'a HashMap<String, String>>,
    dim_mask: u64,
    size: Option<iced::Size>,
    is_container: bool,
) -> Element<'a, Message, Theme> {
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
        for (index, pin_def) in node.pin_defs.iter().enumerate() {
            let side = match pin_def.direction {
                PinDirection::Input => PinSide::Left,
                PinDirection::Output => PinSide::Right,
            };
            let direction = match pin_def.direction {
                PinDirection::Input => NgPinDirection::Input,
                PinDirection::Output => NgPinDirection::Output,
            };

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
    // settings is fixed per node type, so the widget tree stays stable.
    for def in &node.settings {
        let node_raw_id = node.id.0;
        let key = def.name.clone();
        let current = settings
            .and_then(|m| m.get(&*def.name))
            .map(|s| s.as_str())
            .unwrap_or(&def.default);

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
                field
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
    } else {
        container(inner).width(NODE_WIDTH).into()
    }
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
