use zeughaus_core::*;

pub struct ConstF64Node {
    value: f64,
    pins: Vec<PinDefinition>,
}

impl ConstF64Node {
    pub fn new(value: f64) -> Self {
        Self {
            value,
            pins: vec![PinDefinition::output("value", Ty::Float)],
        }
    }
}

impl ExecutableNode for ConstF64Node {
    fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        ctx.emit_typed("value", self.value);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn set_parameter(&mut self, name: &str, value: Value) -> Result<()> {
        if name == "value"
            && let Some(v) = value.downcast_ref::<f64>()
        {
            self.value = *v;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_configured_value() {
        let mut node = ConstF64Node::new(42.0);
        let inputs = InputSet::new();
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).unwrap();
        ctx.flush(); // already flushed inside, but harmless
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["value"].downcast_ref::<f64>(), Some(&42.0));
    }

    #[test]
    fn set_parameter_updates_value() {
        let mut node = ConstF64Node::new(0.0);
        node.set_parameter("value", Value::new(99.0f64)).unwrap();
        let inputs = InputSet::new();
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["value"].downcast_ref::<f64>(), Some(&99.0));
    }
}
