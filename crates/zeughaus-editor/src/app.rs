use std::collections::{HashMap, HashSet};

use iced::keyboard;
use iced::widget::{column, container, stack, text};
use iced::{Color, Element, Event, Length, Point, Subscription, Task, Theme};
use iced_nodegraph::{
    NodeConfig as NgNodeConfig, NodeContentStyle, NodeGraph, NodeStatus,
    PinDirection as NgPinDirection, PinRef, PinSide, node_pin, simple_node,
};
use iced_palette::{get_filtered_command_index, is_toggle_shortcut};
use zeughaus_core::{
    DomainPlugin, EdgeId, EdgeSemantic, NodeConfig, NodeDefinition, NodeId, PinDefinition,
    PinDirection, Value,
};
use zeughaus_runtime::{Graph, GraphEdge, GraphExecutor, GraphNode};
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

    // Command palette state
    palette_open: bool,
    palette_input: String,
    palette_selected: usize,
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
            palette_open: false,
            palette_input: String::new(),
            palette_selected: 0,
        };

        (app, Task::none())
    }

    fn spawn_node(&mut self, type_id: &str, position: Point) {
        let exec = self.plugins.iter().find_map(|p| p.create_node(type_id));
        let Some(exec) = exec else { return };

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
        self.execute_graph();
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
            self.executor.graph.remove_edge(edge.id);
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
                for edge in &self.edges {
                    if edge.to_node == node_id
                        && let Some(val) = self.executor.edge_value(edge.id)
                    {
                        self.display_values.insert(node_id, format_value(val));
                    }
                }
            } else if node.type_id == "transform.const_f64" {
                for edge in &self.edges {
                    if edge.from_node == node_id
                        && let Some(val) = self.executor.edge_value(edge.id)
                    {
                        self.display_values.insert(node_id, format_value(val));
                    }
                }
            }
        }
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
                self.connect_edge(
                    NodeId(from.node_id),
                    from.pin_id,
                    NodeId(to.node_id),
                    to.pin_id,
                );
            }
            Message::EdgeDisconnected { from, to } => {
                self.disconnect_edge(
                    NodeId(from.node_id),
                    from.pin_id,
                    NodeId(to.node_id),
                    to.pin_id,
                );
            }
            Message::NodeMoved { node_id, position } => {
                if let Some(node) = self.nodes.get_mut(&NodeId(node_id)) {
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
                    self.executor.graph.remove_node(id);
                }
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
        }
        Task::none()
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
                    .border_color(Color::from_rgb(0.3, 0.6, 1.0))
                    .border_width(2.5),
                NodeStatus::Idle => base,
            });

        let node_cfg = NgNodeConfig::new().corner_radius(8.0).opacity(0.88);

        for id in &self.node_order {
            if let Some(node) = self.nodes.get(id) {
                let display_val = self.display_values.get(id).map(|s| s.as_str());
                let content = build_node_element(node, display_val);
                ng.push_node_styled(node.id.0, node.position, content, node_cfg.clone());
            }
        }

        for edge in &self.edges {
            ng.push_edge(
                PinRef::new(edge.from_node.0, edge.from_pin),
                PinRef::new(edge.to_node.0, edge.to_pin),
            );
        }

        let graph_view: Element<'_, Message> = container(ng)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();

        if self.palette_open {
            let commands = palette::build_commands(&self.catalog);
            let palette_view = palette::view(&self.palette_input, &commands, self.palette_selected);

            // Empty spacer to keep palette not full screen
            let overlay = container(palette_view)
                .width(Length::Fill)
                .padding(80.0)
                .align_x(iced::Alignment::Center);

            stack![graph_view, overlay].into()
        } else {
            graph_view
        }
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
    node: &EditorNode,
    display_value: Option<&'a str>,
) -> Element<'a, Message, Theme> {
    let theme = Theme::Dark;
    let style = match node.type_id.as_str() {
        "transform.const_f64" => NodeContentStyle::input(&theme),
        "transform.display" => NodeContentStyle::output(&theme),
        _ => NodeContentStyle::process(&theme),
    };

    let mut items: Vec<Element<'_, Message, Theme>> = Vec::new();

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
                .color(color)
                .into();
        items.push(pin);
    }

    if let Some(val) = display_value {
        items.push(
            text(val)
                .size(14)
                .color(Color::from_rgb(0.9, 0.9, 0.5))
                .into(),
        );
    }

    let body: Element<'_, Message, Theme> = column(items).spacing(4).into();
    let node_el = simple_node(&node.display_name, style, body);
    container(node_el).width(180.0).into()
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
