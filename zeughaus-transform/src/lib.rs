pub mod nodes;

use zeughaus_core::*;

use nodes::{AddNode, ConstF64Node, DisplayNode, MultiplyNode, ToStringNode};

pub struct TransformPlugin;

impl DomainPlugin for TransformPlugin {
    fn name(&self) -> &str {
        "transform"
    }

    fn node_catalog(&self) -> Vec<NodeDefinition> {
        vec![
            NodeDefinition {
                type_id: "transform.const_f64",
                display_name: "Const (f64)",
                category: "Math",
                pins: ConstF64Node::new(0.0).pin_definitions().to_vec(),
            },
            NodeDefinition {
                type_id: "transform.add",
                display_name: "Add",
                category: "Math",
                pins: AddNode::new().pin_definitions().to_vec(),
            },
            NodeDefinition {
                type_id: "transform.multiply",
                display_name: "Multiply",
                category: "Math",
                pins: MultiplyNode::new().pin_definitions().to_vec(),
            },
            NodeDefinition {
                type_id: "transform.to_string",
                display_name: "To String",
                category: "Convert",
                pins: ToStringNode::new().pin_definitions().to_vec(),
            },
            NodeDefinition {
                type_id: "transform.display",
                display_name: "Display",
                category: "Output",
                pins: DisplayNode::new().pin_definitions().to_vec(),
            },
        ]
    }

    fn create_node(&self, type_id: &str) -> Option<Box<dyn ExecutableNode>> {
        match type_id {
            "transform.const_f64" => Some(Box::new(ConstF64Node::new(0.0))),
            "transform.add" => Some(Box::new(AddNode::new())),
            "transform.multiply" => Some(Box::new(MultiplyNode::new())),
            "transform.to_string" => Some(Box::new(ToStringNode::new())),
            "transform.display" => Some(Box::new(DisplayNode::new())),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_all_nodes() {
        let plugin = TransformPlugin;
        let catalog = plugin.node_catalog();
        assert_eq!(catalog.len(), 5);
    }

    #[test]
    fn create_known_node() {
        let plugin = TransformPlugin;
        assert!(plugin.create_node("transform.add").is_some());
        assert!(plugin.create_node("transform.const_f64").is_some());
        assert!(plugin.create_node("transform.display").is_some());
    }

    #[test]
    fn create_unknown_returns_none() {
        let plugin = TransformPlugin;
        assert!(plugin.create_node("unknown.type").is_none());
    }
}
