pub mod nodes;

use zeughaus_core::*;

use nodes::*;

pub struct TransformPlugin;

impl DomainPlugin for TransformPlugin {
    fn name(&self) -> &str {
        "transform"
    }

    fn node_catalog(&self) -> Vec<NodeDefinition> {
        vec![
            // Math
            catalog_entry("transform.const_f64", "Const (f64)", "Math", &ConstF64Node::new(0.0)),
            catalog_entry("transform.add", "Add", "Math", &AddNode::new()),
            catalog_entry("transform.subtract", "Subtract", "Math", &SubtractNode::new()),
            catalog_entry("transform.multiply", "Multiply", "Math", &MultiplyNode::new()),
            catalog_entry("transform.divide", "Divide", "Math", &DivideNode::new()),
            catalog_entry("transform.negate", "Negate", "Math", &NegateNode::new()),
            catalog_entry("transform.abs", "Abs", "Math", &AbsNode::new()),
            catalog_entry("transform.clamp", "Clamp", "Math", &ClampNode::new()),
            // Logic
            catalog_entry("transform.greater_than", "Greater Than", "Logic", &GreaterThanNode::new()),
            catalog_entry("transform.equal", "Equal", "Logic", &EqualNode::new()),
            catalog_entry("transform.select", "Select", "Logic", &SelectNode::new()),
            // Convert
            catalog_entry("transform.to_string", "To String", "Convert", &ToStringNode::new()),
            // Output
            catalog_entry("transform.display", "Display", "Output", &DisplayNode::new()),
        ]
    }

    fn create_node(&self, type_id: &str) -> Option<Box<dyn ExecutableNode>> {
        match type_id {
            "transform.const_f64" => Some(Box::new(ConstF64Node::new(0.0))),
            "transform.add" => Some(Box::new(AddNode::new())),
            "transform.subtract" => Some(Box::new(SubtractNode::new())),
            "transform.multiply" => Some(Box::new(MultiplyNode::new())),
            "transform.divide" => Some(Box::new(DivideNode::new())),
            "transform.negate" => Some(Box::new(NegateNode::new())),
            "transform.abs" => Some(Box::new(AbsNode::new())),
            "transform.clamp" => Some(Box::new(ClampNode::new())),
            "transform.greater_than" => Some(Box::new(GreaterThanNode::new())),
            "transform.equal" => Some(Box::new(EqualNode::new())),
            "transform.select" => Some(Box::new(SelectNode::new())),
            "transform.to_string" => Some(Box::new(ToStringNode::new())),
            "transform.display" => Some(Box::new(DisplayNode::new())),
            _ => None,
        }
    }
}

fn catalog_entry(
    type_id: &'static str,
    display_name: &'static str,
    category: &'static str,
    node: &dyn ExecutableNode,
) -> NodeDefinition {
    NodeDefinition {
        type_id,
        display_name,
        category,
        pins: node.pin_definitions().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_all_nodes() {
        let plugin = TransformPlugin;
        let catalog = plugin.node_catalog();
        assert_eq!(catalog.len(), 13);
    }

    #[test]
    fn create_all_catalog_nodes() {
        let plugin = TransformPlugin;
        for def in plugin.node_catalog() {
            assert!(
                plugin.create_node(def.type_id).is_some(),
                "Failed to create node: {}",
                def.type_id
            );
        }
    }

    #[test]
    fn create_unknown_returns_none() {
        let plugin = TransformPlugin;
        assert!(plugin.create_node("unknown.type").is_none());
    }
}
