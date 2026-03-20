use std::collections::{HashMap, HashSet};

use iced::keyboard;
use iced::widget::text;
use iced::{Element, Event, Length, Point, Subscription, Task, Theme};
use iced_nodegraph::{
    NodeContentStyle, NodeGraph, NodeStatus, PinDirection as NgPinDirection, PinRef, PinSide,
    node_pin, simple_node,
};
use zeughaus_core::{
    DomainPlugin, EdgeId, EdgeSemantic, NodeConfig, NodeDefinition, NodeId, PinDefinition,
    PinDirection, Value,
};
use zeughaus_runtime::{GraphEdge, GraphExecutor, GraphNode, Graph};
use zeughaus_transform::TransformPlugin;

use crate::message::{Message, PinLabel};

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
}

impl App {
    pub fn new() -> (Self, Task<Message>) {
        let plugins: Vec<Box<dyn DomainPlugin>> = vec![Box::new(TransformPlugin)];
        let catalog: Vec<NodeDefinition> = plugins.iter().flat_map(|p| p.node_catalog()).collect();
        let executor = GraphExecutor::new(Graph::new());

        let app = Self {
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
        };

        (app, Task::none())
    }

    fn spawn_node(&mut self, type_id: &str, position: Point) {
        // Find the plugin that can create this node
        let exec = self
            .plugins
            .iter()
            .find_map(|p| p.create_node(type_id));

        let Some(exec) = exec else { return };

        let pin_defs = exec.pin_definitions().to_vec();
        let id = NodeId::next();

        // Find display name from catalog
        let display_name = self
            .catalog
            .iter()
            .find(|d| d.type_id == type_id)
            .map(|d| d.display_name.to_string())
            .unwrap_or_else(|| type_id.to_string());

        // Add to runtime graph
        self.executor.graph.add_node(GraphNode {
            id,
            type_id: type_id.to_string(),
            config: NodeConfig::default(),
            pin_defs: pin_defs.clone(),
            position: (position.x, position.y),
        });
        self.executor.register_node(id, exec);

        // Add to editor state
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

        // Execute to get initial values
        self.execute_graph();
    }

    fn connect_edge(&mut self, from_node: NodeId, from_pin: PinLabel, to_node: NodeId, to_pin: PinLabel) {
        let edge_id = EdgeId::next();

        // Add to runtime graph
        self.executor.graph.add_edge(GraphEdge {
            id: edge_id,
            from_node,
            from_pin,
            to_node,
            to_pin,
            semantic: EdgeSemantic::default(),
        });

        // Add to editor state
        self.edges.push(EditorEdge {
            id: edge_id,
            from_node,
            from_pin,
            to_node,
            to_pin,
        });

        // Mark downstream dirty and execute
        self.executor.mark_dirty_downstream(from_node);
        self.execute_graph();
    }

    fn disconnect_edge(&mut self, from_node: NodeId, from_pin: PinLabel, to_node: NodeId, to_pin: PinLabel) {
        // Find and remove the edge
        if let Some(pos) = self.edges.iter().position(|e| {
            e.from_node == from_node
                && e.from_pin == from_pin
                && e.to_node == to_node
                && e.to_pin == to_pin
        }) {
            let edge = self.edges.remove(pos);
            self.executor.graph.remove_edge(edge.id);
            // Mark downstream dirty and re-execute
            self.executor.mark_dirty_downstream(to_node);
            self.execute_graph();
        }
    }

    fn execute_graph(&mut self) {
        if let Err(e) = self.executor.execute_dirty() {
            eprintln!("Execution error: {e}");
            return;
        }
        self.update_display_values();
    }

