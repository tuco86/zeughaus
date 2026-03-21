use zeughaus_core::*;

pub struct ModuloNode {
    pins: Vec<PinDefinition>,
}

impl Default for ModuloNode {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuloNode {
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

impl ExecutableNode for ModuloNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let a: f64 = inputs.get("a").unwrap_or(0.0);
        let b: f64 = inputs.get("b").unwrap_or(1.0);
        let result = if b == 0.0 { f64::NAN } else { a % b };
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
    fn modulo_basic() {
        let mut node = ModuloNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(10.0f64));
        inputs.insert("b", Value::new(3.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<f64>(), Some(&1.0));
    }

    #[test]
    fn modulo_by_zero_is_nan() {
        let mut node = ModuloNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(10.0f64));
        inputs.insert("b", Value::new(0.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert!(outputs["result"].downcast_ref::<f64>().unwrap().is_nan());
    }
}
