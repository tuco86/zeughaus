use std::collections::{HashMap, HashSet};
#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;
use std::sync::Arc;

use iced::keyboard;
use iced::widget::{column, container, row, stack, text, text_input};
use iced::{Color, Element, Event, Length, Point, Subscription, Task, Theme};
use iced_nodegraph::{
    EdgeStyle, NodeGraph, NodeStatus, NodeStyle, Pattern, PinDirection as NgPinDirection, PinRef,
    PinInfo, PinSide, PinStyle, default_edge_style, default_node_style, default_pin_style,
    edge as ng_edge, node as ng_node, node_header, node_pin,
};
use iced_palette::{get_filtered_command_index, is_toggle_shortcut};
use zeughaus_core::{
    DomainPlugin, EdgeData, EdgeId, EdgeSemantic, GraphDocument, NodeConfig, NodeData,
    NodeDefinition, NodeId, PinDefinition, PinDirection, SettingDef, TypeConverters, Value,
};
#[cfg(not(target_arch = "wasm32"))]
use zeughaus_runtime::DeferredWork;
use zeughaus_runtime::{Graph, GraphEdge, GraphExecutor, GraphNode};
#[cfg(not(target_arch = "wasm32"))]
use zeughaus_capture::CapturePlugin;
#[cfg(not(target_arch = "wasm32"))]
use zeughaus_llm::LlmPlugin;
#[cfg(not(target_arch = "wasm32"))]
use zeughaus_process::ProcessPlugin;
use zeughaus_ml::MlPlugin;
use zeughaus_transform::TransformPlugin;

use crate::message::{Message, PinLabel};
use crate::palette;

pub struct EditorNode {
    pub id: NodeId,
    pub type_id: String,
    pub display_name: String,
    pub position: Point,
    pub pin_defs: Vec<PinDefinition>,
    pub settings: Vec<SettingDef>,
}

pub struct EditorEdge {
    pub id: EdgeId,
    pub from_node: NodeId,
    pub from_pin: PinLabel,
    pub to_node: NodeId,
    pub to_pin: PinLabel,
}

pub struct App {
    // Editor state
    nodes: HashMap<NodeId, EditorNode>,
    node_order: Vec<NodeId>,
    edges: Vec<EditorEdge>,
    selected: HashSet<NodeId>,
    camera_position: Point,
    camera_zoom: f32,

    // Runtime
    executor: GraphExecutor,
    plugins: Vec<Box<dyn DomainPlugin>>,
    catalog: Vec<NodeDefinition>,
    /// Type converters built from the plugins. Shared with the executor; used
    /// here for connection validation so the editor and runtime agree on which
    /// type pairs may connect.
    converters: Arc<TypeConverters>,

    // Display values (node_id -> display string)
    display_values: HashMap<NodeId, String>,

    // Const node text inputs (node_id -> current text)
    const_inputs: HashMap<NodeId, String>,

    // In-node text settings (node_id -> setting name -> current value)
    node_settings: HashMap<NodeId, HashMap<String, String>>,

    // Spawn offset counter (staggers new nodes so they don't overlap)
    spawn_counter: u32,

    // Status bar
    last_exec_us: u64,
    last_error: String,

    // Command palette state
    palette_open: bool,
    palette_input: String,
    palette_selected: usize,
}

impl App {
    pub fn new() -> (Self, Task<Message>) {
        // Process and capture plugins are native-only (DLL injection, DXGI
        // capture). The wasm editor designs graphs; native runners execute them.
        let plugins: Vec<Box<dyn DomainPlugin>> = vec![
            Box::new(TransformPlugin),
            Box::new(MlPlugin),
            #[cfg(not(target_arch = "wasm32"))]
            Box::new(ProcessPlugin),
            #[cfg(not(target_arch = "wasm32"))]
            Box::new(CapturePlugin),
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

        let mut app = Self {
            nodes: HashMap::new(),
            node_order: Vec::new(),
            edges: Vec::new(),
            selected: HashSet::new(),
            camera_position: Point::ORIGIN,
            camera_zoom: 1.0,
            executor,
            plugins,
            catalog,
            converters,
            display_values: HashMap::new(),
            const_inputs: HashMap::new(),
            node_settings: HashMap::new(),
            spawn_counter: 0,
            last_exec_us: 0,
            last_error: String::new(),
            palette_open: false,
            palette_input: String::new(),
            palette_selected: 0,
        };

        // Restore last session (may kick off async node work, e.g. chat nodes).
        let task = app.load_autosave();

        (app, task)
    }

    fn spawn_node(&mut self, type_id: &str, position: Point) -> Task<Message> {
        let exec = self.plugins.iter().find_map(|p| p.create_node(type_id));
        let Some(exec) = exec else { return Task::none() };

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
            .find(|d| d.type_id == type_id)
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
                let _ = self
                    .executor
                    .set_parameter(id, def.name, Value::new(def.default.to_string()));
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
            },
        );
        self.node_order.push(id);

