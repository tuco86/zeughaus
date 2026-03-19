use std::collections::{HashMap, VecDeque};

use zeughaus_core::{NodeId, Result, ZeughausError};

use crate::graph::Graph;

/// Topological sort via Kahn's algorithm.
/// Returns nodes in execution order (sources first).
pub fn topological_sort(graph: &Graph) -> Result<Vec<NodeId>> {
    let mut in_degree: HashMap<NodeId, usize> = HashMap::new();

    for node_id in graph.node_ids() {
        in_degree.entry(node_id).or_insert(0);
        for &edge_id in graph.outgoing_edges(node_id) {
            if let Some(edge) = graph.edge(edge_id) {
                *in_degree.entry(edge.to_node).or_insert(0) += 1;
            }
        }
    }

    let mut queue: VecDeque<NodeId> = in_degree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(id, _)| *id)
        .collect();

    // Sort the initial queue for deterministic output
    let mut sorted_queue: Vec<NodeId> = queue.drain(..).collect();
    sorted_queue.sort();
    queue.extend(sorted_queue);

    let mut result = Vec::new();

    while let Some(node) = queue.pop_front() {
        result.push(node);
        for &edge_id in graph.outgoing_edges(node) {
            if let Some(edge) = graph.edge(edge_id)
                && let Some(deg) = in_degree.get_mut(&edge.to_node)
            {
                *deg -= 1;
                if *deg == 0 {
                    queue.push_back(edge.to_node);
                }
            }
        }
    }

    if result.len() != in_degree.len() {
        return Err(ZeughausError::CycleDetected);
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Graph, GraphEdge, GraphNode};
    use zeughaus_core::{EdgeId, EdgeSemantic, NodeConfig};

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
    fn empty_graph() {
        let g = Graph::new();
        let order = topological_sort(&g).unwrap();
        assert!(order.is_empty());
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

        let order = topological_sort(&g).unwrap();
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

        let order = topological_sort(&g).unwrap();
        let pos_a = order.iter().position(|&x| x == a).unwrap();
        let pos_b = order.iter().position(|&x| x == b).unwrap();
        let pos_c = order.iter().position(|&x| x == c).unwrap();
        let pos_d = order.iter().position(|&x| x == d).unwrap();
        assert!(pos_a < pos_b);
        assert!(pos_a < pos_c);
        assert!(pos_b < pos_d);
        assert!(pos_c < pos_d);
    }

    #[test]
    fn cycle_detected() {
        let mut g = Graph::new();
        let a = NodeId::next();
        let b = NodeId::next();
        g.add_node(make_node(a));
        g.add_node(make_node(b));
        g.add_edge(make_edge(a, b));
        g.add_edge(make_edge(b, a));

        assert!(matches!(
            topological_sort(&g),
            Err(ZeughausError::CycleDetected)
        ));
    }
}
