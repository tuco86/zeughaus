use std::collections::{HashMap, HashSet};

use iced::widget::text;
use iced::{Element, Length, Point, Task, Theme};
use iced_nodegraph::{
    NodeContentStyle, NodeGraph, NodeStatus, PinDirection as NgPinDirection, PinRef, PinSide,
    node_pin, simple_node,
};
use zeughaus_core::{NodeId, PinDefinition, PinDirection};

use crate::message::{Message, PinLabel};

pub struct EditorNode {
    pub id: NodeId,
    pub type_id: String,
    pub display_name: String,
    pub position: Point,
    pub pin_defs: Vec<PinDefinition>,
}

pub struct EditorEdge {
    pub from_node: NodeId,
    pub from_pin: PinLabel,
    pub to_node: NodeId,
    pub to_pin: PinLabel,
}

pub struct App {
    pub nodes: HashMap<NodeId, EditorNode>,
    pub node_order: Vec<NodeId>,
    pub edges: Vec<EditorEdge>,
    pub selected: HashSet<NodeId>,
    pub camera_position: Point,
    pub camera_zoom: f32,
}

impl App {
    pub fn new() -> (Self, Task<Message>) {
        let mut nodes = HashMap::new();
        let mut node_order = Vec::new();

        let n1 = NodeId::next();
        let n2 = NodeId::next();
        let n3 = NodeId::next();

        nodes.insert(
            n1,
            EditorNode {
                id: n1,
                type_id: "transform.const_f64".to_string(),
                display_name: "Const A".to_string(),
                position: Point::new(50.0, 150.0),
                pin_defs: vec![PinDefinition {
                    name: "value",
                    direction: PinDirection::Output,
                    data_mode: zeughaus_core::DataMode::Value,
                    pin_kind: zeughaus_core::PinKind::Sample,
                    type_name: "f64",
                }],
            },
        );

        nodes.insert(
            n2,
            EditorNode {
                id: n2,
                type_id: "transform.add".to_string(),
                display_name: "Add".to_string(),
                position: Point::new(300.0, 200.0),
                pin_defs: vec![
                    PinDefinition {
                        name: "a",
                        direction: PinDirection::Input,
                        data_mode: zeughaus_core::DataMode::Value,
                        pin_kind: zeughaus_core::PinKind::Trigger,
                        type_name: "f64",
                    },
                    PinDefinition {
                        name: "b",
                        direction: PinDirection::Input,
                        data_mode: zeughaus_core::DataMode::Value,
                        pin_kind: zeughaus_core::PinKind::Trigger,
                        type_name: "f64",
                    },
                    PinDefinition {
                        name: "result",
                        direction: PinDirection::Output,
                        data_mode: zeughaus_core::DataMode::Value,
                        pin_kind: zeughaus_core::PinKind::Sample,
                        type_name: "f64",
                    },
                ],
            },
        );

        nodes.insert(
            n3,
            EditorNode {
                id: n3,
                type_id: "transform.display".to_string(),
                display_name: "Display".to_string(),
                position: Point::new(550.0, 200.0),
                pin_defs: vec![PinDefinition {
                    name: "input",
                    direction: PinDirection::Input,
                    data_mode: zeughaus_core::DataMode::Value,
                    pin_kind: zeughaus_core::PinKind::Trigger,
                    type_name: "any",
                }],
            },
        );

        node_order.extend([n1, n2, n3]);

        let app = Self {
            nodes,
            node_order,
            edges: Vec::new(),
            selected: HashSet::new(),
            camera_position: Point::ORIGIN,
            camera_zoom: 1.0,
        };

        (app, Task::none())
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::EdgeConnected { from, to } => {
                // Convert u64 back to NodeId
                let from_node = NodeId(from.node_id);
                let to_node = NodeId(to.node_id);
                self.edges.push(EditorEdge {
                    from_node,
                    from_pin: from.pin_id,
                    to_node,
                    to_pin: to.pin_id,
                });
            }
            Message::EdgeDisconnected { from, to } => {
                let from_node = NodeId(from.node_id);
                let to_node = NodeId(to.node_id);
                self.edges.retain(|e| {
                    !(e.from_node == from_node
                        && e.from_pin == from.pin_id
                        && e.to_node == to_node
                        && e.to_pin == to.pin_id)
                });
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
                    self.edges
                        .retain(|e| e.from_node != id && e.to_node != id);
                }
            }
            Message::CameraChanged { position, zoom } => {
                self.camera_position = position;
                self.camera_zoom = zoom;
            }
        }
        Task::none()
    }

    pub fn view(&self) -> Element<'_, Message> {
        // Use u64 as the NodeGraph ID type (avoids orphan rule issues)
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
                let content = self.build_node_element(node);
                // Convert NodeId to u64 for the graph widget
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

    fn build_node_element<'a>(&self, node: &EditorNode) -> Element<'a, Message, Theme> {
        let theme = Theme::Dark;
        let style = NodeContentStyle::process(&theme);

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

        let body: Element<'_, Message, Theme> = iced::widget::column(pins).spacing(4).into();

        simple_node(&node.display_name, style, body)
    }

    pub fn theme(&self) -> Theme {
        Theme::Dark
    }
}
