use zeughaus_core::*;

pub struct GreaterThanNode {
    pins: Vec<PinDefinition>,
}

impl Default for GreaterThanNode {
    fn default() -> Self {
        Self::new()
    }
}

impl GreaterThanNode {
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
                    type_name: "bool",
                },
            ],
        }
    }
}

impl ExecutableNode for GreaterThanNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let a: f64 = inputs.get("a").unwrap_or(0.0);
        let b: f64 = inputs.get("b").unwrap_or(0.0);
        ctx.emit_typed("result", a > b);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }
}

pub struct EqualNode {
    pins: Vec<PinDefinition>,
}

impl Default for EqualNode {
    fn default() -> Self {
        Self::new()
    }
}

impl EqualNode {
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
                    name: "epsilon",
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

impl ExecutableNode for EqualNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let a: f64 = inputs.get("a").unwrap_or(0.0);
        let b: f64 = inputs.get("b").unwrap_or(0.0);
        let eps: f64 = inputs.get("epsilon").unwrap_or(1e-10);
        ctx.emit_typed("result", (a - b).abs() < eps);
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
    fn greater_than_true() {
        let mut node = GreaterThanNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(5.0f64));
        inputs.insert("b", Value::new(3.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<bool>(), Some(&true));
    }

    #[test]
    fn greater_than_false() {
        let mut node = GreaterThanNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(2.0f64));
        inputs.insert("b", Value::new(3.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<bool>(), Some(&false));
    }

    #[test]
    fn equal_within_epsilon() {
        let mut node = EqualNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(1.0f64));
        inputs.insert("b", Value::new(1.00000000001f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<bool>(), Some(&true));
    }

    #[test]
    fn not_equal_outside_epsilon() {
        let mut node = EqualNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(1.0f64));
        inputs.insert("b", Value::new(2.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<bool>(), Some(&false));
    }
}