        match type_id {
            "transform.const_f64" => { self.const_inputs.insert(id, "0".to_string()); }
            "transform.const_bool" => { self.const_inputs.insert(id, "false".to_string()); }
            "transform.const_string" => { self.const_inputs.insert(id, String::new()); }
            _ => {}
        }

        let task = self.execute_graph();
        self.autosave();
        task
    }


    fn connect_edge(
        &mut self,
        from_node: NodeId,
        from_pin: PinLabel,
        to_node: NodeId,
        to_pin: PinLabel,
    ) -> Task<Message> {
        // Ignore exact duplicates (snap can re-fire on_connect for the same pair).
        if self.edges.iter().any(|e| {
            e.from_node == from_node
                && e.from_pin == from_pin
                && e.to_node == to_node
                && e.to_pin == to_pin
        }) {
            return Task::none();
        }

        // An input pin holds at most one edge: drop any existing wire into the
        // same target before connecting the new source.
        self.remove_edges_into(to_node, to_pin);

        let edge_id = EdgeId::next();
        self.executor.graph.add_edge(GraphEdge {
            id: edge_id,
            from_node,
            from_pin,
            to_node,
            to_pin,
            semantic: EdgeSemantic::default(),
        });
        self.edges.push(EditorEdge {
            id: edge_id,
            from_node,
            from_pin,
            to_node,
            to_pin,
        });
        // Seed the edge from the source's cached output and recompute only the
        // target subtree. The source is not re-run (no spurious LLM calls).
        self.executor.on_edge_added(edge_id);
        let task = self.execute_graph();
        self.autosave();
        task
    }

    /// Removes every edge feeding the given input pin, from both the editor
    /// state and the executor graph/cache.
    fn remove_edges_into(&mut self, to_node: NodeId, to_pin: PinLabel) {
        let stale: Vec<EdgeId> = self
            .edges
            .iter()
            .filter(|e| e.to_node == to_node && e.to_pin == to_pin)
            .map(|e| e.id)
            .collect();
        for edge_id in stale {
            self.edges.retain(|e| e.id != edge_id);
            self.executor.disconnect_edge(edge_id);
        }
    }

    fn disconnect_edge(
        &mut self,
        from_node: NodeId,
        from_pin: PinLabel,
        to_node: NodeId,
        to_pin: PinLabel,
    ) -> Task<Message> {
        if let Some(pos) = self.edges.iter().position(|e| {
            e.from_node == from_node
                && e.from_pin == from_pin
                && e.to_node == to_node
                && e.to_pin == to_pin
        }) {
            let edge = self.edges.remove(pos);
            self.executor.disconnect_edge(edge.id);
            let task = self.execute_graph();
            self.autosave();
            task
        } else {
            Task::none()
        }
    }

    fn execute_graph(&mut self) -> Task<Message> {
        // web_time::Instant re-exports std on native and uses the browser clock
        // on wasm, so timing works on both targets.
        let start = web_time::Instant::now();
        let task = match self.executor.execute_dirty() {
            Ok(deferred) => {
                self.last_exec_us = start.elapsed().as_micros() as u64;
                self.last_error.clear();
                self.spawn_async(deferred)
            }
            Err(e) => {
                self.last_exec_us = start.elapsed().as_micros() as u64;
                self.last_error = e.to_string();
                Task::none()
            }
        };
        self.update_display_values();
        task
    }

    /// Turns deferred node work into background tasks. Each runs its blocking
    /// work on a tokio blocking thread, then reports back via AsyncNodeDone so
    /// the executor can resume the dependent downstream nodes.
    #[cfg(not(target_arch = "wasm32"))]
    fn spawn_async(&self, deferred: DeferredWork) -> Task<Message> {
        if deferred.is_empty() {
            return Task::none();
        }
        let tasks = deferred.into_iter().map(|(node_id, work)| {
            let raw_id = node_id.0;
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || work.run())
                        .await
                        .unwrap_or_else(|e| {
                            Err(zeughaus_core::ZeughausError::ExecutionFailed(format!(
                                "background task failed: {e}"
                            )))
                        })
                        .map_err(|e| e.to_string())
                },
                move |result| Message::AsyncNodeDone {
                    node_id: raw_id,
                    result,
                },
            )
        });
        Task::batch(tasks)
    }

    // On wasm there is no LLM plugin, so no node ever defers work.
    #[cfg(target_arch = "wasm32")]
    fn spawn_async(&self, _deferred: zeughaus_runtime::DeferredWork) -> Task<Message> {
        Task::none()
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
    fn load_autosave(&mut self) -> Task<Message> {
        let path = Self::autosave_path();
        if let Ok(json) = std::fs::read_to_string(&path)
            && let Ok(doc) = serde_json::from_str::<GraphDocument>(&json)
        {
            self.load_document(doc)
        } else {
            Task::none()
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn load_autosave(&mut self) -> Task<Message> {
        Task::none()
    }

    fn update_display_values(&mut self) {
        self.display_values.clear();
        for &node_id in self.nodes.keys() {
            // Show values from outgoing edges (what this node produced)
            for edge in &self.edges {
                if edge.from_node == node_id
                    && let Some(val) = self.executor.edge_value(edge.id)
                {
                    self.display_values
                        .entry(node_id)
                        .or_insert_with(|| val.to_string());
                }
            }
            // For sink nodes (no outgoing edges), show incoming values
            if !self.display_values.contains_key(&node_id) {
                for edge in &self.edges {
                    if edge.to_node == node_id
                        && let Some(val) = self.executor.edge_value(edge.id)
                    {
                        self.display_values
                            .entry(node_id)
                            .or_insert_with(|| val.to_string());
                    }
                }
            }
        }
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

    fn load_document(&mut self, doc: GraphDocument) -> Task<Message> {
        // Clear current state
        self.nodes.clear();
        self.node_order.clear();
        self.edges.clear();
        self.const_inputs.clear();
        self.node_settings.clear();
        self.display_values.clear();
        self.executor = GraphExecutor::new(Graph::new());

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
            let type_id = &node_data.type_id;
            let exec = self.plugins.iter().find_map(|p| p.create_node(type_id));
            let Some(mut exec) = exec else { continue };

            let id = NodeId(node_data.id);
            // Skip duplicate ids (e.g. a file corrupted by the old id-collision
            // bug); rendering the same id twice causes the doubled-drag glitch.
            if self.nodes.contains_key(&id) {
                continue;
            }
            let pin_defs = exec.pin_definitions().to_vec();
            let setting_defs = exec.settings();
            let position = Point::new(node_data.x, node_data.y);

            let is_const = type_id.starts_with("transform.const_");
            let setting_names: HashSet<&str> = setting_defs.iter().map(|d| d.name).collect();

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
                },
            );
            self.node_order.push(id);
        }

        // Rebuild edges
        for edge_data in &doc.edges {
            let edge_id = EdgeId(edge_data.id);
            let from_pin: &'static str = leak_string(&edge_data.from_pin);
            let to_pin: &'static str = leak_string(&edge_data.to_pin);

            self.executor.graph.add_edge(GraphEdge {
                id: edge_id,
                from_node: NodeId(edge_data.from_node),
                from_pin,
                to_node: NodeId(edge_data.to_node),
                to_pin,
                semantic: EdgeSemantic::default(),
            });

            self.edges.push(EditorEdge {
                id: edge_id,
                from_node: NodeId(edge_data.from_node),
                from_pin,
                to_node: NodeId(edge_data.to_node),
                to_pin,
            });
        }

        // Execute full graph; run any deferred (async) node work that results.
        let deferred = self.executor.execute_all().unwrap_or_default();
        self.update_display_values();
        self.spawn_async(deferred)
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
        let original_idx = get_filtered_command_index(
            &self.palette_input,
            &commands,
            self.palette_selected,
        )?;

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
                // `from` is always the output pin and `to` the input pin.
                return self.connect_edge(
                    NodeId(from.node_id),
                    from.pin_id,
                    NodeId(to.node_id),
                    to.pin_id,
                );
            }
            Message::EdgeDisconnected { from, to } => {
                return self.disconnect_edge(
                    NodeId(from.node_id),
                    from.pin_id,
                    NodeId(to.node_id),
                    to.pin_id,
                );
            }
            Message::GroupMoved { node_ids, delta } => {
                // Fires once on drag release; persist the new positions.
                for raw_id in &node_ids {
                    let id = NodeId(*raw_id);
                    if let Some(node) = self.nodes.get_mut(&id) {
                        node.position = Point::new(
                            node.position.x + delta.x,
                            node.position.y + delta.y,
                        );
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
                let mut tasks = Vec::new();
                for (type_id, pos) in positions {
                    let offset_pos = Point::new(pos.x + 30.0, pos.y + 30.0);
                    tasks.push(self.spawn_node(&type_id, offset_pos));
                }
                return Task::batch(tasks);
            }
            Message::DeleteNodes(ids) => {
                for raw_id in &ids {
                    let id = NodeId(*raw_id);
                    self.nodes.remove(&id);
                    self.node_order.retain(|n| *n != id);
                    self.edges.retain(|e| e.from_node != id && e.to_node != id);
                    self.executor.remove_node(id);
                    self.const_inputs.remove(&id);
                    self.node_settings.remove(&id);
                }
                self.autosave();
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
                return self.spawn_node(&type_id, pos);
            }
            Message::ConstValueChanged { node_id, value } => {
                let id = NodeId(node_id);
                self.const_inputs.insert(id, value.clone());
                let node_type = self.nodes.get(&id).map(|n| n.type_id.as_str());
                let mut task = Task::none();
                match node_type {
                    Some("transform.const_f64") => {
                        if let Ok(f) = value.parse::<f64>() {
                            let _ = self.executor.set_parameter(id, "value", Value::new(f));
                            task = self.execute_graph();
                        }
                    }
                    Some("transform.const_bool") => {
                        let b = value == "true" || value == "1";
                        let _ = self.executor.set_parameter(id, "value", Value::new(b));
                        task = self.execute_graph();
                    }
                    Some("transform.const_string") => {
                        let _ = self.executor.set_parameter(id, "value", Value::new(value));
                        task = self.execute_graph();
                    }
                    _ => {}
                }
                self.autosave();
                return task;
            }
            Message::NodeSettingChanged { node_id, key, value } => {
                let id = NodeId(node_id);
                self.node_settings
                    .entry(id)
                    .or_default()
                    .insert(key.clone(), value.clone());
                let _ = self.executor.set_parameter(id, &key, Value::new(value));
                let task = self.execute_graph();
                self.autosave();
                return task;
            }
            Message::Tick => {
                // No-op: re-rendering advances the widget's animation clock.
            }
            Message::AsyncNodeDone { node_id, result } => {
                let id = NodeId(node_id);
                match result {
                    Ok(outputs) => match self.executor.deliver_async_result(id, outputs) {
                        Ok(deferred) => {
                            self.last_error.clear();
                            self.update_display_values();
                            return self.spawn_async(deferred);
                        }
                        Err(e) => {
                            self.last_error = e.to_string();
                            self.update_display_values();
                        }
                    },
                    Err(e) => {
                        self.executor.mark_error(id);
                        self.last_error = e;
                        self.update_display_values();
                    }
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
                let task = self.load_document(doc);
                self.autosave();
                return task;
            }
        }
        Task::none()
    }

    pub fn view(&self) -> Element<'_, Message> {
        let mut ng: NodeGraph<'_, u64, PinLabel, Color, Message, Theme, _> = NodeGraph::default();

        ng = ng
            .on_connect(|from, to| Message::EdgeConnected { from, to })
            .on_disconnect(|from, to| Message::EdgeDisconnected { from, to })
            .on_move(|delta, node_ids| Message::GroupMoved { node_ids, delta })
            .on_select(Message::SelectionChanged)
            .on_clone(Message::CloneNodes)
            .on_delete(Message::DeleteNodes)
            .on_pan(|position, zoom| Message::CameraChanged { position, zoom })
            .view(self.camera_position, self.camera_zoom)
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
                    let from_pin = f.pin_defs.iter().find(|p| p.name == *from.pin_id());
                    let to_pin = t.pin_defs.iter().find(|p| p.name == *to.pin_id());
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
                    // Converters are directional (output type -> input type), so
                    // resolve which side is the output before checking.
                    let (out_pin, in_pin) = if fp.direction == PinDirection::Output {
                        (fp, tp)
                    } else {
                        (tp, fp)
                    };
                    converters.compatible(out_pin.type_name, in_pin.type_name)
                }
            });

        for id in &self.node_order {
            if let Some(node) = self.nodes.get(id) {
                let display_val = self.display_values.get(id).map(|s| s.as_str());
                let const_input = self.const_inputs.get(id).map(|s| s.as_str());
                let settings = self.node_settings.get(id);
                let content = build_node_element(node, display_val, const_input, settings);
                // Per-node activity feedback: red marching-ants on error, accent
                // marching-ants while the node is working (async pending).
                let pending = self.executor.is_pending(*id);
                let errored = self.executor.is_error(*id);
                let node_widget = ng_node(node.id.0, node.position, content)
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
                        if pending {
                            return NodeStyle {
                                corner_radius: 8.0,
                                opacity: 0.88,
                                border_color: Color::from_rgb(0.3, 0.75, 0.95).into(),
                                border_pattern: Pattern::dashed(2.0, 6.0, 4.0).flow(45.0),
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
                        |theme, pin: &PinInfo<'_, PinLabel, Color>, _other, status| PinStyle {
                            color: (*pin.info()).into(),
                            ..default_pin_style(theme, status)
                        },
                    );
                ng.push_node(node_widget);
            }
        }

        for edge in &self.edges {
            // Color edge based on source pin type
            let edge_color = self
                .nodes
                .get(&edge.from_node)
                .and_then(|n| n.pin_defs.iter().find(|p| p.name == edge.from_pin))
                .map(|p| pin_color(p.type_name))
                .unwrap_or(Color::from_rgb(0.6, 0.6, 0.6));

            // An edge reflects its source node's state: red marching-ants when
            // the source errored (broken data), accent flow while it works.
            let src_error = self.executor.is_error(edge.from_node);
            let src_pending = self.executor.is_pending(edge.from_node);

            let edge_widget = ng_edge(
                PinRef::new(edge.from_node.0, edge.from_pin),
                PinRef::new(edge.to_node.0, edge.to_pin),
                (),
            )
            .style(move |theme, status, _start, _end| {
                if src_error {
                    return EdgeStyle::error();
                }
                if src_pending {
                    return EdgeStyle {
                        stroke_color: edge_color.into(),
                        pattern: Pattern::dashed(2.0, 6.0, 4.0).flow(45.0),
                        ..default_edge_style(theme, status)
                    };
                }
                EdgeStyle {
                    stroke_color: edge_color.into(),
                    ..default_edge_style(theme, status)
                }
            });
            ng.push_edge(edge_widget);
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

        // Status bar
        let running = self.executor.pending_count();
        let running_text = if running > 0 {
            format!(" | running: {running}")
        } else {
            String::new()
        };
        let status_text = if self.last_error.is_empty() {
            format!(
                "  {} nodes | {} edges | exec: {}us{}",
                self.nodes.len(),
                self.edges.len(),
                self.last_exec_us,
                running_text,
            )
        } else {
            format!(
                "  {} nodes | {} edges | exec: {}us{} | ERROR: {}",
                self.nodes.len(),
                self.edges.len(),
                self.last_exec_us,
                running_text,
                self.last_error,
            )
        };

        let error_color = if self.last_error.is_empty() {
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

        column![graph_view, status_bar].into()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let events = iced::event::listen_with(|event, _status, _id| {
            if let Event::Keyboard(keyboard::Event::KeyPressed {
                key, modifiers, ..
            }) = event
            {
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

        // The library only auto-redraws for animated edges, not node borders.
        // While any node is working, drive ~30fps redraws so the marching-ants
        // node border animates. Native only: iced::time::every needs the tokio
        // executor feature, and async work only runs natively anyway.
        #[cfg(not(target_arch = "wasm32"))]
        if self.executor.pending_count() > 0 {
            let ticks = iced::time::every(std::time::Duration::from_millis(33))
                .map(|_| Message::Tick);
            return Subscription::batch([events, ticks]);
        }
        events
    }

    pub fn theme(&self) -> Theme {
        Theme::Dark
    }
}

fn build_node_element<'a>(
    node: &'a EditorNode,
    display_value: Option<&'a str>,
    const_input: Option<&'a str>,
    settings: Option<&'a HashMap<String, String>>,
) -> Element<'a, Message, Theme> {
    let is_const = node.type_id.starts_with("transform.const_");

    let mut items: Vec<Element<'_, Message, Theme>> = Vec::new();

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
        let pin_type = node.pin_defs.first().map(|p| p.type_name).unwrap_or("any");

        let input_field = text_input(placeholder, input_text)
            .on_input(move |v| Message::ConstValueChanged {
                node_id: node_raw_id,
                value: v,
            })
            .size(13)
            .width(Length::Fill);

        let pin: Element<'_, Message, Theme> =
            node_pin(PinSide::Right, "value", input_field)
                .direction(NgPinDirection::Output)
                .info(pin_color(pin_type))
                .into();
        items.push(pin);
    } else {
        for pin_def in &node.pin_defs {
            let side = match pin_def.direction {
                PinDirection::Input => PinSide::Left,
                PinDirection::Output => PinSide::Right,
            };
            let direction = match pin_def.direction {
                PinDirection::Input => NgPinDirection::Input,
                PinDirection::Output => NgPinDirection::Output,
            };

            let color = pin_color(pin_def.type_name);

            let pin: Element<'_, Message, Theme> =
                node_pin(side, pin_def.name, text(pin_def.name).size(12))
                    .direction(direction)
                    .info(color)
                    .into();
            items.push(pin);
        }
    }

    // In-node text settings (e.g. LLM base_url/model/prompt). Each renders a
    // labeled text input that updates the node parameter on edit. The number of
    // settings is fixed per node type, so the widget tree stays stable.
    for def in &node.settings {
        let node_raw_id = node.id.0;
        let key = def.name;
        let current = settings
            .and_then(|m| m.get(def.name))
            .map(|s| s.as_str())
            .unwrap_or(def.default);

        let field = text_input(def.placeholder, current)
            .on_input(move |v| Message::NodeSettingChanged {
                node_id: node_raw_id,
                key: key.to_string(),
                value: v,
            })
            .size(12)
            .width(Length::Fill);

        items.push(
            column![
                text(def.name).size(11).color(Color::from_rgb(0.6, 0.6, 0.6)),
                field
            ]
            .spacing(1)
            .into(),
        );
    }

    // Always render a value display row to keep widget tree structure stable.
    // Empty text when no value -- prevents iced widget state downcast panics
    // caused by children count changing between view() calls.
    if !is_const {
        let (prefix, value_text) = match display_value {
            Some(val) => ("= ", val),
            None => ("", ""),
        };
        items.push(
            row![
                text(prefix).size(12).color(Color::from_rgb(0.6, 0.6, 0.6)),
                text(value_text).size(13).color(Color::from_rgb(0.9, 0.9, 0.5))
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
    container(inner).width(180.0).into()
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

fn pin_color(type_name: &str) -> Color {
    match type_name {
        "f64" => Color::from_rgb(0.3, 0.8, 0.4),
        "String" => Color::from_rgb(0.9, 0.7, 0.2),
        "bool" => Color::from_rgb(0.3, 0.5, 0.9),
        "Conversation" => Color::from_rgb(0.8, 0.4, 0.85),
        "any" => Color::from_rgb(0.7, 0.7, 0.7),
        _ => Color::from_rgb(0.6, 0.6, 0.6),
    }
}


/// Leaks a string to get a &'static str. Used for pin labels loaded from JSON.
/// Acceptable for graph loading since pin labels are a small, bounded set.
fn leak_string(s: &str) -> &'static str {
    // Check common pin names first to avoid leaking
    match s {
        "value" => "value",
        "result" => "result",
        "input" => "input",
        "a" => "a",
        "b" => "b",
        "t" => "t",
        "condition" => "condition",
        "true_val" => "true_val",
        "false_val" => "false_val",
        "min" => "min",
        "max" => "max",
        "base" => "base",
        "exp" => "exp",
        "in_min" => "in_min",
        "in_max" => "in_max",
        "out_min" => "out_min",
        "out_max" => "out_max",
        "sep" => "sep",
        "text" => "text",
        "length" => "length",
        "epsilon" => "epsilon",
        "conv" => "conv",
        "out" => "out",
        "reply" => "reply",
        "tok_per_s" => "tok_per_s",
        other => Box::leak(other.to_string().into_boxed_str()),
    }
}
