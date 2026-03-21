use zeughaus_core::*;

pub struct PowerNode {
    pins: Vec<PinDefinition>,
}

impl Default for PowerNode {
    fn default() -> Self {
        Self::new()
    }
}

impl PowerNode {
    pub fn new() -> Self {
        Self {
            pins: vec![
                PinDefinition {
                    name: "base",
                    direction: PinDirection::Input,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Trigger,
                    type_name: "f64",
                },
                PinDefinition {
                    name: "exp",
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

impl ExecutableNode for PowerNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let base: f64 = inputs.get("base").unwrap_or(0.0);
        let exp: f64 = inputs.get("exp").unwrap_or(1.0);
        ctx.emit_typed("result", base.powf(exp));
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
    fn power_basic() {
        let mut node = PowerNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("base", Value::new(2.0f64));
        inputs.insert("exp", Value::new(10.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<f64>(), Some(&1024.0));
    }

    #[test]
    fn power_fractional() {
        let mut node = PowerNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("base", Value::new(9.0f64));
        inputs.insert("exp", Value::new(0.5f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<f64>(), Some(&3.0));
    }
}
