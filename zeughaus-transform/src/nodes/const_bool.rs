use zeughaus_core::*;

pub struct ConstBoolNode {
    value: bool,
    pins: Vec<PinDefinition>,
}

impl ConstBoolNode {
    pub fn new(value: bool) -> Self {
        Self {
            value,
            pins: vec![PinDefinition::output("value", Ty::Bool)],
        }
    }
}

impl ExecutableNode for ConstBoolNode {
    fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        ctx.emit_typed("value", self.value);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn set_parameter(&mut self, name: &str, value: Value) -> Result<()> {
        if name == "value" {
            if let Some(v) = value.downcast_ref::<bool>() {
                self.value = *v;
            } else if let Some(s) = value.downcast_ref::<String>() {
                self.value = s == "true" || s == "1";
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_true() {
        let mut node = ConstBoolNode::new(true);
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&InputSet::new(), &mut ctx).unwrap();
        assert_eq!(
            ctx.take_outputs()["value"].downcast_ref::<bool>(),
            Some(&true)
        );
    }

    #[test]
    fn set_parameter_toggles() {
        let mut node = ConstBoolNode::new(false);
        node.set_parameter("value", Value::new(true)).unwrap();
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&InputSet::new(), &mut ctx).unwrap();
        assert_eq!(
            ctx.take_outputs()["value"].downcast_ref::<bool>(),
            Some(&true)
        );
    }

    #[test]
    fn set_parameter_from_string() {
        let mut node = ConstBoolNode::new(false);
        node.set_parameter("value", Value::new("true".to_string()))
            .unwrap();
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&InputSet::new(), &mut ctx).unwrap();
        assert_eq!(
            ctx.take_outputs()["value"].downcast_ref::<bool>(),
            Some(&true)
        );
    }
}
