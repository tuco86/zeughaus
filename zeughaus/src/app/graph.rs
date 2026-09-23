//! The editor's model of the graph: the nodes and edges on screen, the node
//! instance behind each node, and the rules a wire has to pass before it is
//! drawn.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use iced::{Point, Vector};
use iced_nodegraph::PinRef;
use zeughaus_core::{
    EdgeData, EdgeId, GraphDocument, NodeData, NodeId, PinBinding, PinDefinition, PinDirection,
    PinKind, SettingDef, SettingKind, Ty, TypeConverters, Value, renamed_field,
};

use super::App;
#[cfg(not(target_arch = "wasm32"))]
use super::store::edge_data;
use crate::message::{GraphIds, PinLabel};

pub struct EditorNode {
    pub id: NodeId,
    pub type_id: String,
    pub display_name: String,
    /// What the catalog files this node type under, read off it once when the
    /// node is inserted: it decides the header colour, and `view` asks per
    /// drawn node per frame.
    pub category: Arc<str>,
    pub position: Point,
    pub pin_defs: Vec<PinDefinition>,
    pub settings: Vec<SettingDef>,
    /// The container node this node lives inside, `NodeId(0)` for the root
    /// graph. Only the editor knows about the nesting: an edge always names
    /// the real nodes at its ends.
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

/// The edges on one node, by direction, as positions in [`App::edges`].
///
/// The index behind [`App::edge_index`]: what a node shows is read from the
/// values on the edges it touches, and answering that by scanning every edge
/// twice per node is O(nodes x edges) at the rate a capture graph delivers.
/// Positions rather than ids because the edge itself -- its pins -- is what
/// the lookup needs; [`App::reindex_edges`] rebuilds them whenever the edge
/// set changes.
#[derive(Default)]
pub(super) struct NodeEdges {
    pub outgoing: Vec<usize>,
    pub incoming: Vec<usize>,
    /// The nodes this one feeds through a *dataflow* edge.
    ///
    /// Relations are absent on purpose: two tables referencing each other is a
    /// legal schema, and this list is what answers "would this wire close a
    /// cycle".
    pub flow_out: Vec<NodeId>,
}

impl App {
    /// The nodes directly inside `parent`, in creation order.
    ///
    /// Order matters: it decides which boundary wins a duplicate pin name, and
    /// `node_order` is the only stable order the editor has.
    pub(super) fn children(&self, parent: NodeId) -> impl Iterator<Item = &EditorNode> {
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
    pub(super) fn descendants(&self, parent: NodeId) -> Vec<NodeId> {
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
    pub(super) fn is_container(&self, type_id: &str) -> bool {
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
    pub(super) fn boundary_name(&self, child: NodeId) -> String {
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
    pub(super) fn refresh_container_pins(&mut self, container: NodeId) {
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
            node.pin_defs = pins;
        }
    }

    /// The real node an edge endpoint has to name in the store.
    ///
    /// A wire dropped on a container's pin belongs to the boundary node behind
    /// that pin: edges always connect real nodes, so nothing outside the
    /// editor has to know a container exists. `None` when the pin names no
    /// boundary, which is a wire that cannot be stored and is therefore
    /// dropped.
    pub(super) fn resolve_boundary(
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
    pub(super) fn view_endpoint(
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
    pub(super) fn refresh_boundary_owner(&mut self, node: NodeId) {
        let Some(parent) = self.nodes.get(&node).map(|n| n.parent) else {
            return;
        };
        self.refresh_container_pins(parent);
    }

    /// Creates a node of `type_id` in the graph being edited, at `position` if
    /// nothing is there and stepped clear of what is.
    ///
    /// The palette hands out one position for every node it spawns, so without
    /// this the second node lands exactly on the first: hidden behind it, with
    /// its header under the other node's body, and the only way to find out is
    /// to drag the top one away.
    pub(super) fn spawn_node(&mut self, type_id: &str, position: Point) {
        let position = self.clear_spot(position);
        self.spawn_node_into(type_id, position, self.current_graph);
    }

    /// `position`, or the first spot down-right of it that no node of the
    /// graph being edited already starts at.
    ///
    /// Compares the top-left corners only: a node's drawn size is the widget's
    /// business and depends on its content, while what makes a new node
    /// unfindable is another node's header sitting on top of its own.
    pub(super) fn clear_spot(&self, position: Point) -> Point {
        /// One header height plus a little, so a stepped node's title is
        /// readable next to the one it stepped around.
        const STEP: f32 = 32.0;
        /// Closer than this counts as the same spot.
        const TAKEN: f32 = 24.0;

        let mut spot = position;
        // Bounded: a graph with a long diagonal of nodes stops stepping rather
        // than walking the new node off the far edge of the world.
        for _ in 0..16 {
            let taken = self.nodes.values().any(|n| {
                n.parent == self.current_graph
                    && (n.position.x - spot.x).abs() < TAKEN
                    && (n.position.y - spot.y).abs() < TAKEN
            });
            if !taken {
                break;
            }
            spot = Point::new(spot.x + STEP, spot.y + STEP);
        }
        spot
    }

    /// Creates a node of `type_id` inside `parent`, seeded with the defaults
    /// its type declares. `None` when no plugin in this build provides the
    /// type.
    ///
    /// Separate from [`Self::spawn_node`] because a clone decides both the
    /// parent and the exact position: a copied subtree's children belong to
    /// the copied container, and staggering them would move them inside it.
    pub(super) fn spawn_node_into(
        &mut self,
        type_id: &str,
        position: Point,
        parent: NodeId,
    ) -> Option<NodeId> {
        let instance = self.plugins.iter().find_map(|p| p.create_node(type_id))?;

        let pin_defs = instance.pin_definitions().to_vec();
        let setting_defs = instance.settings();
        let defaults: Vec<(Arc<str>, String)> = setting_defs
            .iter()
            .map(|def| (Arc::clone(&def.name), def.default.to_string()))
            .collect();
        let id = NodeId::next();
        let display_name = self
            .catalog
            .iter()
            .find(|d| &*d.type_id == type_id)
            .map(|d| d.display_name.to_string())
            .unwrap_or_else(|| type_id.to_string());
        self.instances.insert(id, instance);

        self.nodes.insert(
            id,
            EditorNode {
                id,
                type_id: type_id.to_string(),
                display_name,
                category: self.category(type_id),
                position,
                pin_defs,
                settings: setting_defs,
                parent,
                is_container: self.is_container(type_id),
            },
        );
        self.node_order.push(id);

        // Seed every setting from its default, so what the widget shows is
        // what the node holds. A node that refuses its own default says so on
        // the node rather than nowhere.
        for (key, default) in defaults {
            self.apply_setting(id, &key, default);
        }

        // A boundary node spawned inside a container is a new pin on it.
        self.refresh_container_pins(parent);
        #[cfg(not(target_arch = "wasm32"))]
        self.derive_db_params(id);
        #[cfg(not(target_arch = "wasm32"))]
        self.push_node(id);
        Some(id)
    }

    /// What the catalog files a node type under, which is what the header
    /// colour follows. Empty for a type this build has no entry for.
    pub(super) fn category(&self, type_id: &str) -> Arc<str> {
        self.catalog
            .iter()
            .find(|d| &*d.type_id == type_id)
            .map_or_else(|| Arc::from(""), |d| Arc::clone(&d.category))
    }

    /// Copies a node, what has been typed into it, and -- for a container --
    /// everything inside it. Returns the copy of `root`.
    ///
    /// A copy is the original: same settings, same contents. One that comes up
    /// with default settings, or a copied container that comes up empty, looks
    /// like the original and behaves differently -- which is worse than either
    /// copying properly or refusing.
    ///
    /// Ids are remapped, so a wire between two copied nodes joins the copies.
    /// Relations are ordinary edges here and are copied like the rest. An edge
    /// with only one end inside the copied subtree is NOT copied: a second
    /// wire onto a single-slot input would be refused anyway, and for a
    /// relation, guessing which of the two schemas the user meant to reference
    /// is worse than leaving the wire to be drawn.
    pub(super) fn clone_subtree(&mut self, root: NodeId, offset: Vector) -> Option<NodeId> {
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

            if let Some(settings) = self.node_settings.get(&original).cloned() {
                for (key, value) in settings {
                    self.apply_setting(copy, &key, value);
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
        // settings, and `spawn_node_into` builds them while every copied
        // boundary still holds its default name: two copied `graph.input`s
        // would collapse into one pin, and the container's pins would carry
        // default names while `boundary_name` reports the copied ones, so
        // `resolve_boundary` would match nothing and a wire dropped on the
        // clone's pin would go nowhere.
        for copy in &copies {
            if self.nodes.get(copy).is_some_and(|n| n.is_container) {
                self.refresh_container_pins(*copy);
            }
        }

        // The derived parameters say where the copy sits and what is wired to
        // it; they are not the original's to inherit. Derived last, when the
        // copied edges exist and the answer is knowable: the settings copied
        // above carry the original's `relations` line, which would have the
        // runner create a foreign key for a wire this graph does not show.
        #[cfg(not(target_arch = "wasm32"))]
        for copy in &copies {
            self.derive_db_params(*copy);
            self.derive_db_dependents(*copy);
        }

        copy_of.get(&root).copied()
    }

    pub(super) fn connect_edge(
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
        self.edges.push(EditorEdge {
            id: edge_id,
            from_node,
            from_pin,
            to_node,
            to_pin,
        });
        self.reindex_edges();
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
        // The source's last reported value is already held, so the new wire
        // shows it at the next frame instead of staying blank until the
        // runtime publishes again.
        self.update_display_values();
    }

    /// After a node's connections change, recompute its pins if it is variadic
    /// (e.g. a merge node grows an input as the last one fills) and mirror the
    /// pin set it now declares, so the added or removed pin is drawn
    /// immediately.
    ///
    /// What each input is offered is the source pin's declared type: no value
    /// is held per wire here, because nothing is executed here.
    pub(super) fn resync_pins(&mut self, node: NodeId) {
        let bindings: Vec<(Arc<str>, Ty)> = self
            .edges
            .iter()
            .filter(|edge| edge.to_node == node)
            .map(|edge| {
                let ty = self
                    .pin_def(edge.from_node, edge.from_pin.as_str())
                    .map_or(Ty::Any, |pin| pin.ty.clone());
                (Arc::clone(&edge.to_pin.0), ty)
            })
            .collect();
        let connected: Vec<PinBinding<'_>> = bindings
            .iter()
            .map(|(name, ty)| PinBinding { name, ty })
            .collect();
        let Some(instance) = self.instances.get_mut(&node) else {
            return;
        };
        if !instance.sync_pins(&connected) {
            return;
        }
        let pins = instance.pin_definitions().to_vec();
        if let Some(en) = self.nodes.get_mut(&node) {
            en.pin_defs = pins;
        }
    }

    /// Re-reads a node's own pin declaration after its parameters changed, and
    /// mirrors it into the editor's node.
    ///
    /// Distinct from [`App::resync_pins`], which grows a variadic node from
    /// what is wired to it: this one picks up a pin set the node derived from a
    /// setting, where nothing is connected yet.
    pub(super) fn refresh_node_pins(&mut self, node: NodeId) {
        let Some(pins) = self
            .instances
            .get(&node)
            .map(|instance| instance.pin_definitions().to_vec())
        else {
            return;
        };
        if let Some(en) = self.nodes.get_mut(&node)
            && en.pin_defs != pins
        {
            en.pin_defs = pins;
        }
    }

    pub(super) fn disconnect_edge(
        &mut self,
        from_node: NodeId,
        from_pin: PinLabel,
        to_node: NodeId,
        to_pin: PinLabel,
    ) {
        let wire = self
            .edges
            .iter()
            .find(|e| {
                e.from_node == from_node
                    && e.from_pin == from_pin
                    && e.to_node == to_node
                    && e.to_pin == to_pin
            })
            .map(|e| e.id);
        if let Some(wire) = wire {
            // Also drops the particle queue that was riding it: without that
            // every local rewiring leaks a queue nothing will ever draw again.
            self.forget_edge(wire);
            self.resync_pins(to_node);
            #[cfg(not(target_arch = "wasm32"))]
            self.push_edge_remove(wire);
            // A table pin that lost its wire is a column list that no longer
            // applies, and a field that lost one is a foreign key that is
            // gone -- from whichever end declared it.
            #[cfg(not(target_arch = "wasm32"))]
            self.derive_db_params(to_node);
            #[cfg(not(target_arch = "wasm32"))]
            self.derive_db_params(from_node);
            self.update_display_values();
        }
    }

    /// A node's setting value, or the default its type declares.
    pub(super) fn setting_or_default(&self, node: NodeId, key: &str) -> String {
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
    pub(super) fn is_field_setting(&self, node: NodeId, key: &str) -> bool {
        self.nodes.get(&node).is_some_and(|n| {
            n.settings
                .iter()
                .any(|s| &*s.name == key && matches!(s.kind, SettingKind::Fields { .. }))
        })
    }

    /// Applies one setting's text to the node that owns it: the text this
    /// window shows, what the node makes of it, and the pin set it now
    /// declares.
    ///
    /// The one path from a setting's text to a node, taken by a spawn default,
    /// a keystroke, a row arriving from the store and a loaded document alike.
    /// Every parameter is text -- the node is the only thing that knows what
    /// its settings mean -- so a value is never parsed on the way in.
    ///
    /// Immediate on purpose. What the user typed has to be on screen at the
    /// next frame; what the *store* learns is a separate question, answered by
    /// [`App::commit_node`] once the typing stops.
    ///
    /// The node's refusal is kept and drawn under the field, so a rejected
    /// value does not sit there looking accepted while the node keeps the old
    /// one.
    pub(super) fn apply_setting(&mut self, node: NodeId, key: &str, text: String) {
        self.node_settings
            .entry(node)
            .or_default()
            .insert(key.to_string(), text.clone());
        let refusal = self
            .instances
            .get_mut(&node)
            .and_then(|instance| instance.set_parameter(key, Value::new(text)).err());
        self.record_setting_error(node, key, refusal);
        // A setting can decide the node's pins (a table's columns are one), so
        // the widget re-reads what it now declares.
        self.refresh_node_pins(node);
    }

    /// Remembers, or clears, what a node said about one of its settings.
    pub(super) fn record_setting_error(
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
    pub(super) fn settle_relations(&mut self, node: NodeId, key: &str, was: &str) {
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

    /// Drops an edge from this window: the view, the index over it, and the
    /// particle queue that was riding it.
    pub(super) fn forget_edge(&mut self, id: EdgeId) {
        self.edges.retain(|e| e.id != id);
        self.reindex_edges();
        #[cfg(not(target_arch = "wasm32"))]
        self.runtime.particles.remove(&id);
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
    pub(super) fn rename_pin_edges(&mut self, node: NodeId, old: &str, new: &str) {
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
            self.edges.push(EditorEdge {
                id: fresh,
                from_node,
                from_pin,
                to_node,
                to_pin,
            });
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
    pub(super) fn pin_def(&self, node: NodeId, pin: &str) -> Option<&PinDefinition> {
        self.nodes
            .get(&node)?
            .pin_defs
            .iter()
            .find(|p| &*p.name == pin)
    }

    /// Why the wire between these two pins was turned down, in one sentence,
    /// or `None` if these rules would have allowed it after all.
    ///
    /// The same [`wire_refusal`] `can_connect` asks, so the sentence names the
    /// rule that actually refused the drop. What differs is where the two get
    /// their facts: the widget hands `can_connect` the occupancy it knows, and
    /// a refused drop arrives as bare ids, so occupancy is read off the edges
    /// this window holds.
    pub(super) fn refusal_sentence(
        &self,
        from: &PinRef<GraphIds>,
        to: &PinRef<GraphIds>,
    ) -> Option<String> {
        let from_id = NodeId(from.node_id);
        let to_id = NodeId(to.node_id);
        let closes_cycle = |from_is_output: bool| {
            let (source, target) = if from_is_output {
                (from_id, to_id)
            } else {
                (to_id, from_id)
            };
            flow_reaches(&self.edge_index, target, source)
        };
        wire_refusal(&Wire {
            same_node: from_id == to_id,
            from: self.pin_def(from_id, from.pin_id.as_str()),
            to: self.pin_def(to_id, to.pin_id.as_str()),
            from_free: !self.input_taken(from_id, from.pin_id.as_str()),
            to_free: !self.input_taken(to_id, to.pin_id.as_str()),
            closes_cycle: &closes_cycle,
            converters: &self.converters,
        })
    }

    /// Whether this pin is an input that already carries a wire.
    pub(super) fn input_taken(&self, node: NodeId, pin: &str) -> bool {
        self.pin_def(node, pin)
            .is_some_and(|p| p.direction == PinDirection::Input)
            && self
                .edges
                .iter()
                .any(|e| e.to_node == node && e.to_pin.as_str() == pin)
    }

    /// Whether a node declares this pin as a bidirectional field pin.
    ///
    /// Read off the pin declaration, never off a node type: what makes an edge
    /// a relation is the same fact here, in the runtime and in the runner.
    pub(super) fn is_field_pin(&self, node: NodeId, pin: &str) -> bool {
        self.pin_def(node, pin)
            .is_some_and(|p| p.direction == PinDirection::Both)
    }

    /// Whether an edge is a relation: both its ends are field pins.
    ///
    /// Only the derivation of the `relations` parameter asks, and that reaches
    /// the runner through the store -- which the browser editor has no path to.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn is_relation(&self, edge: &EditorEdge) -> bool {
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
    /// reading "not a field pin" as "gone" would delete every live wire into
    /// a table's field the moment any setting on the node is edited.
    ///
    /// A relation without its field is nothing: it would stay in the store,
    /// invisible in every view, and reattach itself if a field of that name
    /// ever came back. Renaming a field therefore drops its relations, which
    /// is the honest reading of "that field is gone".
    pub(super) fn drop_orphaned_relations(&mut self, node: NodeId) {
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

    /// Rebuilds [`Self::edge_index`] from the current edge set.
    ///
    /// Called from every place that adds or removes an edge. Rebuilding the
    /// whole thing is O(edges) and happens on a user action; the index exists
    /// to keep the O(nodes x edges) scan out of the path a value takes, which
    /// runs at frame rate.
    pub(super) fn reindex_edges(&mut self) {
        let mut index: HashMap<NodeId, NodeEdges> = HashMap::new();
        for (position, edge) in self.edges.iter().enumerate() {
            let from = edge.from_node;
            let to = edge.to_node;
            index.entry(from).or_default().outgoing.push(position);
            index.entry(to).or_default().incoming.push(position);
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

    /// The graph as a `.zgh` document, for an explicit Save. On wasm nothing
    /// writes one: the browser editor has no file dialog.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub(super) fn to_document(&self) -> GraphDocument {
        let nodes = self
            .node_order
            .iter()
            .filter_map(|id| {
                let node = self.nodes.get(id)?;
                let params: Vec<(String, String)> = self
                    .node_settings
                    .get(id)
                    .map(|settings| {
                        settings
                            .iter()
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
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
    /// params) and records it. Shared by document loading and remote sync
    /// apply. No-op on a duplicate id or an unknown node type.
    pub(super) fn insert_node_from_data(&mut self, node_data: &NodeData) {
        let type_id = &node_data.type_id;
        let Some(instance) = self.plugins.iter().find_map(|p| p.create_node(type_id)) else {
            return;
        };
        let id = NodeId(node_data.id);
        if self.nodes.contains_key(&id) {
            return;
        }
        let pin_defs = instance.pin_definitions().to_vec();
        let setting_defs = instance.settings();
        self.instances.insert(id, instance);

        self.nodes.insert(
            id,
            EditorNode {
                id,
                type_id: type_id.clone(),
                display_name: node_data.display_name.clone(),
                category: self.category(type_id),
                position: Point::new(node_data.x, node_data.y),
                pin_defs,
                settings: setting_defs,
                parent: NodeId(node_data.parent),
                is_container: self.is_container(type_id),
            },
        );
        self.node_order.push(id);

        // Every stored parameter reaches the node as the text it was stored
        // as, including the hidden ones this editor derives: what a parameter
        // means is the node's business, and a refusal belongs under the field.
        for (name, text) in &node_data.params {
            self.apply_setting(id, name, text.clone());
        }
    }

    pub(super) fn load_document(&mut self, doc: GraphDocument) {
        // Everything the outgoing document owned: the nodes, their instances,
        // what was typed into them and what they said about it.
        self.nodes.clear();
        self.node_order.clear();
        self.instances.clear();
        self.edges.clear();
        self.edge_index.clear();
        self.node_settings.clear();
        self.setting_errors.clear();
        self.display_values.clear();
        // Every feed was opened for the outgoing document's node ids; nothing
        // here is known to still be wanted, so they all stop. The reconcile that
        // follows the load reopens whatever the new document asks for.
        #[cfg(not(target_arch = "wasm32"))]
        self.runtime.feeds.clear();

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
            self.edges.push(EditorEdge {
                id: edge_id,
                from_node: NodeId(edge_data.from_node),
                from_pin: PinLabel(from_pin),
                to_node: NodeId(edge_data.to_node),
                to_pin: PinLabel(to_pin),
            });
        }
        self.reindex_edges();

        // Nothing is executed here: a loaded graph shows values once the
        // runtime publishes them.
        self.update_display_values();
    }
}

/// Whether `start` can reach `goal` by following dataflow edges of `index`.
///
/// What makes a wire a cycle: a wire from `goal` to `start` closes one exactly
/// when this is true. Only `flow_out` is followed, so a relation between two
/// table fields is not a path -- two tables referencing each other is a legal
/// schema, and the runtime already keeps such an edge out of execution.
///
/// The runner executes the acyclic part of a graph that has a cycle, so this
/// is not the last line of defence. It is the only place where refusing needs
/// no explanation: at the drop, nothing has happened yet.
pub(super) fn flow_reaches(
    index: &HashMap<NodeId, NodeEdges>,
    start: NodeId,
    goal: NodeId,
) -> bool {
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

/// Everything the connection rules read about one attempted wire.
///
/// The rules run twice: once while a cable is being dragged, to decide whether
/// the pin under the cursor is a target at all, and once when a drag ends over
/// a pin that was not ([`Message::ConnectRefused`]). They have to agree, so
/// both ask [`wire_refusal`] and this is what it is given.
pub struct Wire<'a> {
    /// Whether both ends are on the same node.
    pub same_node: bool,
    /// What each end's node declares for the pin, `None` when it declares no
    /// such pin any more.
    pub from: Option<&'a PinDefinition>,
    pub to: Option<&'a PinDefinition>,
    /// Whether each end, if it is a single-slot input, is still free. The
    /// widget excludes the cable being dragged, so re-routing a wire onto its
    /// own input is free.
    pub from_free: bool,
    pub to_free: bool,
    /// Whether the wire would close a dataflow loop. Asked with `true` when
    /// the `from` end is the output, because the answer depends on which way
    /// the value would travel.
    pub closes_cycle: &'a dyn Fn(bool) -> bool,
    /// Which output type reaches which input type, directly or by conversion.
    pub converters: &'a TypeConverters,
}

/// Why this wire is refused, or `None` when it is allowed.
///
/// One function for the verdict and the sentence: a refusal the user reads has
/// to be the reason the drop was actually turned down, and two copies of these
/// rules would drift the first time one of them changed.
pub fn wire_refusal(wire: &Wire<'_>) -> Option<String> {
    let (Some(from), Some(to)) = (wire.from, wire.to) else {
        return Some("that pin is no longer there".to_string());
    };
    let both_fields = from.direction == PinDirection::Both && to.direction == PinDirection::Both;
    if wire.same_node {
        return Some(if both_fields {
            "a table cannot reference itself".to_string()
        } else {
            "a node cannot feed itself".to_string()
        });
    }
    // Two field pins are a relation: it carries nothing, so there is no
    // direction to respect and no occupancy to check -- one primary key is
    // referenced by many. Only the type has to agree.
    if both_fields {
        return (from.ty != to.ty)
            .then(|| format!("{} and {} fields cannot be related", from.ty, to.ty));
    }
    // A field pin wired to a data pin: a value has nowhere to go on a pin that
    // is not an endpoint of flow.
    if from.direction == PinDirection::Both || to.direction == PinDirection::Both {
        return Some("a field only relates to another field".to_string());
    }
    let from_is_output = from.direction == PinDirection::Output;
    if from.direction == to.direction {
        return Some(if from_is_output {
            "two outputs cannot be wired together".to_string()
        } else {
            "two inputs cannot be wired together".to_string()
        });
    }
    if !wire.from_free || !wire.to_free {
        return Some("that input already has a wire".to_string());
    }
    if (wire.closes_cycle)(from_is_output) {
        return Some("that wire would close a loop".to_string());
    }
    let (out, into) = if from_is_output {
        (from, to)
    } else {
        (to, from)
    };
    (!wire.converters.compatible(&out.ty, &into.ty))
        .then(|| format!("nothing converts {} into {}", out.ty, into.ty))
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
/// A pin that is still declared, field or not, is not lost. Reading "not a
/// field pin" as "gone" would delete an ordinary output wired to a table's
/// field -- a live edge, gone from the shared store -- the moment any setting
/// on that node is touched.
fn is_lost_relation(own: Option<&PinDefinition>, other: Option<&PinDefinition>) -> bool {
    own.is_none() && other.is_some_and(|p| p.direction == PinDirection::Both)
}

/// The field name a relation treats as the key it points at.
#[cfg(not(target_arch = "wasm32"))]
pub(super) const KEY_FIELD: &str = "id";

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
pub(super) fn relation_references_to(from_pin: &str, to_pin: &str) -> bool {
    to_pin == KEY_FIELD || from_pin != KEY_FIELD
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every refusal the drop can meet, and the sentence it produces. The
    /// widget only reports the pair, so this function is the whole of what the
    /// user is told -- and the whole of what `can_connect` decides.
    #[test]
    fn a_refused_wire_names_the_rule_that_refused_it() {
        let converters = TypeConverters::with_builtins();
        let never = |_: bool| false;
        let always = |_: bool| true;
        let out = PinDefinition::output("out", Ty::Int);
        let out_str = PinDefinition::output("out", Ty::Str);
        let input = PinDefinition::input("a", Ty::Int, PinKind::Sample);
        let field_int = PinDefinition::field("id", Ty::Int);
        let field_str = PinDefinition::field("name", Ty::Str);
        let wire = |from, to, same_node, from_free, to_free, cycle: &dyn Fn(bool) -> bool| {
            wire_refusal(&Wire {
                same_node,
                from,
                to,
                from_free,
                to_free,
                closes_cycle: cycle,
                converters: &converters,
            })
        };

        // What is allowed says nothing.
        assert_eq!(
            wire(Some(&out), Some(&input), false, true, true, &never),
            None
        );
        assert_eq!(
            wire(
                Some(&field_int),
                Some(&field_int),
                false,
                true,
                true,
                &never
            ),
            None
        );

        // A relation onto the same table, and a value onto its own node, are
        // two different sentences about the same mistake.
        assert_eq!(
            wire(Some(&field_int), Some(&field_str), true, true, true, &never),
            Some("a table cannot reference itself".to_string())
        );
        assert_eq!(
            wire(Some(&out), Some(&input), true, true, true, &never),
            Some("a node cannot feed itself".to_string())
        );

        assert_eq!(
            wire(
                Some(&field_int),
                Some(&field_str),
                false,
                true,
                true,
                &never
            ),
            Some("int and str fields cannot be related".to_string())
        );
        assert_eq!(
            wire(Some(&field_int), Some(&input), false, true, true, &never),
            Some("a field only relates to another field".to_string())
        );
        assert_eq!(
            wire(Some(&out), Some(&out), false, true, true, &never),
            Some("two outputs cannot be wired together".to_string())
        );
        assert_eq!(
            wire(Some(&input), Some(&input), false, true, true, &never),
            Some("two inputs cannot be wired together".to_string())
        );
        assert_eq!(
            wire(Some(&out), Some(&input), false, true, false, &never),
            Some("that input already has a wire".to_string())
        );
        assert_eq!(
            wire(Some(&out), Some(&input), false, true, true, &always),
            Some("that wire would close a loop".to_string())
        );
        // No converter between the two types, named by type rather than by pin.
        let opaque = PinDefinition::output("table", Ty::opaque("db.table"));
        assert_eq!(
            wire(Some(&opaque), Some(&input), false, true, true, &never),
            Some("nothing converts db.table into int".to_string())
        );
        assert_eq!(
            wire(Some(&out_str), Some(&input), false, true, true, &never),
            Some("nothing converts str into int".to_string())
        );
        // A pin the node stopped declaring: the drop landed on nothing.
        assert_eq!(
            wire(None, Some(&input), false, true, true, &never),
            Some("that pin is no longer there".to_string())
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
        // declares. Not a relation of ours, and above all not ours to delete:
        // this is the case that would take live wires with it.
        assert!(!is_lost_relation(Some(&output), Some(&field)));
        assert!(!is_lost_relation(Some(&input), Some(&field)));

        // Nothing to do with relations at all: a dataflow wire whose pin
        // vanished for a moment while a setting was half-typed must survive.
        assert!(!is_lost_relation(None, Some(&output)));
        assert!(!is_lost_relation(None, None));
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

        // A graph that already has a cycle must not hang the walk: one can
        // arrive from the store, which the runner tolerates.
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
                    outgoing: vec![0],
                    incoming: vec![1],
                    flow_out: Vec::new(),
                },
            ),
            (NodeId(2), node(vec![])),
        ]);
        assert!(!flow_reaches(&relations, NodeId(1), NodeId(2)));
    }
}
