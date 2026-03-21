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
            // Constants
            catalog_entry("transform.const_f64", "Const (f64)", "Const", &ConstF64Node::new(0.0)),
            catalog_entry("transform.const_bool", "Const (bool)", "Const", &ConstBoolNode::new(false)),
            catalog_entry("transform.const_string", "Const (String)", "Const", &ConstStringNode::new("")),
            // Math
            catalog_entry("transform.add", "Add", "Math", &AddNode::new()),
            catalog_entry("transform.subtract", "Subtract", "Math", &SubtractNode::new()),
            catalog_entry("transform.multiply", "Multiply", "Math", &MultiplyNode::new()),
            catalog_entry("transform.divide", "Divide", "Math", &DivideNode::new()),
            catalog_entry("transform.negate", "Negate", "Math", &NegateNode::new()),
            catalog_entry("transform.abs", "Abs", "Math", &AbsNode::new()),
            catalog_entry("transform.clamp", "Clamp", "Math", &ClampNode::new()),
            catalog_entry("transform.modulo", "Modulo", "Math", &ModuloNode::new()),
            catalog_entry("transform.power", "Power", "Math", &PowerNode::new()),
            catalog_entry("transform.min", "Min", "Math", &MinNode::new()),
            catalog_entry("transform.max", "Max", "Math", &MaxNode::new()),
            catalog_entry("transform.lerp", "Lerp", "Math", &LerpNode::new()),
            // Trig
            catalog_entry("transform.sin", "Sin", "Trig", &SinNode::new()),
            catalog_entry("transform.cos", "Cos", "Trig", &CosNode::new()),
            catalog_entry("transform.tan", "Tan", "Trig", &TanNode::new()),
            catalog_entry("transform.sqrt", "Sqrt", "Trig", &SqrtNode::new()),
            catalog_entry("transform.floor", "Floor", "Trig", &FloorNode::new()),
            catalog_entry("transform.ceil", "Ceil", "Trig", &CeilNode::new()),
            catalog_entry("transform.round", "Round", "Trig", &RoundNode::new()),
            catalog_entry("transform.log2", "Log2", "Trig", &Log2Node::new()),
            catalog_entry("transform.ln", "Ln", "Trig", &LnNode::new()),
            // Logic
            catalog_entry("transform.greater_than", "Greater Than", "Logic", &GreaterThanNode::new()),
            catalog_entry("transform.equal", "Equal", "Logic", &EqualNode::new()),
            catalog_entry("transform.select", "Select", "Logic", &SelectNode::new()),
            catalog_entry("transform.not", "Not", "Logic", &NotNode::new()),
            catalog_entry("transform.and", "And", "Logic", &AndNode::new()),
            catalog_entry("transform.or", "Or", "Logic", &OrNode::new()),
            // Utility
            catalog_entry("transform.map_range", "Map Range", "Utility", &MapRangeNode::new()),
            // String
            catalog_entry("transform.to_string", "To String", "String", &ToStringNode::new()),
            catalog_entry("transform.concat", "Concat", "String", &ConcatNode::new()),
            catalog_entry("transform.string_len", "String Length", "String", &StringLenNode::new()),
            // Output
            catalog_entry("transform.display", "Display", "Output", &DisplayNode::new()),
        ]
    }

    fn create_node(&self, type_id: &str) -> Option<Box<dyn ExecutableNode>> {
        match type_id {
            "transform.const_f64" => Some(Box::new(ConstF64Node::new(0.0))),
            "transform.const_bool" => Some(Box::new(ConstBoolNode::new(false))),
            "transform.const_string" => Some(Box::new(ConstStringNode::new(""))),
            "transform.add" => Some(Box::new(AddNode::new())),
            "transform.subtract" => Some(Box::new(SubtractNode::new())),
            "transform.multiply" => Some(Box::new(MultiplyNode::new())),
            "transform.divide" => Some(Box::new(DivideNode::new())),
            "transform.negate" => Some(Box::new(NegateNode::new())),
            "transform.abs" => Some(Box::new(AbsNode::new())),
            "transform.clamp" => Some(Box::new(ClampNode::new())),
            "transform.modulo" => Some(Box::new(ModuloNode::new())),
            "transform.power" => Some(Box::new(PowerNode::new())),
            "transform.min" => Some(Box::new(MinNode::new())),
            "transform.max" => Some(Box::new(MaxNode::new())),
            "transform.lerp" => Some(Box::new(LerpNode::new())),
            "transform.sin" => Some(Box::new(SinNode::new())),
            "transform.cos" => Some(Box::new(CosNode::new())),
            "transform.tan" => Some(Box::new(TanNode::new())),
            "transform.sqrt" => Some(Box::new(SqrtNode::new())),
            "transform.floor" => Some(Box::new(FloorNode::new())),
            "transform.ceil" => Some(Box::new(CeilNode::new())),
            "transform.round" => Some(Box::new(RoundNode::new())),
            "transform.log2" => Some(Box::new(Log2Node::new())),
            "transform.ln" => Some(Box::new(LnNode::new())),
            "transform.greater_than" => Some(Box::new(GreaterThanNode::new())),
            "transform.equal" => Some(Box::new(EqualNode::new())),
            "transform.select" => Some(Box::new(SelectNode::new())),
            "transform.not" => Some(Box::new(NotNode::new())),
            "transform.and" => Some(Box::new(AndNode::new())),
            "transform.or" => Some(Box::new(OrNode::new())),
            "transform.map_range" => Some(Box::new(MapRangeNode::new())),
            "transform.to_string" => Some(Box::new(ToStringNode::new())),
            "transform.concat" => Some(Box::new(ConcatNode::new())),
            "transform.string_len" => Some(Box::new(StringLenNode::new())),
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
        assert_eq!(catalog.len(), 35);
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
