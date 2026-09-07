use std::collections::HashMap;
use std::sync::Arc;

use zeughaus_core::{EdgeId, EdgeSemantic, NodeConfig, NodeId, PinDefinition, PinDirection};

pub struct GraphNode {
    pub id: NodeId,
    pub type_id: String,
    pub config: NodeConfig,
    pub pin_defs: Vec<PinDefinition>,
    pub position: (f32, f32),
}

pub struct GraphEdge {
    pub id: EdgeId,
    pub from_node: NodeId,
    /// Source pin name. Owned because pin names are not compile-time literals:
    /// a variadic node grows them and a loaded document brings them as strings.
    pub from_pin: Arc<str>,
    pub to_node: NodeId,
    pub to_pin: Arc<str>,
    pub semantic: EdgeSemantic,
}

pub struct Graph {
    nodes: HashMap<NodeId, GraphNode>,
    edges: HashMap<EdgeId, GraphEdge>,
    outgoing: HashMap<NodeId, Vec<EdgeId>>,
    incoming: HashMap<NodeId, Vec<EdgeId>>,
    /// Bumped by every mutator this type has. What it buys is a cache key for
    /// anything derived from the topology -- the execution order above all --
    /// and it is trustworthy because these fields are private: there is no way
    /// to move a node, a wire or a pin declaration without passing through one
    /// of the methods below.
    revision: u64,
}

