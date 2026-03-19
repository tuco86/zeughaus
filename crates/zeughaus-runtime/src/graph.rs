use std::collections::HashMap;

use zeughaus_core::{EdgeId, EdgeSemantic, NodeConfig, NodeId, PinDefinition};

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
    pub from_pin: &'static str,
    pub to_node: NodeId,
    pub to_pin: &'static str,
    pub semantic: EdgeSemantic,
}

pub struct Graph {
    nodes: HashMap<NodeId, GraphNode>,
    edges: HashMap<EdgeId, GraphEdge>,
    outgoing: HashMap<NodeId, Vec<EdgeId>>,
    incoming: HashMap<NodeId, Vec<EdgeId>>,
}

impl Graph {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            edges: HashMap::new(),
            outgoing: HashMap::new(),
            incoming: HashMap::new(),
        }
    }

    pub fn add_node(&mut self, node: GraphNode) {
        let id = node.id;
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
    }

    pub fn add_edge(&mut self, edge: GraphEdge) {
        let id = edge.id;
        let from = edge.from_node;
        let to = edge.to_node;
        self.edges.insert(id, edge);
        self.outgoing.entry(from).or_default().push(id);
        self.incoming.entry(to).or_default().push(id);
    }

    pub fn remove_edge(&mut self, id: EdgeId) {
        if let Some(edge) = self.edges.remove(&id) {
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

    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut GraphNode> {
        self.nodes.get_mut(&id)
    }

    pub fn edge(&self, id: EdgeId) -> Option<&GraphEdge> {
        self.edges.get(&id)
    }

    pub fn incoming_edges(&self, node: NodeId) -> &[EdgeId] {
        self.incoming.get(&node).map_or(&[], |v| v.as_slice())
    }

    pub fn outgoing_edges(&self, node: NodeId) -> &[EdgeId] {
        self.outgoing.get(&node).map_or(&[], |v| v.as_slice())
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
            for &edge_id in self.outgoing_edges(node) {
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
    use zeughaus_core::NodeConfig;

    fn make_node(id: NodeId) -> GraphNode {
        GraphNode {
            id,
            type_id: "test".to_string(),
            config: NodeConfig::default(),
            pin_defs: vec![],
            position: (0.0, 0.0),
        }
    }

    fn make_edge(from: NodeId, to: NodeId) -> GraphEdge {
        GraphEdge {
            id: EdgeId::next(),
            from_node: from,
            from_pin: "out",
            to_node: to,
            to_pin: "in",
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
        assert_eq!(g.outgoing_edges(a).len(), 1);
        assert_eq!(g.incoming_edges(b).len(), 1);
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
        assert_eq!(g.outgoing_edges(a).len(), 0);
        assert_eq!(g.incoming_edges(b).len(), 0);
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
}
