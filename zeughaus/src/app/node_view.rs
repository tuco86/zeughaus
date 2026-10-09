//! What one node draws: its body, its settings, its pins, and the value or
//! frame it shows.

use std::collections::{HashMap, HashSet};
use std::f32::consts::FRAC_1_SQRT_2;
use std::sync::Arc;

use iced::theme::palette::mix;
use iced::widget::text::Wrapping;
use iced::widget::{button, canvas, column, container, image, pick_list, row, text, text_input};
use iced::{Alignment, Color, ContentFit, Element, Length, Padding, Rectangle, Vector, mouse};
use iced_nodegraph::{PinDirection as NgPinDirection, PinShape, PinSide, node_header, node_pin};
use zeughaus_core::{
    Image, NodeId, PinDirection, PinKind, SettingDef, SettingKind, Ty, Value, field_rows,
};
use zeughaus_theme::{Ansi, Theme};

use super::graph::EditorNode;
use super::{App, RENAME_INPUT, danger_on_hover};
use crate::message::{Message, PinLabel};

/// What a node shows inline: either the value rendered as text, or a decoded
/// image frame.
///
/// The image variant caches the `image::Handle` next to the pixels it was built
/// from. `Handle::from_rgba` mints a fresh id on every call, and a new id means
/// a new GPU upload -- for a 4K capture that is 33 MB per frame. Rebuilding the
/// handle only when the pixel buffer actually changed (pointer equality on the
/// shared frame) keeps a still frame at zero upload cost across redraws.
pub(super) enum DisplayValue {
    Text(String),
    /// A frame only ever comes from a feed, and the browser editor has no
    /// transport to one, so nothing builds this there.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    Frame {
        pixels: Arc<[u8]>,
        handle: image::Handle,
    },
}

impl App {
    /// What one node shows inline: a value the runtime reported on one of its
    /// own output pins, what it receives when it reported none, or the frame
    /// its feed delivered.
    ///
    /// A node shows what it produced; a sink (no outgoing edges) shows what it
    /// received, which is what makes the Display node work. Only scalars
    /// travel as values ([`zeughaus_core::wire`]), so a frame arrives on the
    /// feed alone.
    pub(super) fn display_for(&self, node_id: NodeId) -> Option<DisplayValue> {
        // A frame from a feed outranks a reported value, because for an image
        // pin there is none: pixels never travel through the graph link or the
        // event stream, so the feed is the only thing that has the frame.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(frame) = self.feed_frame(node_id) {
            return Some(self.frame_display(node_id, frame));
        }
        let edges = self.edge_index.get(&node_id)?;
        let produced = edges
            .outgoing
            .iter()
            .filter_map(|index| self.edges.get(*index))
            .find_map(|edge| self.output_value(node_id, edge.from_pin.as_str()));
        let received = || {
            edges
                .incoming
                .iter()
                .filter_map(|index| self.edges.get(*index))
                .find_map(|edge| self.output_value(edge.from_node, edge.from_pin.as_str()))
        };
        let value = produced.or_else(received)?;
        Some(DisplayValue::Text(value.to_string()))
    }

    /// The value the runtime last reported on one of a node's output pins.
    pub(super) fn output_value(&self, node: NodeId, pin: &str) -> Option<&Value> {
        self.runtime.remote_outputs.get(&node)?.get(pin)
    }

    /// Recomputes what every node shows.
    ///
    /// For the paths that change the whole picture at once: a graph loaded, a
    /// snapshot adopted, a runtime gone. A single delivered value uses
    /// [`Self::update_displays_from`] instead.
    pub(super) fn update_display_values(&mut self) {
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
    pub(super) fn update_displays_from(&mut self, node: NodeId) {
        let mut touched: Vec<NodeId> = vec![node];
        if let Some(edges) = self.edge_index.get(&node) {
            touched.extend(
                edges
                    .outgoing
                    .iter()
                    .filter_map(|index| self.edges.get(*index))
                    .map(|edge| edge.to_node),
            );
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
    ///
    /// Only the feed produces frames, so the browser editor never calls this.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub(super) fn frame_display(&self, node_id: NodeId, frame: &Image) -> DisplayValue {
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
    /// output the runtime reported no value for.
    ///
    /// "Did the last run produce it", not "did anything ever cross this wire":
    /// that is the distinction the user needs to see -- an `error` pin that
    /// stays dim means nothing failed.
    pub(super) fn dim_mask(&self, id: NodeId, node: &EditorNode) -> u64 {
        let produced = self.runtime.remote_outputs.get(&id);
        node.pin_defs
            .iter()
            .take(64)
            .enumerate()
            .filter(|(_, p)| p.direction == PinDirection::Output)
            .filter(|(_, p)| produced.is_none_or(|pins| !pins.contains_key(&*p.name)))
            .fold(0u64, |mask, (i, _)| mask | 1 << i)
    }
}

/// Default content size of a Display node before the user resizes it. Wide
/// enough that a downscaled 16:9 frame is recognizable.
pub(super) const DISPLAY_SIZE: iced::Size = iced::Size::new(240.0, 150.0);

/// Width of every other node. Their content is text and pin rows, which do not
/// benefit from being resizable.
const NODE_WIDTH: f32 = 180.0;

/// Width of a node whose body holds a field row editor: a name field, a type
/// choice and a remove button side by side need more than a pin label does.
const FIELDS_NODE_WIDTH: f32 = 260.0;

/// The Display node: the one node type whose body shows data rather than pin
/// rows. That makes it the only one worth resizing, and the only one that asks
/// the runtime for a video feed.
pub(super) fn is_display(type_id: &str) -> bool {
    type_id == "transform.display"
}

/// Halves a color's brightness, marking a pin or edge that currently carries no
/// value. Scaling the channels (rather than the alpha) keeps the hue readable
/// against both the canvas and a node body.
pub(super) fn dim(c: Color) -> Color {
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
    theme: &Theme,
    errors: Option<&'a HashMap<String, String>>,
    def: &'a SettingDef,
) -> iced::widget::Text<'a, Theme> {
    let message = errors
        .and_then(|e| e.get(&*def.name))
        .map(String::as_str)
        .unwrap_or("");
    text(message).size(10).color(theme.error())
}

/// Everything the widget needs about one node besides the node itself: what it
/// shows, what has been typed into it, and what the node said about that.
pub(super) struct NodeChrome<'a> {
    pub display: Option<&'a DisplayValue>,
    pub settings: Option<&'a HashMap<String, String>>,
    /// What the node refused, per setting key.
    pub errors: Option<&'a HashMap<String, String>>,
    /// Why the node's last run failed, as the runtime reported it.
    pub failure: Option<&'a str>,
    pub dim_mask: u64,
    pub size: Option<iced::Size>,
    pub is_container: bool,
    /// The draft name while this node is being renamed; `None` shows the name.
    pub rename: Option<&'a str>,
}

