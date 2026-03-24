use serde::{Deserialize, Serialize};

use crate::pin::PinDefinition;

/// Per-node configuration. `capture` is reserved for opt-in result
/// persistence to database (see DESIGN.md). Not yet implemented.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeConfig {
    pub capture: bool,
}

#[derive(Debug, Clone)]
pub struct NodeDefinition {
    pub type_id: &'static str,
    pub display_name: &'static str,
    pub category: &'static str,
    pub pins: Vec<PinDefinition>,
}

/// Helper to build a NodeDefinition from a node instance.
pub fn catalog_entry(
    type_id: &'static str,
    display_name: &'static str,
    category: &'static str,
    node: &dyn crate::plugin::ExecutableNode,
) -> NodeDefinition {
    NodeDefinition {
        type_id,
        display_name,
        category,
        pins: node.pin_definitions().to_vec(),
    }
}
