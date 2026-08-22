use zeughaus_core::*;

pub struct StringLenNode {
    pins: Vec<PinDefinition>,
}

impl Default for StringLenNode {
    fn default() -> Self {
        Self::new()
    }
}

impl StringLenNode {
    pub fn new() -> Self {
        Self {
            pins: vec![
                PinDefinition::input("input", Ty::Str, PinKind::Trigger),
                PinDefinition::output("result", Ty::Float),
            ],
        }
    }
}

impl ExecutableNode for StringLenNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let s: String = inputs.get("input").unwrap_or_default();
        ctx.emit_typed("result", s.len() as f64);
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
    fn len_of_hello() {
        let mut node = StringLenNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new("hello".to_string()));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(ctx.take_outputs()["result"].downcast_ref::<f64>(), Some(&5.0));
    }

    #[test]
    fn len_of_empty() {
        let mut node = StringLenNode::new();
        let inputs = InputSet::new();
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(ctx.take_outputs()["result"].downcast_ref::<f64>(), Some(&0.0));
    }
}