    fn update_display_values(&mut self) {
        self.display_values.clear();

        for (&node_id, node) in &self.nodes {
            if node.type_id == "transform.display" {
                // Read the value from incoming edges
                for edge in &self.edges {
                    if edge.to_node == node_id
                        && let Some(val) = self.executor.edge_value(edge.id)
                    {
                        let text = format_value(val);
                        self.display_values.insert(node_id, text);
                    }
                }
            } else if node.type_id == "transform.const_f64" {
                // Show current const value from outgoing edges
                for edge in &self.edges {
                    if edge.from_node == node_id
                        && let Some(val) = self.executor.edge_value(edge.id)
                    {
                        let text = format_value(val);
                        self.display_values.insert(node_id, text);
                    }
                }
            }
        }
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::EdgeConnected { from, to } => {
                self.connect_edge(NodeId(from.node_id), from.pin_id, NodeId(to.node_id), to.pin_id);
            }
            Message::EdgeDisconnected { from, to } => {
                self.disconnect_edge(NodeId(from.node_id), from.pin_id, NodeId(to.node_id), to.pin_id);
            }
            Message::NodeMoved { node_id, position } => {
                let id = NodeId(node_id);
                if let Some(node) = self.nodes.get_mut(&id) {
                    node.position = position;
                }
            }
            Message::SelectionChanged(sel) => {
                self.selected = sel.into_iter().map(NodeId).collect();
            }
            Message::DeleteNodes(ids) => {
                for raw_id in &ids {
                    let id = NodeId(*raw_id);
                    self.nodes.remove(&id);
                    self.node_order.retain(|n| *n != id);

                    // Remove edges connected to this node
                    let edge_ids: Vec<EdgeId> = self
                        .edges
                        .iter()
                        .filter(|e| e.from_node == id || e.to_node == id)
                        .map(|e| e.id)
                        .collect();
                    for eid in &edge_ids {
                        self.executor.graph.remove_edge(*eid);
                    }
                    self.edges.retain(|e| e.from_node != id && e.to_node != id);

                    // Remove from runtime
                    self.executor.graph.remove_node(id);
                }
            }
            Message::CameraChanged { position, zoom } => {
                self.camera_position = position;
                self.camera_zoom = zoom;
            }
            Message::KeyPressed(key) => {
                // Temporary keyboard shortcuts for spawning nodes
                let center = self.viewport_center();
                match key {
                    keyboard::Key::Character(ref c) if c.as_str() == "1" => {
                        self.spawn_node("transform.const_f64", center);
                    }
                    keyboard::Key::Character(ref c) if c.as_str() == "2" => {
                        self.spawn_node("transform.add", center);
                    }
                    keyboard::Key::Character(ref c) if c.as_str() == "3" => {
                        self.spawn_node("transform.multiply", center);
                    }
                    keyboard::Key::Character(ref c) if c.as_str() == "4" => {
                        self.spawn_node("transform.to_string", center);
                    }
                    keyboard::Key::Character(ref c) if c.as_str() == "5" => {
                        self.spawn_node("transform.display", center);
                    }
                    keyboard::Key::Character(ref c) if c.as_str() == "e" => {
                        // Execute all
                        self.executor.execute_all().ok();
                        self.update_display_values();
                    }
                    _ => {}
                }
            }
        }
        Task::none()
    }

    fn viewport_center(&self) -> Point {
        // Approximate center of viewport in world coordinates
        let screen_center = Point::new(640.0, 400.0);
        Point::new(
            screen_center.x / self.camera_zoom - self.camera_position.x,
            screen_center.y / self.camera_zoom - self.camera_position.y,
        )
    }

    pub fn view(&self) -> Element<'_, Message> {
        let mut ng: NodeGraph<'_, u64, PinLabel, u64, Message, Theme, _> = NodeGraph::default();

        ng = ng
            .on_connect(|from, to| Message::EdgeConnected { from, to })
            .on_disconnect(|from, to| Message::EdgeDisconnected { from, to })
            .on_move(|node_id, position| Message::NodeMoved { node_id, position })
            .on_select(Message::SelectionChanged)
            .on_delete(Message::DeleteNodes)
            .on_camera_change(|position, zoom| Message::CameraChanged { position, zoom })
            .initial_camera(self.camera_position, self.camera_zoom)
            .node_style(|_theme, status, base| match status {
                NodeStatus::Selected => base
                    .border_color(iced::Color::from_rgb(0.3, 0.6, 1.0))
                    .border_width(2.5),
                NodeStatus::Idle => base,
            });

        for id in &self.node_order {
            if let Some(node) = self.nodes.get(id) {
                let display_val = self.display_values.get(id).map(|s| s.as_str());
                let content = build_node_element(node, display_val);
                ng.push_node(node.id.0, node.position, content);
            }
        }

        for edge in &self.edges {
            ng.push_edge(
                PinRef::new(edge.from_node.0, edge.from_pin),
                PinRef::new(edge.to_node.0, edge.to_pin),
            );
        }

        iced::widget::container(ng)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        iced::event::listen_with(|event, _status, _id| {
            if let Event::Keyboard(keyboard::Event::KeyPressed {
                key, modifiers, ..
            }) = event
            {
                // Only handle when no modifiers (avoid capturing Ctrl+C etc.)
                if modifiers.is_empty() {
                    return Some(Message::KeyPressed(key));
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
    node: &EditorNode,
    display_value: Option<&'a str>,
) -> Element<'a, Message, Theme> {
    let theme = Theme::Dark;
    let style = match node.type_id.as_str() {
        "transform.const_f64" => NodeContentStyle::input(&theme),
        "transform.display" => NodeContentStyle::output(&theme),
        _ => NodeContentStyle::process(&theme),
    };

    let mut pins: Vec<Element<'_, Message, Theme>> = Vec::new();

    for pin_def in &node.pin_defs {
        let side = match pin_def.direction {
            PinDirection::Input => PinSide::Left,
            PinDirection::Output => PinSide::Right,
        };
        let direction = match pin_def.direction {
            PinDirection::Input => NgPinDirection::Input,
            PinDirection::Output => NgPinDirection::Output,
        };

        let pin: Element<'_, Message, Theme> =
            node_pin(side, pin_def.name, text(pin_def.name).size(12))
                .direction(direction)
                .into();
        pins.push(pin);
    }

    // Show display value in node body if available
    if let Some(val) = display_value {
        pins.push(text(val).size(14).into());
    }

    let body: Element<'_, Message, Theme> = iced::widget::column(pins).spacing(4).into();

    simple_node(&node.display_name, style, body)
}

fn format_value(val: &Value) -> String {
    if let Some(f) = val.downcast_ref::<f64>() {
        format!("{f}")
    } else if let Some(s) = val.downcast_ref::<String>() {
        s.clone()
    } else if let Some(b) = val.downcast_ref::<bool>() {
        format!("{b}")
    } else {
        format!("<{}>", val.type_name())
    }
}