pub(super) fn build_node_element<'a>(
    theme: &Theme,
    node: &'a EditorNode,
    chrome: NodeChrome<'a>,
) -> Element<'a, Message, Theme> {
    let NodeChrome {
        display,
        settings,
        errors,
        failure,
        dim_mask,
        size,
        is_container,
        rename,
    } = chrome;
    let is_button = node.type_id == "flow.button";
    let is_display = is_display(&node.type_id);

    let mut items: Vec<Element<'_, Message, Theme>> = Vec::new();

    // A container's body is a way in: its contents open as a tab. Its pins
    // come from the boundary nodes inside it, so without this the node would
    // be a box with no purpose.
    if is_container {
        items.push(
            button(text("open").size(11))
                .padding(2.0)
                .on_press(Message::OpenGraph(node.id.0))
                .into(),
        );
    }

    if is_button {
        // The button IS the pin content: pressing it is the event this node
        // exists to produce, so there is nothing else worth showing.
        let node_raw_id = node.id.0;
        let first_pin = node.pin_defs.first();
        let tint =
            first_pin.map_or_else(|| pin_color(theme, &Ty::Bool), |p| pin_color(theme, &p.ty));
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
                .color(theme.ansi(Ansi::Yellow))
                .into(),
            None => text("").size(14).into(),
        };
        let pin_def = node.pin_defs.first();
        let name = pin_def.map_or_else(|| Arc::from("input"), |p| p.name.clone());
        let visual = PinVisual {
            color: pin_def.map_or_else(|| pin_color(theme, &Ty::Any), |p| pin_color(theme, &p.ty)),
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
            items.push(title_setting(
                theme,
                node,
                def,
                setting_value(settings, def),
            ));
            items.push(setting_refusal(theme, errors, def).into());
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
            items.extend(field_setting(theme, node, def, types, current, dim_mask));
            items.push(setting_refusal(theme, errors, def).into());
        }

        for (index, pin_def) in node.pin_defs.iter().enumerate() {
            if fields.contains(&*pin_def.name) {
                continue;
            }
            let (side, direction) = pin_geometry(pin_def.direction);

            let tint = pin_color(theme, &pin_def.ty);
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
                text(&*def.name).size(11).color(theme.muted()),
                field,
                setting_refusal(theme, errors, def)
            ]
            .spacing(1)
            .into(),
        );
    }

    // Always render a value display row to keep widget tree structure stable.
    // Empty text when no value -- prevents iced widget state downcast panics
    // caused by children count changing between view() calls. The Display and
    // Button nodes are exempt: their body already IS the value or the control.
    if !is_display && !is_button {
        let value_text = match display {
            Some(DisplayValue::Text(val)) => val.as_str(),
            // A frame has no text form; the Display node is where it is shown.
            Some(DisplayValue::Frame { .. }) => "frame",
            None => "",
        };
        let prefix = if value_text.is_empty() { "" } else { "= " };
        items.push(
            row![
                text(prefix).size(12).color(theme.muted()),
                text(value_text).size(13).color(theme.ansi(Ansi::Yellow))
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
            .color(theme.error())
            .into(),
    );

    let body = column(items).spacing(4);
    let title: Element<'_, Message, Theme> = match rename {
        Some(draft) => text_input("", draft)
            .id(RENAME_INPUT)
            .on_input(Message::RenameInput)
            .on_submit(Message::RenameCommit)
            .size(14)
            .padding([1, 4])
            .width(Length::Fill)
            .into(),
        None => container(
            text(node.display_name.as_str())
                .size(14)
                .color(theme.extended().background.weak.text)
                .wrapping(Wrapping::None),
        )
        .width(Length::Fill)
        .clip(true)
        .into(),
    };
    // The pencil commits an open rename rather than restarting it, so the
    // button that opened the field is also the one that closes it.
    let edit = round_button(
        Icon::Pencil,
        if rename.is_some() {
            Message::RenameCommit
        } else {
            Message::RenameStart(node.id.0)
        },
        button::secondary,
    );
    // This node alone, not the selection: the button sits on one node.
    let delete = round_button(
        Icon::Close,
        Message::DeleteNodes(vec![node.id.0]),
        |theme, status| danger_on_hover(theme, status, button::secondary),
    );
    // Left padding clears the 8 px corner the header is rounded with.
    let header = node_header(
        row![title, edit, delete]
            .spacing(4)
            .align_y(Alignment::Center),
        header_color(theme, &node.category),
        8.0,
    )
    .padding(Padding {
        top: 3.0,
        bottom: 3.0,
        left: 10.0,
        right: 5.0,
    });
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

/// A [`SettingKind::Title`] setting: what the node is called, editable in
/// place, under the same small grey label every other text setting carries.
///
/// The label is what tells the two names at the top of a `db.table` apart: the
/// header is the node type ("Table"), this is the table's own name. Without it
/// the field read as a second, unexplained heading.
fn title_setting<'a>(
    theme: &Theme,
    node: &'a EditorNode,
    def: &'a SettingDef,
    current: &'a str,
) -> Element<'a, Message, Theme> {
    let node_raw_id = node.id.0;
    let key = def.name.clone();
    let field = text_input(&def.placeholder, current)
        .on_input(move |v| Message::NodeSettingChanged {
            node_id: node_raw_id,
            key: key.to_string(),
            value: v,
        })
        .size(14)
        .width(Length::Fill);
    column![text(&*def.name).size(11).color(theme.muted()), field]
        .spacing(1)
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
/// `name:type` text that the runner and the node's own parser
/// already speak, and this editor needs to know nothing about what the fields
/// mean.
fn field_setting<'a>(
    theme: &Theme,
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
        let tint = pin.map_or_else(
            || pin_color(theme, &Ty::Any),
            |(_, p)| pin_color(theme, &p.ty),
        );
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

