use zeughaus_core::*;

/// Linear interpolation: result = a + (b - a) * t
pub struct LerpNode {
    pins: Vec<PinDefinition>,
}

impl Default for LerpNode {
    fn default() -> Self {
        Self::new()
    }
}

impl LerpNode {
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
                    name: "t",
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
                    type_name: "f64",
                },
            ],
        }
    }
}

impl ExecutableNode for LerpNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let a: f64 = inputs.get("a").unwrap_or(0.0);
        let b: f64 = inputs.get("b").unwrap_or(1.0);
        let t: f64 = inputs.get("t").unwrap_or(0.5);
        ctx.emit_typed("result", a + (b - a) * t);
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
    fn lerp_at_zero() {
        let mut node = LerpNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(10.0f64));
        inputs.insert("b", Value::new(20.0f64));
        inputs.insert("t", Value::new(0.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<f64>(), Some(&10.0));
    }

    #[test]
    fn lerp_at_one() {
        let mut node = LerpNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(10.0f64));
        inputs.insert("b", Value::new(20.0f64));
        inputs.insert("t", Value::new(1.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<f64>(), Some(&20.0));
    }

    #[test]
    fn lerp_at_half() {
        let mut node = LerpNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(0.0f64));
        inputs.insert("b", Value::new(100.0f64));
        inputs.insert("t", Value::new(0.5f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<f64>(), Some(&50.0));
    }
}
