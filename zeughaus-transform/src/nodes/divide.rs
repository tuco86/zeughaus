use zeughaus_core::*;

pub struct DivideNode {
    pins: Vec<PinDefinition>,
}

impl Default for DivideNode {
    fn default() -> Self {
        Self::new()
    }
}

impl DivideNode {
    pub fn new() -> Self {
        Self {
            pins: vec![
                PinDefinition {
                    name: "a",
                    direction: PinDirection::Input,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Trigger,
                    type_name: "f64",
                },
                PinDefinition {
                    name: "b",
                    direction: PinDirection::Input,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Trigger,
                    type_name: "f64",
                },
                PinDefinition {
                    name: "result",
                    direction: PinDirection::Output,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "f64",
                },
            ],
        }
    }
}

impl ExecutableNode for DivideNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let a: f64 = inputs.get("a").unwrap_or(0.0);
        let b: f64 = inputs.get("b").unwrap_or(1.0);
        let result = if b == 0.0 { f64::INFINITY } else { a / b };
        ctx.emit_typed("result", result);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divides_two_values() {
        let mut node = DivideNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(10.0f64));
        inputs.insert("b", Value::new(4.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<f64>(), Some(&2.5));
    }

    #[test]
    fn divide_by_zero_returns_infinity() {
        let mut node = DivideNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(10.0f64));
        inputs.insert("b", Value::new(0.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        let val = outputs["result"].downcast_ref::<f64>().unwrap();
        assert!(val.is_infinite());
    }

    #[test]
    fn missing_divisor_defaults_to_one() {
        let mut node = DivideNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(7.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<f64>(), Some(&7.0));
    }
}
