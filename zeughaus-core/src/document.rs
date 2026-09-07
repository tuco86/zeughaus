//! Serializable graph document format for save/load.

use serde::{Deserialize, Serialize};

/// A complete graph document that can be serialized to/from JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphDocument {
    pub nodes: Vec<NodeData>,
    pub edges: Vec<EdgeData>,
}

/// Serializable node data (position, type, parameters).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeData {
    pub id: u64,
    pub type_id: String,
    pub display_name: String,
    pub x: f32,
    pub y: f32,
    /// The container node this node lives inside, `0` for the root graph.
    ///
    /// Defaulted so a document written before subgraphs existed still loads:
    /// every node in it belongs to the root graph, which is what `0` says.
    #[serde(default)]
    pub parent: u64,
    /// Serialized parameter values (name -> JSON value string).
    #[serde(default)]
    pub params: Vec<(String, String)>,
}

/// Serializable edge data.
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
