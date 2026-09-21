//! Arranging a graph's nodes in columns by depth.

use std::collections::{HashMap, HashSet};

use iced::Point;
use zeughaus_core::NodeId;

/// Horizontal distance between two ranks. One node is 180 wide (240 for a
/// Display), so this leaves a cable's worth of room between columns.
const LAYOUT_COLUMN: f32 = 320.0;

/// Vertical distance between two nodes of the same rank.
const LAYOUT_ROW: f32 = 160.0;

/// Where the first node goes; the same margin on both axes.
const LAYOUT_MARGIN: f32 = 40.0;

/// Positions for one graph: a column per depth, a row per node in it.
///
/// Free function taking ids and edges so the arithmetic can be tested without
/// an editor, and so the caller decides what "one graph" means -- the editor
/// passes the current graph's nodes and its *mapped* view edges, which is what
/// makes a subgraph lay out by the wires the user can see.
///
/// Rank is the longest path from a source, so a node sits to the right of
/// everything that feeds it. Within a rank, nodes are ordered by the mean
/// height of their already-placed predecessors (the barycenter, which is what
/// keeps cables from crossing), ties by id so the result is stable. A cycle has
/// no source and no longest path: the lowest remaining id is admitted as if its
/// incoming edges were not there, which is exactly "ignore the back edge", and
/// guarantees termination because every step places one node.
pub(super) fn auto_layout(nodes: &[NodeId], edges: &[(NodeId, NodeId)]) -> Vec<(NodeId, Point)> {
    use std::collections::BTreeSet;

    let present: HashSet<NodeId> = nodes.iter().copied().collect();
    let wires: Vec<(NodeId, NodeId)> = edges
        .iter()
        .copied()
        .filter(|(from, to)| from != to && present.contains(from) && present.contains(to))
        .collect();

    let mut incoming: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    let mut outgoing: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for (from, to) in &wires {
        outgoing.entry(*from).or_default().push(*to);
        incoming.entry(*to).or_default().push(*from);
    }
    let mut pending: HashMap<NodeId, usize> = nodes
        .iter()
        .map(|id| (*id, incoming.get(id).map_or(0, Vec::len)))
        .collect();

    let mut ready: BTreeSet<u64> = pending
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(id, _)| id.0)
        .collect();
    let mut rank: HashMap<NodeId, usize> = nodes.iter().map(|id| (*id, 0)).collect();
    let mut placed: HashSet<NodeId> = HashSet::with_capacity(nodes.len());
    let mut order: Vec<NodeId> = Vec::with_capacity(nodes.len());

    while order.len() < nodes.len() {
        let next = match ready.iter().next().copied() {
            Some(next) => next,
            // Everything left is in a cycle. Admitting the lowest id ignores
            // its back edges instead of looping forever.
            None => nodes
                .iter()
                .filter(|id| !placed.contains(id))
                .map(|id| id.0)
                .min()
                .expect("nodes remain while the order is short"),
        };
        ready.remove(&next);
        let node = NodeId(next);
        if !placed.insert(node) {
            continue;
        }
        order.push(node);
        let depth = rank[&node];
        for target in outgoing.get(&node).into_iter().flatten() {
            if placed.contains(target) {
                continue;
            }
            let known = rank.entry(*target).or_default();
            *known = (*known).max(depth + 1);
            if let Some(count) = pending.get_mut(target) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    ready.insert(target.0);
                }
            }
        }
    }

    let mut columns: Vec<Vec<NodeId>> = Vec::new();
    for node in &order {
        let depth = rank[node];
        if columns.len() <= depth {
            columns.resize(depth + 1, Vec::new());
        }
        columns[depth].push(*node);
    }

    let mut positions: HashMap<NodeId, Point> = HashMap::with_capacity(nodes.len());
    for (depth, column) in columns.iter().enumerate() {
        let mut sorted: Vec<(Option<f32>, u64, NodeId)> = column
            .iter()
            .map(|node| {
                let heights: Vec<f32> = incoming
                    .get(node)
                    .into_iter()
                    .flatten()
                    .filter_map(|source| positions.get(source).map(|p| p.y))
                    .collect();
                let barycenter = if heights.is_empty() {
                    None
                } else {
                    Some(heights.iter().sum::<f32>() / heights.len() as f32)
                };
                (barycenter, node.0, *node)
            })
            .collect();
        // A node with nothing placed above it has no barycenter and goes last,
        // where it cannot push a wired node out of line.
        sorted.sort_by(|a, b| match (a.0, b.0) {
            (Some(left), Some(right)) => left
                .partial_cmp(&right)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.1.cmp(&b.1),
        });
        for (row, (_, _, node)) in sorted.into_iter().enumerate() {
            positions.insert(
                node,
                Point::new(
                    LAYOUT_MARGIN + depth as f32 * LAYOUT_COLUMN,
                    LAYOUT_MARGIN + row as f32 * LAYOUT_ROW,
                ),
            );
        }
    }

    let mut out: Vec<(NodeId, Point)> = positions.into_iter().collect();
    out.sort_by_key(|(node, _)| node.0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(positions: &[(NodeId, Point)], node: u64) -> Point {
        positions
            .iter()
            .find(|(id, _)| id.0 == node)
            .map(|(_, p)| *p)
            .unwrap_or_else(|| panic!("node {node} was not placed"))
    }

    /// The layout has to say something for every node, put each one to the
    /// right of what feeds it, and never place two in the same spot -- and a
    /// cycle has to come out the other side rather than hang.
    #[test]
    fn a_diamond_and_a_cycle_land_in_columns_without_overlap() {
        // 1 -> 2 -> 4, 1 -> 3 -> 4 (a diamond), plus 5 <-> 6 (a cycle) hanging
        // off 4.
        let nodes: Vec<NodeId> = (1..=6).map(NodeId).collect();
        let edges: Vec<(NodeId, NodeId)> = vec![
            (NodeId(1), NodeId(2)),
            (NodeId(1), NodeId(3)),
            (NodeId(2), NodeId(4)),
            (NodeId(3), NodeId(4)),
            (NodeId(4), NodeId(5)),
            (NodeId(5), NodeId(6)),
            (NodeId(6), NodeId(5)),
        ];

        let positions = auto_layout(&nodes, &edges);
        assert_eq!(positions.len(), nodes.len());

        // Columns by depth: the diamond's sides share one, its tip is past
        // both, and the longest path decides -- not the first path found.
        assert_eq!(at(&positions, 1).x, 40.0);
        assert_eq!(at(&positions, 2).x, 40.0 + 320.0);
        assert_eq!(at(&positions, 3).x, 40.0 + 320.0);
        assert_eq!(at(&positions, 4).x, 40.0 + 2.0 * 320.0);
        assert_eq!(at(&positions, 5).x, 40.0 + 3.0 * 320.0);
        assert_eq!(at(&positions, 6).x, 40.0 + 4.0 * 320.0);

        // The two sides of the diamond share a column, so they must not share
        // a row.
        assert_ne!(at(&positions, 2).y, at(&positions, 3).y);
        assert_eq!(at(&positions, 2).y, 40.0);
        assert_eq!(at(&positions, 3).y, 40.0 + 160.0);

        // Nothing lands on top of anything else.
        let mut spots: Vec<(u32, u32)> = positions
            .iter()
            .map(|(_, p)| (p.x.to_bits(), p.y.to_bits()))
            .collect();
        spots.sort_unstable();
        let unique = spots.len();
        spots.dedup();
        assert_eq!(spots.len(), unique, "two nodes were placed in one spot");
    }

    /// A graph with nothing but a cycle still lays out: every node is placed
    /// once, in a column, and the call returns.
    #[test]
    fn a_graph_that_is_only_a_cycle_still_terminates() {
        let nodes: Vec<NodeId> = (1..=3).map(NodeId).collect();
        let edges = vec![
            (NodeId(1), NodeId(2)),
            (NodeId(2), NodeId(3)),
            (NodeId(3), NodeId(1)),
        ];
        let positions = auto_layout(&nodes, &edges);
        assert_eq!(positions.len(), 3);
        assert_eq!(at(&positions, 1).x, 40.0);
        assert_eq!(at(&positions, 2).x, 40.0 + 320.0);
        assert_eq!(at(&positions, 3).x, 40.0 + 2.0 * 320.0);
    }

    /// An edge naming a node of another graph is not this graph's business:
    /// the editor passes mapped view edges, and anything unmapped is dropped.
    #[test]
    fn edges_to_nodes_outside_the_graph_are_ignored() {
        let nodes = vec![NodeId(1), NodeId(2)];
        let edges = vec![
            (NodeId(9), NodeId(1)),
            (NodeId(1), NodeId(2)),
            (NodeId(2), NodeId(2)),
        ];
        let positions = auto_layout(&nodes, &edges);
        assert_eq!(at(&positions, 1), Point::new(40.0, 40.0));
        assert_eq!(at(&positions, 2), Point::new(360.0, 40.0));
    }
}
