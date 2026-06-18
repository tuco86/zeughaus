use std::collections::{HashMap, HashSet};
#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;

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
    NodeDefinition, NodeId, PinDefinition, PinDirection, Value,
};
use zeughaus_runtime::{Graph, GraphEdge, GraphExecutor, GraphNode};
#[cfg(not(target_arch = "wasm32"))]
use zeughaus_capture::CapturePlugin;
#[cfg(not(target_arch = "wasm32"))]
use zeughaus_process::ProcessPlugin;
use zeughaus_transform::TransformPlugin;

use crate::message::{Message, PinLabel};
use crate::palette;

pub struct EditorNode {
    pub id: NodeId,
    pub type_id: String,
    pub display_name: String,
    pub position: Point,
    pub pin_defs: Vec<PinDefinition>,
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

    // Display values (node_id -> display string)
    display_values: HashMap<NodeId, String>,

    // Const node text inputs (node_id -> current text)
    const_inputs: HashMap<NodeId, String>,

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
            #[cfg(not(target_arch = "wasm32"))]
            Box::new(ProcessPlugin),
            #[cfg(not(target_arch = "wasm32"))]
            Box::new(CapturePlugin),
        ];
        let catalog: Vec<NodeDefinition> = plugins.iter().flat_map(|p| p.node_catalog()).collect();
        let executor = GraphExecutor::new(Graph::new());

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
            display_values: HashMap::new(),
            const_inputs: HashMap::new(),
            spawn_counter: 0,
            last_exec_us: 0,
            last_error: String::new(),
            palette_open: false,
            palette_input: String::new(),
            palette_selected: 0,
        };

        // Restore last session
        app.load_autosave();

        (app, Task::none())
    }

    fn spawn_node(&mut self, type_id: &str, position: Point) {
        let exec = self.plugins.iter().find_map(|p| p.create_node(type_id));
        let Some(exec) = exec else { return };

        // Stagger each new node so they don't pile up
        let offset = (self.spawn_counter % 10) as f32 * 30.0;
        self.spawn_counter += 1;
        let position = Point::new(position.x + offset, position.y + offset);

        let pin_defs = exec.pin_definitions().to_vec();
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

        self.nodes.insert(
            id,
            EditorNode {
                id,
                type_id: type_id.to_string(),
                display_name,
                position,
                pin_defs,
            },
        );
        self.node_order.push(id);

        match type_id {
            "transform.const_f64" => { self.const_inputs.insert(id, "0".to_string()); }
            "transform.const_bool" => { self.const_inputs.insert(id, "false".to_string()); }
            "transform.const_string" => { self.const_inputs.insert(id, String::new()); }
            _ => {}
        }

        self.execute_graph();
        self.autosave();
    }

    /// Normalize edge direction: ensure from=Output pin, to=Input pin.
    /// iced_nodegraph fires on_connect with drag-start as "from" which may be an Input pin.
    fn normalize_edge_direction(
        &self,
        a: PinRef<u64, PinLabel>,
        b: PinRef<u64, PinLabel>,
    ) -> (NodeId, PinLabel, NodeId, PinLabel) {
        let a_node = NodeId(a.node_id);
        let b_node = NodeId(b.node_id);

        // Check if "a" pin is an Output pin
        let a_is_output = self
            .nodes
            .get(&a_node)
            .and_then(|n| n.pin_defs.iter().find(|p| p.name == a.pin_id))
            .is_some_and(|p| p.direction == PinDirection::Output);

        if a_is_output {
            // a=Output, b=Input (correct order)
            (a_node, a.pin_id, b_node, b.pin_id)
        } else {
            // a=Input, b=Output (swap)
            (b_node, b.pin_id, a_node, a.pin_id)
        }
    }

    fn connect_edge(
        &mut self,
        from_node: NodeId,
        from_pin: PinLabel,
        to_node: NodeId,
        to_pin: PinLabel,
    ) {
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
        self.executor.mark_dirty_downstream(from_node);
        self.execute_graph();
        self.autosave();
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
            self.execute_graph();
            self.autosave();
        }
    }

    fn execute_graph(&mut self) {
        // web_time::Instant re-exports std on native and uses the browser clock
        // on wasm, so timing works on both targets.
        let start = web_time::Instant::now();
        match self.executor.execute_dirty() {
            Ok(()) => {
                self.last_exec_us = start.elapsed().as_micros() as u64;
                self.last_error.clear();
            }
            Err(e) => {
                self.last_exec_us = start.elapsed().as_micros() as u64;
                self.last_error = e.to_string();
            }
        }
        self.update_display_values();
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
        let path = Self::autosave_path();
        if let Ok(json) = std::fs::read_to_string(&path)
            && let Ok(doc) = serde_json::from_str::<GraphDocument>(&json)
        {
            self.load_document(doc);
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn load_autosave(&mut self) {}

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
                let params = self
                    .const_inputs
                    .get(id)
                    .map(|v| vec![("value".to_string(), v.clone())])
                    .unwrap_or_default();
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

    fn load_document(&mut self, doc: GraphDocument) {
        // Clear current state
        self.nodes.clear();
        self.node_order.clear();
        self.edges.clear();
        self.const_inputs.clear();
        self.display_values.clear();
        self.executor = GraphExecutor::new(Graph::new());

        // Rebuild from document
        for node_data in &doc.nodes {
            let type_id = &node_data.type_id;
            let exec = self.plugins.iter().find_map(|p| p.create_node(type_id));
            let Some(mut exec) = exec else { continue };

            let id = NodeId(node_data.id);
            let pin_defs = exec.pin_definitions().to_vec();
            let position = Point::new(node_data.x, node_data.y);

            // Apply saved parameters
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
                self.const_inputs.insert(id, value_str.clone());
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

        // Execute full graph
        let _ = self.executor.execute_all();
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
                // iced_nodegraph passes from=drag-start, to=snap-target.
                // Normalize: from must be the Output pin, to must be the Input pin.
                let (out_node, out_pin, in_node, in_pin) =
                    self.normalize_edge_direction(from, to);
                self.connect_edge(out_node, out_pin, in_node, in_pin);
            }
            Message::EdgeDisconnected { from, to } => {
                let (out_node, out_pin, in_node, in_pin) =
                    self.normalize_edge_direction(from, to);
                self.disconnect_edge(out_node, out_pin, in_node, in_pin);
            }
            Message::GroupMoved { node_ids, delta } => {
                for raw_id in &node_ids {
                    let id = NodeId(*raw_id);
                    if let Some(node) = self.nodes.get_mut(&id) {
                        node.position = Point::new(
                            node.position.x + delta.x,
                            node.position.y + delta.y,
                        );
                    }
                }
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
                    let offset_pos = Point::new(pos.x + 30.0, pos.y + 30.0);
                    self.spawn_node(&type_id, offset_pos);
                }
            }
            Message::DeleteNodes(ids) => {
                for raw_id in &ids {
                    let id = NodeId(*raw_id);
                    self.nodes.remove(&id);
                    self.node_order.retain(|n| *n != id);
                    self.edges.retain(|e| e.from_node != id && e.to_node != id);
                    self.executor.remove_node(id);
                    self.const_inputs.remove(&id);
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
                self.spawn_node(&type_id, pos);
            }
            Message::ConstValueChanged { node_id, value } => {
                let id = NodeId(node_id);
                self.const_inputs.insert(id, value.clone());
                let node_type = self.nodes.get(&id).map(|n| n.type_id.as_str());
                match node_type {
                    Some("transform.const_f64") => {
                        if let Ok(f) = value.parse::<f64>() {
                            let _ = self.executor.set_parameter(id, "value", Value::new(f));
                            self.execute_graph();
                        }
                    }
                    Some("transform.const_bool") => {
                        let b = value == "true" || value == "1";
                        let _ = self.executor.set_parameter(id, "value", Value::new(b));
                        self.execute_graph();
                    }
                    Some("transform.const_string") => {
                        let _ = self.executor.set_parameter(id, "value", Value::new(value));
                        self.execute_graph();
                    }
                    _ => {}
                }
                self.autosave();
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
                let nodes = &self.nodes;
                move |from, to| {
                    let from_node = nodes.get(&NodeId(*from.node_id()));
                    let to_node = nodes.get(&NodeId(*to.node_id()));
                    match (from_node, to_node) {
                        (Some(f), Some(t)) => {
                            let from_type = f.pin_defs.iter()
                                .find(|p| p.name == *from.pin_id())
                                .map(|p| p.type_name);
                            let to_type = t.pin_defs.iter()
                                .find(|p| p.name == *to.pin_id())
                                .map(|p| p.type_name);
                            match (from_type, to_type) {
                                (Some("any"), _) | (_, Some("any")) => true,
                                (Some(a), Some(b)) => a == b,
                                _ => true,
                            }
                        }
                        _ => true,
                    }
                }
            });

        for id in &self.node_order {
            if let Some(node) = self.nodes.get(id) {
                let display_val = self.display_values.get(id).map(|s| s.as_str());
                let const_input = self.const_inputs.get(id).map(|s| s.as_str());
                let content = build_node_element(node, display_val, const_input);
                let node_widget = ng_node(node.id.0, node.position, content)
                    .style(|theme, status| {
                        let base = default_node_style(theme, status);
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

            let edge_widget = ng_edge(
                PinRef::new(edge.from_node.0, edge.from_pin),
                PinRef::new(edge.to_node.0, edge.to_pin),
                (),
            )
            .style(move |theme, status, _start, _end| EdgeStyle {
                stroke_color: edge_color.into(),
                ..default_edge_style(theme, status)
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
        let status_text = if self.last_error.is_empty() {
            format!(
                "  {} nodes | {} edges | exec: {}us",
                self.nodes.len(),
                self.edges.len(),
                self.last_exec_us,
            )
        } else {
            format!(
                "  {} nodes | {} edges | exec: {}us | ERROR: {}",
                self.nodes.len(),
                self.edges.len(),
                self.last_exec_us,
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
        iced::event::listen_with(|event, _status, _id| {
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
        })
    }

    pub fn theme(&self) -> Theme {
        Theme::Dark
    }
}

fn build_node_element<'a>(
    node: &'a EditorNode,
    display_value: Option<&'a str>,
    const_input: Option<&'a str>,
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
        other => Box::leak(other.to_string().into_boxed_str()),
    }
}
