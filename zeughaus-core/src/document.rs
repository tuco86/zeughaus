//! The graph as it is stored: one row per node and per edge.
//!
//! [`NodeData`] and [`EdgeData`] are the shape of the store's `node` and
//! `edge` tables, and what the sync layer hands to the editor and the runner.
//! [`GraphDocument`] is the same rows as one JSON file, which is what the
//! editor's explicit Save/Load writes (`.zgh`).
//!
//! Pins and settings are not in here: both processes regenerate them from the
//! plugin instances, so a row carries only what a user decided -- type,
//! name, position, parent and the parameters typed into the node.

use serde::{Deserialize, Serialize};

/// A whole graph as one JSON document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphDocument {
    pub nodes: Vec<NodeData>,
    pub edges: Vec<EdgeData>,
}

/// One node as the store holds it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeData {
    pub id: u64,
    pub type_id: String,
    pub display_name: String,
    pub x: f32,
    pub y: f32,
    /// The container node this node lives inside, `0` for the root graph.
    /// Defaulted so a file that names no parent loads into the root graph.
    #[serde(default)]
    pub parent: u64,
    /// Setting values as the user typed them, by setting key.
    #[serde(default)]
    pub params: Vec<(String, String)>,
    /// The runner that executes this graph, as the `sha256:<hex>` fingerprint
    /// of its endpoint. Set on top-level graphs only; empty everywhere else.
    #[serde(default)]
    pub runner: String,
}

/// One edge as the store holds it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeData {
    pub id: u64,
    pub from_node: u64,
    pub from_pin: String,
    pub to_node: u64,
    pub to_pin: String,
}

impl GraphDocument {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }
}

impl Default for GraphDocument {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_json() {
        let doc = GraphDocument {
            nodes: vec![NodeData {
                id: 1,
                type_id: "transform.const_f64".to_string(),
                display_name: "Const A".to_string(),
                x: 100.0,
                y: 200.0,
                params: vec![("value".to_string(), "42".to_string())],
                parent: 0,
                runner: String::new(),
            }],
            edges: vec![EdgeData {
                id: 10,
                from_node: 1,
                from_pin: "value".to_string(),
                to_node: 2,
                to_pin: "a".to_string(),
            }],
        };

        let json = serde_json::to_string_pretty(&doc).unwrap();
        let loaded: GraphDocument = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.nodes.len(), 1);
        assert_eq!(loaded.edges.len(), 1);
        assert_eq!(loaded.nodes[0].type_id, "transform.const_f64");
        assert_eq!(loaded.nodes[0].params[0].1, "42");
    }
}
