use zeughaus_core::*;

/// Selects between two values based on a boolean condition.
/// If condition is true, outputs the "true_val" input; otherwise "false_val".
pub struct SelectNode {
    pins: Vec<PinDefinition>,
}

impl Default for SelectNode {
    fn default() -> Self {
        Self::new()
    }
}

impl SelectNode {
    pub fn new() -> Self {
        Self {
            pins: vec![
                PinDefinition::input("condition", Ty::Bool, PinKind::Trigger),
                PinDefinition::input("true_val", Ty::Float, PinKind::Sample),
                PinDefinition::input("false_val", Ty::Float, PinKind::Sample),
                PinDefinition::output("result", Ty::Float),
            ],
        }
    }
}

impl ExecutableNode for SelectNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let cond: bool = inputs.get("condition").unwrap_or(false);
        let true_val: f64 = inputs.get("true_val").unwrap_or(0.0);
        let false_val: f64 = inputs.get("false_val").unwrap_or(0.0);
        ctx.emit_typed("result", if cond { true_val } else { false_val });
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
    fn selects_true_branch() {
        let mut node = SelectNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("condition", Value::new(true));
        inputs.insert("true_val", Value::new(42.0f64));
        inputs.insert("false_val", Value::new(0.0f64));
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<f64>(), Some(&42.0));
    }

    #[test]
    fn selects_false_branch() {
        let mut node = SelectNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("condition", Value::new(false));
        inputs.insert("true_val", Value::new(42.0f64));
        inputs.insert("false_val", Value::new(99.0f64));
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<f64>(), Some(&99.0));
    }

    #[test]
    fn default_condition_is_false() {
        let mut node = SelectNode::new();
        let inputs = InputSet::new();
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["result"].downcast_ref::<f64>(), Some(&0.0));
    }
}
