use zeughaus_core::*;

pub struct ConstStringNode {
    value: String,
    pins: Vec<PinDefinition>,
}

impl ConstStringNode {
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            pins: vec![PinDefinition::output("value", Ty::Str)],
        }
    }
}

impl ExecutableNode for ConstStringNode {
    fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        ctx.emit_typed("value", self.value.clone());
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn set_parameter(&mut self, name: &str, value: Value) -> Result<()> {
        if name == "value"
            && let Some(s) = value.downcast_ref::<String>()
        {
            self.value = s.clone();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_string() {
        let mut node = ConstStringNode::new("hello");
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&InputSet::new(), &mut ctx).unwrap();
        assert_eq!(
            ctx.take_outputs()["value"].downcast_ref::<String>().unwrap(),
            "hello"
        );
    }

    #[test]
    fn set_parameter_updates() {
        let mut node = ConstStringNode::new("");
        node.set_parameter("value", Value::new("world".to_string())).unwrap();
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&InputSet::new(), &mut ctx).unwrap();
        assert_eq!(
            ctx.take_outputs()["value"].downcast_ref::<String>().unwrap(),
            "world"
        );
    }
}
