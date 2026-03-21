use zeughaus_core::*;

pub struct ClampNode {
    pins: Vec<PinDefinition>,
}

impl Default for ClampNode {
    fn default() -> Self {
        Self::new()
    }
}

impl ClampNode {
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
                    name: "min",
                    direction: PinDirection::Input,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "f64",
                },
                PinDefinition {
                    name: "max",
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

impl ExecutableNode for ClampNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let v: f64 = inputs.get("input").unwrap_or(0.0);
        let min: f64 = inputs.get("min").unwrap_or(f64::NEG_INFINITY);
        let max: f64 = inputs.get("max").unwrap_or(f64::INFINITY);
        ctx.emit_typed("result", v.clamp(min, max));
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
    fn clamps_to_range() {
        let mut node = ClampNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(15.0f64));
        inputs.insert("min", Value::new(0.0f64));
        inputs.insert("max", Value::new(10.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<f64>(), Some(&10.0));
    }

    #[test]
    fn within_range_unchanged() {
        let mut node = ClampNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(5.0f64));
        inputs.insert("min", Value::new(0.0f64));
        inputs.insert("max", Value::new(10.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<f64>(), Some(&5.0));
    }

    #[test]
    fn clamps_below_min() {
        let mut node = ClampNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(-5.0f64));
        inputs.insert("min", Value::new(0.0f64));
        inputs.insert("max", Value::new(10.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<f64>(), Some(&0.0));
    }
}
