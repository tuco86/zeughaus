use zeughaus_core::*;

/// Accumulates (sums) all incoming values. Stateful node.
/// Each execution adds the input to the running total.
/// Set parameter "reset" to true to reset to 0.
pub struct AccumulatorNode {
    total: f64,
    pins: Vec<PinDefinition>,
}

impl Default for AccumulatorNode {
    fn default() -> Self {
        Self::new()
    }
}

impl AccumulatorNode {
    pub fn new() -> Self {
        Self {
            total: 0.0,
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

impl ExecutableNode for AccumulatorNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let v: f64 = inputs.get("input").unwrap_or(0.0);
        self.total += v;
        ctx.emit_typed("result", self.total);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn set_parameter(&mut self, name: &str, _value: Value) -> Result<()> {
        if name == "reset" {
            self.total = 0.0;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_values() {
        let mut node = AccumulatorNode::new();
        let mut ctx = NodeContext::new(NodeId(1), 0);

        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(5.0f64));
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(ctx.take_outputs()["result"].downcast_ref::<f64>(), Some(&5.0));

        let mut ctx = NodeContext::new(NodeId(1), 1);
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(3.0f64));
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(ctx.take_outputs()["result"].downcast_ref::<f64>(), Some(&8.0));
    }

    #[test]
    fn reset_clears_total() {
        let mut node = AccumulatorNode::new();
        let mut ctx = NodeContext::new(NodeId(1), 0);
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(10.0f64));
        node.execute(&inputs, &mut ctx).unwrap();

        node.set_parameter("reset", Value::new(true)).unwrap();

        let mut ctx = NodeContext::new(NodeId(1), 1);
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(1.0f64));
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(ctx.take_outputs()["result"].downcast_ref::<f64>(), Some(&1.0));
    }
}