/// Header background color by the node's catalog category: the theme's green
/// where values enter the graph, its yellow where they are read out, its own
/// weak background for every step in between. Three colours rather than one
/// per category, because the only distinction worth a glance across a whole
/// graph is where the data comes from and where it ends up.
fn header_color(theme: &Theme, category: &str) -> Color {
    let base = theme.extended().background.weak.color;
    match category {
        "Const" => mix(base, theme.ansi(Ansi::Green), 0.35),
        "Output" => mix(base, theme.ansi(Ansi::Yellow), 0.35),
        _ => base,
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
    pub(super) color: Color,
    pub(super) shape: PinShape,
}

/// Pin shape encodes the transmission mode: Event (Trigger) pins are squares,
/// State (Sample) pins are circles. `PinShape` offers exactly these two.
fn pin_shape(kind: PinKind) -> PinShape {
    match kind {
        PinKind::Trigger => PinShape::Square,
        PinKind::Sample => PinShape::Circle,
    }
}

/// Pin color per payload type, drawn from the theme's ANSI slots: the same
/// sixteen colours the terminals in the other panes use, so a graph and a
/// shell in one window are one palette. Opaque and composite types share the
/// neutral white rather than a generated hue.
pub(super) fn pin_color(theme: &Theme, ty: &Ty) -> Color {
    theme.ansi(match ty {
        Ty::Float => Ansi::Green,
        Ty::Str => Ansi::Yellow,
        Ty::Bool => Ansi::Blue,
        Ty::Int => Ansi::Cyan,
        Ty::Any => Ansi::BrightWhite,
        Ty::List(_) | Ty::Option(_) | Ty::Record(_) | Ty::Opaque(_) => Ansi::White,
    })
}

/// A round header button: 18 px across, its symbol centred, the given iced
/// button style with the corners rounded to a circle.
fn round_button(
    icon: Icon,
    message: Message,
    style: fn(&iced::Theme, button::Status) -> button::Style,
) -> Element<'static, Message, Theme> {
    button(
        canvas(IconProgram { icon, style })
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .width(18)
    .height(18)
    .padding(0)
    .on_press(message)
    .style(move |theme: &Theme, status| button::Style {
        border: iced::border::rounded(9),
        ..style(theme.base(), status)
    })
    .into()
}

/// What a round header button shows.
#[derive(Clone, Copy)]
enum Icon {
    Pencil,
    Close,
}

/// A header button's symbol, drawn as paths around the centre of its button.
/// A glyph sits where its font's metrics put it, which in an 18 px circle is
/// visibly off centre, and off by a different amount in every font.
struct IconProgram {
    icon: Icon,
    /// The style of the button around the symbol, which decides its colour.
    style: fn(&iced::Theme, button::Status) -> button::Style,
}

impl canvas::Program<Message, Theme> for IconProgram {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &iced::Renderer,
        theme: &Theme,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        // The button's status does not reach its content; the cursor does.
        // Hovering is the only status whose colours differ here, and a press
        // happens under the cursor.
        let status = if cursor.is_over(bounds) {
            button::Status::Hovered
        } else {
            button::Status::Active
        };
        let color = (self.style)(theme.base(), status).text_color;
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let c = frame.center();
        match self.icon {
            Icon::Close => {
                const ARM: f32 = 3.5;
                let cross = canvas::Path::new(|p| {
                    p.move_to(c + Vector::new(-ARM, -ARM));
                    p.line_to(c + Vector::new(ARM, ARM));
                    p.move_to(c + Vector::new(-ARM, ARM));
                    p.line_to(c + Vector::new(ARM, -ARM));
                });
                frame.stroke(
                    &cross,
                    canvas::Stroke::default()
                        .with_color(color)
                        .with_width(1.6)
                        .with_line_cap(canvas::LineCap::Round),
                );
            }
            Icon::Pencil => {
                // A point `along` the pencil, which points to the top right,
                // and `across` it. The area's centroid sits on the centre, not
                // the bounding box: the body outweighs the tip, and a solid
                // shape centred by its box looks pushed towards its body.
                // Body 6.4 x 2.8 around 0.8, tip 2.4 long around -4.0.
                let at = |along: f32, across: f32| {
                    c + Vector::new(along + across, across - along) * FRAC_1_SQRT_2
                };
                let pencil = canvas::Path::new(|p| {
                    p.move_to(at(-2.4, -1.4));
                    p.line_to(at(4.0, -1.4));
                    p.line_to(at(4.0, 1.4));
                    p.line_to(at(-2.4, 1.4));
                    p.close();
                    // The tip, set off from the body by a hairline gap.
                    p.move_to(at(-3.2, -1.4));
                    p.line_to(at(-5.6, 0.0));
                    p.line_to(at(-3.2, 1.4));
                    p.close();
                });
                frame.fill(&pencil, color);
            }
        }
        vec![frame.into_geometry()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The row editor's whole contract: every edit is the setting's full text
    /// again, so the runner and the node's parser keep speaking
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
}
