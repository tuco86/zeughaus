use zeughaus_core::*;

pub struct ToStringNode {
    pins: Vec<PinDefinition>,
}

impl Default for ToStringNode {
    fn default() -> Self {
        Self::new()
    }
}

impl ToStringNode {
    pub fn new() -> Self {
        Self {
            pins: vec![
                PinDefinition::input("input", Ty::Any, PinKind::Trigger),
                PinDefinition::output("result", Ty::Str),
            ],
        }
    }
}

impl ExecutableNode for ToStringNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let text = if let Some(val) = inputs.get_value("input") {
            val.to_string()
        } else {
            String::new()
        };
        ctx.emit_typed("result", text);
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
    fn converts_f64() {
        let mut node = ToStringNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(2.5f64));
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<String>().unwrap(), "2.5");
    }

    #[test]
    fn converts_string() {
        let mut node = ToStringNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(String::from("hello")));
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<String>().unwrap(), "hello");
    }
}
