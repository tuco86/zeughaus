use zeughaus_core::*;

pub struct NotNode {
    pins: Vec<PinDefinition>,
}

impl Default for NotNode {
    fn default() -> Self {
        Self::new()
    }
}

impl NotNode {
    pub fn new() -> Self {
        Self {
            pins: vec![
                PinDefinition::input("input", Ty::Bool, PinKind::Trigger),
                PinDefinition::output("result", Ty::Bool),
            ],
        }
    }
}

impl ExecutableNode for NotNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let v: bool = inputs.get("input").unwrap_or(false);
        ctx.emit_typed("result", !v);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }
}

pub struct AndNode {
    pins: Vec<PinDefinition>,
}

impl Default for AndNode {
    fn default() -> Self {
        Self::new()
    }
}

impl AndNode {
    pub fn new() -> Self {
        Self {
            pins: vec![
                PinDefinition::input("a", Ty::Bool, PinKind::Trigger),
                PinDefinition::input("b", Ty::Bool, PinKind::Trigger),
                PinDefinition::output("result", Ty::Bool),
            ],
        }
    }
}

impl ExecutableNode for AndNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let a: bool = inputs.get("a").unwrap_or(false);
        let b: bool = inputs.get("b").unwrap_or(false);
        ctx.emit_typed("result", a && b);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }
}

pub struct OrNode {
    pins: Vec<PinDefinition>,
}

impl Default for OrNode {
    fn default() -> Self {
        Self::new()
    }
}

impl OrNode {
    pub fn new() -> Self {
        Self {
            pins: vec![
                PinDefinition::input("a", Ty::Bool, PinKind::Trigger),
                PinDefinition::input("b", Ty::Bool, PinKind::Trigger),
                PinDefinition::output("result", Ty::Bool),
            ],
        }
    }
}

impl ExecutableNode for OrNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let a: bool = inputs.get("a").unwrap_or(false);
        let b: bool = inputs.get("b").unwrap_or(false);
        ctx.emit_typed("result", a || b);
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
    fn not_true() {
        let mut node = NotNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(true));
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(
            ctx.take_outputs()["result"].downcast_ref::<bool>(),
            Some(&false)
        );
    }

    #[test]
    fn not_false() {
        let mut node = NotNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(false));
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(
            ctx.take_outputs()["result"].downcast_ref::<bool>(),
            Some(&true)
        );
    }

    #[test]
    fn and_truth_table() {
        for (a, b, expected) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, true),
        ] {
            let mut node = AndNode::new();
            let mut inputs = InputSet::new();
            inputs.insert("a", Value::new(a));
            inputs.insert("b", Value::new(b));
            let mut ctx = NodeContext::new(NodeId(1));
            node.execute(&inputs, &mut ctx).unwrap();
            assert_eq!(
                ctx.take_outputs()["result"].downcast_ref::<bool>(),
                Some(&expected),
                "AND({a}, {b}) should be {expected}"
            );
        }
    }

    #[test]
    fn or_truth_table() {
        for (a, b, expected) in [
            (false, false, false),
            (true, false, true),
            (false, true, true),
            (true, true, true),
        ] {
            let mut node = OrNode::new();
            let mut inputs = InputSet::new();
            inputs.insert("a", Value::new(a));
            inputs.insert("b", Value::new(b));
            let mut ctx = NodeContext::new(NodeId(1));
            node.execute(&inputs, &mut ctx).unwrap();
            assert_eq!(
                ctx.take_outputs()["result"].downcast_ref::<bool>(),
                Some(&expected),
                "OR({a}, {b}) should be {expected}"
            );
        }
    }
}
