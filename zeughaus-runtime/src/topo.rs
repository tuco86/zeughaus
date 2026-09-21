use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use zeughaus_core::NodeId;

use crate::graph::Graph;

/// The order the graph can be executed in, plus the nodes that have no place
/// in any order.
///
/// Kahn's algorithm with NodeId tiebreaking: when several nodes have in-degree
/// zero, the smallest id is taken first, so the order is the same on every run
/// and in every process.
///
/// `stuck` is whatever is left when Kahn stalls: every node in a cycle, and
/// everything downstream of one -- their in-degree never reaches zero either.
/// Reported rather than turned into one failure for the whole graph, because a
/// cycle is a property of the nodes in it: the rest of the document has
/// nothing to do with it and has to keep running. Sorted by id like `order`,
/// so what a caller reports about them is stable too.
pub fn topological_order(graph: &Graph) -> (Vec<NodeId>, Vec<NodeId>) {
    let mut in_degree: HashMap<NodeId, usize> = HashMap::new();

    for node_id in graph.node_ids() {
        in_degree.entry(node_id).or_insert(0);
        for edge_id in graph.outgoing_edges(node_id) {
            if let Some(edge) = graph.edge(edge_id) {
                *in_degree.entry(edge.to_node).or_insert(0) += 1;
            }
        }
    }

    // Min-heap by NodeId for deterministic ordering
    let mut heap: BinaryHeap<Reverse<NodeId>> = in_degree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(id, _)| Reverse(*id))
        .collect();

    let mut result = Vec::new();

    while let Some(Reverse(node)) = heap.pop() {
        result.push(node);
        for edge_id in graph.outgoing_edges(node) {
            if let Some(edge) = graph.edge(edge_id)
                && let Some(deg) = in_degree.get_mut(&edge.to_node)
            {
                *deg -= 1;
                if *deg == 0 {
                    heap.push(Reverse(edge.to_node));
                }
            }
        }
    }

    // Everything Kahn placed reached in-degree zero, so what still carries an
    // incoming edge is exactly what it could not place.
    let mut stuck: Vec<NodeId> = in_degree
        .iter()
        .filter(|(_, degree)| **degree > 0)
        .map(|(id, _)| *id)
        .collect();
    stuck.sort();
    (result, stuck)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Graph, GraphEdge, GraphNode};
    use zeughaus_core::EdgeId;

    fn make_node(id: NodeId) -> GraphNode {
        GraphNode {
            id,
            type_id: "test".to_string(),
            pin_defs: vec![],
        }
    }

    fn make_edge(from: NodeId, to: NodeId) -> GraphEdge {
        GraphEdge {
            id: EdgeId::next(),
            from_node: from,
            from_pin: "out".into(),
            to_node: to,
            to_pin: "in".into(),
        }
    }

    #[test]
    fn empty_graph() {
        let g = Graph::new();
        let (order, stuck) = topological_order(&g);
        assert!(order.is_empty());
        assert!(stuck.is_empty());
    }

    #[test]
    fn linear_chain() {
        let mut g = Graph::new();
        let a = NodeId::next();
        let b = NodeId::next();
        let c = NodeId::next();
        g.add_node(make_node(a));
        g.add_node(make_node(b));
        g.add_node(make_node(c));
        g.add_edge(make_edge(a, b));
        g.add_edge(make_edge(b, c));

        let (order, stuck) = topological_order(&g);
        assert!(stuck.is_empty());
        let pos_a = order.iter().position(|&x| x == a).unwrap();
        let pos_b = order.iter().position(|&x| x == b).unwrap();
        let pos_c = order.iter().position(|&x| x == c).unwrap();
        assert!(pos_a < pos_b);
        assert!(pos_b < pos_c);
    }

    #[test]
    fn diamond() {
        let mut g = Graph::new();
        let a = NodeId::next();
        let b = NodeId::next();
        let c = NodeId::next();
        let d = NodeId::next();
        g.add_node(make_node(a));
        g.add_node(make_node(b));
        g.add_node(make_node(c));
        g.add_node(make_node(d));
        g.add_edge(make_edge(a, b));
        g.add_edge(make_edge(a, c));
        g.add_edge(make_edge(b, d));
        g.add_edge(make_edge(c, d));

        let (order, stuck) = topological_order(&g);
        assert!(stuck.is_empty());
        let pos_a = order.iter().position(|&x| x == a).unwrap();
        let pos_b = order.iter().position(|&x| x == b).unwrap();
        let pos_c = order.iter().position(|&x| x == c).unwrap();
        let pos_d = order.iter().position(|&x| x == d).unwrap();
        assert!(pos_a < pos_b);
        assert!(pos_a < pos_c);
        assert!(pos_b < pos_d);
        assert!(pos_c < pos_d);
    }

    /// What the executor works from: the cycle's nodes and everything after
    /// them are named as stuck, the rest is a usable order. A self-edge counts
    /// as its own cycle.
    #[test]
    fn a_cycle_leaves_its_own_nodes_stuck_and_orders_the_rest() {
        let mut g = Graph::new();
        let (a, b, sink, loner, own) = (
            NodeId::next(),
            NodeId::next(),
            NodeId::next(),
            NodeId::next(),
            NodeId::next(),
        );
        for id in [a, b, sink, loner, own] {
            g.add_node(make_node(id));
        }
        g.add_edge(make_edge(a, b));
        g.add_edge(make_edge(b, a));
        g.add_edge(make_edge(b, sink));
        g.add_edge(make_edge(own, own));

        let (order, stuck) = topological_order(&g);
        assert_eq!(order, vec![loner]);
        assert_eq!(stuck, vec![a, b, sink, own]);
    }

    /// Two independent nodes (no edges) must always be sorted by NodeId.
    /// Run multiple times to verify determinism.
    #[test]
    fn independent_nodes_sorted_by_id() {
        for _ in 0..20 {
            let mut g = Graph::new();
            // Create nodes in arbitrary order -- result must always be sorted by ID
            let b = NodeId::next();
            let a = NodeId::next();
            let c = NodeId::next();
            g.add_node(make_node(c));
            g.add_node(make_node(a));
            g.add_node(make_node(b));

            let (order, _) = topological_order(&g);
            assert_eq!(order.len(), 3);
            // Must be sorted by NodeId (which wraps u64, smallest first)
            assert!(order[0] < order[1]);
            assert!(order[1] < order[2]);
        }
    }

    /// In a diamond, when b and c both become ready after a, the one
    /// with the smaller NodeId must come first.
    #[test]
    fn diamond_tiebreak_deterministic() {
        for _ in 0..20 {
            let mut g = Graph::new();
            let a = NodeId::next();
            let b = NodeId::next();
            let c = NodeId::next();
            let d = NodeId::next();
            g.add_node(make_node(a));
            g.add_node(make_node(b));
            g.add_node(make_node(c));
            g.add_node(make_node(d));
            g.add_edge(make_edge(a, b));
            g.add_edge(make_edge(a, c));
            g.add_edge(make_edge(b, d));
            g.add_edge(make_edge(c, d));

            let (order, _) = topological_order(&g);
            // a must be first, d must be last
            assert_eq!(order[0], a);
            assert_eq!(order[3], d);
            // b and c: smaller ID must come first
            if b < c {
                assert_eq!(order[1], b);
                assert_eq!(order[2], c);
            } else {
                assert_eq!(order[1], c);
                assert_eq!(order[2], b);
            }
        }
    }
}
