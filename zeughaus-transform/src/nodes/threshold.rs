use zeughaus_core::*;

/// Converts f64 to bool based on a threshold.
/// result = input >= threshold
pub struct ThresholdNode {
    pins: Vec<PinDefinition>,
}

impl Default for ThresholdNode {
    fn default() -> Self {
        Self::new()
    }
}

impl ThresholdNode {
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
                    name: "threshold",
                    direction: PinDirection::Input,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "f64",
                },
                PinDefinition {
                    name: "result",
                    direction: PinDirection::Output,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "bool",
                },
            ],
        }
    }
}

impl ExecutableNode for ThresholdNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let v: f64 = inputs.get("input").unwrap_or(0.0);
        let t: f64 = inputs.get("threshold").unwrap_or(0.5);
        ctx.emit_typed("result", v >= t);
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
    fn above_threshold() {
        let mut node = ThresholdNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(0.8f64));
        inputs.insert("threshold", Value::new(0.5f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(ctx.take_outputs()["result"].downcast_ref::<bool>(), Some(&true));
    }

    #[test]
    fn below_threshold() {
        let mut node = ThresholdNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(0.3f64));
        inputs.insert("threshold", Value::new(0.5f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(ctx.take_outputs()["result"].downcast_ref::<bool>(), Some(&false));
    }

    #[test]
    fn at_threshold_is_true() {
        let mut node = ThresholdNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(0.5f64));
        inputs.insert("threshold", Value::new(0.5f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(ctx.take_outputs()["result"].downcast_ref::<bool>(), Some(&true));
    }
}
