use zeughaus_core::*;

pub struct NegateNode {
    pins: Vec<PinDefinition>,
}

impl Default for NegateNode {
    fn default() -> Self {
        Self::new()
    }
}

impl NegateNode {
    pub fn new() -> Self {
        Self {
            pins: vec![
                PinDefinition {
                    name: "input",
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

impl ExecutableNode for NegateNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let v: f64 = inputs.get("input").unwrap_or(0.0);
        ctx.emit_typed("result", -v);
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
    fn negates_positive() {
        let mut node = NegateNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(5.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<f64>(), Some(&-5.0));
    }

    #[test]
    fn negates_negative() {
        let mut node = NegateNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(-3.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<f64>(), Some(&3.0));
    }
}
