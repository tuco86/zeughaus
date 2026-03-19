use zeughaus_core::*;

pub struct DisplayNode {
    last_value: Option<String>,
    pins: Vec<PinDefinition>,
}

impl Default for DisplayNode {
    fn default() -> Self {
        Self::new()
    }
}

impl DisplayNode {
    pub fn new() -> Self {
        Self {
            last_value: None,
            pins: vec![PinDefinition {
                name: "input",
                direction: PinDirection::Input,
                data_mode: DataMode::Value,
                pin_kind: PinKind::Trigger,
                type_name: "any",
            }],
        }
    }

    pub fn last_value(&self) -> Option<&str> {
        self.last_value.as_deref()
    }
}

impl ExecutableNode for DisplayNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        if let Some(val) = inputs.get_value("input") {
            let text = if let Some(f) = val.downcast_ref::<f64>() {
                format!("{f}")
            } else if let Some(s) = val.downcast_ref::<String>() {
                s.clone()
            } else if let Some(b) = val.downcast_ref::<bool>() {
                format!("{b}")
            } else if let Some(i) = val.downcast_ref::<i64>() {
                format!("{i}")
            } else {
                format!("<{}>", val.type_name())
            };
            self.last_value = Some(text);
        }
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
    fn stores_last_value() {
        let mut node = DisplayNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(42.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(node.last_value(), Some("42"));
    }

    #[test]
    fn no_input_keeps_none() {
        let mut node = DisplayNode::new();
        let inputs = InputSet::new();
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(node.last_value(), None);
    }
}
