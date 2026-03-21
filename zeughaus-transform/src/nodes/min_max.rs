use zeughaus_core::*;

pub struct MinNode {
    pins: Vec<PinDefinition>,
}

impl Default for MinNode {
    fn default() -> Self {
        Self::new()
    }
}

impl MinNode {
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

impl ExecutableNode for MinNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let a: f64 = inputs.get("a").unwrap_or(0.0);
        let b: f64 = inputs.get("b").unwrap_or(0.0);
        ctx.emit_typed("result", a.min(b));
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }
}

pub struct MaxNode {
    pins: Vec<PinDefinition>,
}

impl Default for MaxNode {
    fn default() -> Self {
        Self::new()
    }
}

impl MaxNode {
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

impl ExecutableNode for MaxNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let a: f64 = inputs.get("a").unwrap_or(0.0);
        let b: f64 = inputs.get("b").unwrap_or(0.0);
        ctx.emit_typed("result", a.max(b));
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
    fn min_returns_smaller() {
        let mut node = MinNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(3.0f64));
        inputs.insert("b", Value::new(7.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<f64>(), Some(&3.0));
    }

    #[test]
    fn max_returns_larger() {
        let mut node = MaxNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(3.0f64));
        inputs.insert("b", Value::new(7.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<f64>(), Some(&7.0));
    }
}