impl Graph {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            edges: HashMap::new(),
            outgoing: HashMap::new(),
            incoming: HashMap::new(),
            revision: 0,
        }
    }

    /// How many times this graph has been changed.
    ///
    /// The cache key for anything derived from the topology: equal revisions
    /// mean the same nodes, the same wires and the same pin declarations, so
    /// the same execution order.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn add_node(&mut self, node: GraphNode) {
        let id = node.id;
        self.revision += 1;
        self.nodes.insert(id, node);
        self.outgoing.entry(id).or_default();
        self.incoming.entry(id).or_default();
    }

    pub fn remove_node(&mut self, id: NodeId) {
        // Remove all edges connected to this node
        let edge_ids: Vec<EdgeId> = self
            .outgoing
            .get(&id)
            .into_iter()
            .flatten()
            .chain(self.incoming.get(&id).into_iter().flatten())
            .copied()
            .collect();

        for edge_id in edge_ids {
            self.remove_edge(edge_id);
        }

        self.nodes.remove(&id);
        self.outgoing.remove(&id);
        self.incoming.remove(&id);
        self.revision += 1;
    }

    pub fn add_edge(&mut self, edge: GraphEdge) {
        let id = edge.id;
        let from = edge.from_node;
        let to = edge.to_node;
        self.revision += 1;
        self.edges.insert(id, edge);
        self.outgoing.entry(from).or_default().push(id);
        self.incoming.entry(to).or_default().push(id);
    }

    pub fn remove_edge(&mut self, id: EdgeId) {
        if let Some(edge) = self.edges.remove(&id) {
            self.revision += 1;
            if let Some(out) = self.outgoing.get_mut(&edge.from_node) {
                out.retain(|e| *e != id);
            }
            if let Some(inc) = self.incoming.get_mut(&edge.to_node) {
                inc.retain(|e| *e != id);
            }
        }
    }

    pub fn node(&self, id: NodeId) -> Option<&GraphNode> {
        self.nodes.get(&id)
    }

    /// A node, mutable. Counts as a change: a pin declaration decides whether
    /// an edge to it carries data at all (see [`Self::is_dataflow`]), so this
    /// can move the topology and not only the node's position.
    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut GraphNode> {
        self.revision += 1;
        self.nodes.get_mut(&id)
    }

    pub fn edge(&self, id: EdgeId) -> Option<&GraphEdge> {
        self.edges.get(&id)
    }

    /// Whether an edge carries data.
    ///
    /// It does unless one of its ends is a bidirectional field pin
    /// ([`PinDirection::Both`]). Such an edge is a *relation*: it declares a
    /// relationship between the two nodes -- two table fields wired together
    /// are a foreign key -- rather than a value in flight. It must never
    /// become a dependency, because two tables referencing each other is a
    /// legal schema and a legal cycle: as a dependency it would make the whole
    /// graph unorderable and no node at all would run.
    ///
    /// One end is enough. A field a relation still points at may already have
    /// been renamed away on the other side, and that half-stale wire must not
    /// turn into a dependency on the way out.
    ///
    /// This is the one place the rule lives. Every propagation path in this
    /// crate -- [`Self::incoming_edges`], [`Self::outgoing_edges`] and
    /// therefore the topological order, the dirty walk and a node's input set
    /// -- reads it from here. Node type ids play no part: the editor and the
    /// runner build their graph from the same store and get the same answer.
    pub fn is_dataflow(&self, id: EdgeId) -> bool {
        let Some(edge) = self.edges.get(&id) else {
            return false;
        };
        !self.is_field(edge.from_node, &edge.from_pin) && !self.is_field(edge.to_node, &edge.to_pin)
    }

    fn is_field(&self, node: NodeId, pin: &str) -> bool {
        self.nodes.get(&node).is_some_and(|n| {
            n.pin_defs
                .iter()
                .any(|p| &*p.name == pin && p.direction == PinDirection::Both)
        })
    }

    /// The data edges entering `node` (see [`Self::is_dataflow`]).
    pub fn incoming_edges(&self, node: NodeId) -> impl Iterator<Item = EdgeId> + '_ {
        self.incoming
            .get(&node)
            .into_iter()
            .flatten()
            .copied()
            .filter(move |id| self.is_dataflow(*id))
    }

    /// The data edges leaving `node` (see [`Self::is_dataflow`]).
    pub fn outgoing_edges(&self, node: NodeId) -> impl Iterator<Item = EdgeId> + '_ {
        self.outgoing
            .get(&node)
            .into_iter()
            .flatten()
            .copied()
            .filter(move |id| self.is_dataflow(*id))
    }

    /// Every edge with an endpoint on `node`, relations included.
    ///
    /// Structure rather than dataflow: this is what removing a node has to
    /// clean up, and a relation is as much a wire to forget as any other.
    pub fn edges_of(&self, node: NodeId) -> impl Iterator<Item = EdgeId> + '_ {
        self.outgoing
            .get(&node)
            .into_iter()
            .flatten()
            .chain(self.incoming.get(&node).into_iter().flatten())
            .copied()
    }

    pub fn node_ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.nodes.keys().copied()
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    pub fn edges(&self) -> impl Iterator<Item = &GraphEdge> {
        self.edges.values()
    }

    /// Returns all downstream node IDs reachable from the given node.
    pub fn downstream(&self, start: NodeId) -> Vec<NodeId> {
        let mut visited = std::collections::HashSet::new();
        let mut stack = vec![start];
        let mut result = Vec::new();

        while let Some(node) = stack.pop() {
            for edge_id in self.outgoing_edges(node) {
                if let Some(edge) = self.edge(edge_id)
                    && visited.insert(edge.to_node)
                {
                    result.push(edge.to_node);
                    stack.push(edge.to_node);
                }
            }
        }

        result
    }
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeughaus_core::{NodeConfig, Ty};

    fn make_node(id: NodeId) -> GraphNode {
        GraphNode {
            id,
            type_id: "test".to_string(),
            config: NodeConfig::default(),
            pin_defs: vec![],
            position: (0.0, 0.0),
        }
    }

    /// A node with one bidirectional field pin, as a table's field is.
    fn make_field_node(id: NodeId) -> GraphNode {
        GraphNode {
            id,
            type_id: "table".to_string(),
            config: NodeConfig::default(),
            pin_defs: vec![PinDefinition::field("id", Ty::opaque("db.field"))],
            position: (0.0, 0.0),
        }
    }

    fn make_edge(from: NodeId, to: NodeId) -> GraphEdge {
        GraphEdge {
            id: EdgeId::next(),
            from_node: from,
            from_pin: "out".into(),
            to_node: to,
            to_pin: "in".into(),
            semantic: EdgeSemantic::default(),
        }
    }

    fn make_relation(from: NodeId, to: NodeId) -> GraphEdge {
        GraphEdge {
            id: EdgeId::next(),
            from_node: from,
            from_pin: "id".into(),
            to_node: to,
            to_pin: "id".into(),
            semantic: EdgeSemantic::default(),
        }
    }

    #[test]
    fn add_and_query_nodes() {
        let mut g = Graph::new();
        let a = NodeId::next();
        let b = NodeId::next();
        g.add_node(make_node(a));
        g.add_node(make_node(b));
        assert_eq!(g.node_count(), 2);
        assert!(g.node(a).is_some());
        assert!(g.node(b).is_some());
    }

    #[test]
    fn add_and_query_edges() {
        let mut g = Graph::new();
        let a = NodeId::next();
        let b = NodeId::next();
        g.add_node(make_node(a));
        g.add_node(make_node(b));
        g.add_edge(make_edge(a, b));
        assert_eq!(g.edge_count(), 1);
        assert_eq!(g.outgoing_edges(a).count(), 1);
        assert_eq!(g.incoming_edges(b).count(), 1);
    }

    #[test]
    fn remove_edge() {
        let mut g = Graph::new();
        let a = NodeId::next();
        let b = NodeId::next();
        g.add_node(make_node(a));
        g.add_node(make_node(b));
        let edge = make_edge(a, b);
        let eid = edge.id;
        g.add_edge(edge);
        g.remove_edge(eid);
        assert_eq!(g.edge_count(), 0);
        assert_eq!(g.outgoing_edges(a).count(), 0);
        assert_eq!(g.incoming_edges(b).count(), 0);
    }

    #[test]
    fn remove_node_removes_edges() {
        let mut g = Graph::new();
        let a = NodeId::next();
        let b = NodeId::next();
        let c = NodeId::next();
        g.add_node(make_node(a));
        g.add_node(make_node(b));
        g.add_node(make_node(c));
        g.add_edge(make_edge(a, b));
        g.add_edge(make_edge(b, c));
        g.remove_node(b);
        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn downstream() {
        let mut g = Graph::new();
        let a = NodeId::next();
        let b = NodeId::next();
        let c = NodeId::next();
        g.add_node(make_node(a));
        g.add_node(make_node(b));
        g.add_node(make_node(c));
        g.add_edge(make_edge(a, b));
        g.add_edge(make_edge(b, c));
        let ds = g.downstream(a);
        assert!(ds.contains(&b));
        assert!(ds.contains(&c));
        assert!(!ds.contains(&a));
    }

    /// The walk every dirty mark uses has to terminate on a cycle: a user can
    /// wire one, and this is called while the executor is deciding what to run.
    #[test]
    fn downstream_terminates_on_a_cycle() {
        let mut g = Graph::new();
        let a = NodeId::next();
        let b = NodeId::next();
        g.add_node(make_node(a));
        g.add_node(make_node(b));
        g.add_edge(make_edge(a, b));
        g.add_edge(make_edge(b, a));
        let ds = g.downstream(a);
        assert!(ds.contains(&b));
        assert!(ds.contains(&a), "a is reachable from itself through b");
        assert_eq!(ds.len(), 2, "each node is reported once");

        // A node wired to itself is the smallest cycle there is.
        let own = NodeId::next();
        g.add_node(make_node(own));
        g.add_edge(make_edge(own, own));
        assert_eq!(g.downstream(own), vec![own]);
    }

    /// A relation between two field pins is held by the graph but is not a
    /// dependency: two tables referencing each other has to stay orderable.
    #[test]
    fn a_relation_between_field_pins_is_not_dataflow() {
        let mut g = Graph::new();
        let a = NodeId::next();
        let b = NodeId::next();
        g.add_node(make_field_node(a));
        g.add_node(make_field_node(b));
        let there = make_relation(a, b);
        let back = make_relation(b, a);
        let (there_id, back_id) = (there.id, back.id);
        g.add_edge(there);
        g.add_edge(back);

        assert_eq!(g.edge_count(), 2);
        assert!(!g.is_dataflow(there_id));
        assert!(!g.is_dataflow(back_id));
        assert_eq!(g.outgoing_edges(a).count(), 0);
        assert_eq!(g.incoming_edges(b).count(), 0);
        assert!(g.downstream(a).is_empty());
        // Structure still knows them, which is what removing a node cleans up.
        assert_eq!(g.edges_of(a).count(), 2);
        g.remove_node(a);
        assert_eq!(g.edge_count(), 0);
    }

    /// A relation whose other end has already been renamed away is still a
    /// relation: one bidirectional end is enough, so a half-edited schema
    /// cannot turn a foreign key into a dependency.
    #[test]
    fn one_field_end_is_enough_to_keep_a_relation_out_of_dataflow() {
        let mut g = Graph::new();
        let a = NodeId::next();
        let b = NodeId::next();
        g.add_node(make_field_node(a));
        g.add_node(make_node(b));
        let edge = make_relation(a, b);
        let id = edge.id;
        g.add_edge(edge);
        assert!(!g.is_dataflow(id));
        assert_eq!(g.outgoing_edges(a).count(), 0);
        assert_eq!(g.incoming_edges(b).count(), 0);
    }

    /// The revision is a cache key for the execution order, so every change
    /// that can move that order has to move it -- a pin declaration included,
    /// because it decides whether an edge is a dependency at all. A call that
    /// changed nothing must not move it, or the cache would never hold.
    #[test]
    fn every_structural_change_moves_the_revision() {
        let mut g = Graph::new();
        let a = NodeId::next();
        let b = NodeId::next();

        let start = g.revision();
        g.add_node(make_node(a));
        g.add_node(make_node(b));
        let after_nodes = g.revision();
        assert!(after_nodes > start, "adding a node is a change");

        let edge = make_edge(a, b);
        let edge_id = edge.id;
        g.add_edge(edge);
        let after_edge = g.revision();
        assert!(after_edge > after_nodes, "adding a wire is a change");

        g.node_mut(a).expect("node").pin_defs.clear();
        let after_pins = g.revision();
        assert!(after_pins > after_edge, "a pin declaration is a change");

        g.remove_edge(edge_id);
        let after_remove = g.revision();
        assert!(after_remove > after_pins, "removing a wire is a change");

        g.remove_edge(edge_id);
        assert_eq!(
            g.revision(),
            after_remove,
            "removing a wire that is not there changes nothing"
        );

        g.remove_node(a);
        assert!(g.revision() > after_remove, "removing a node is a change");
    }
}
