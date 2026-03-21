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
                PinDefinition {
                    name: "input",
                    direction: PinDirection::Input,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Trigger,
                    type_name: "any",
                },
                PinDefinition {
                    name: "text",
                    direction: PinDirection::Output,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "String",
                },
            ],
        }
    }
}

impl ExecutableNode for ToStringNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let text = if let Some(val) = inputs.get_value("input") {
            if let Some(f) = val.downcast_ref::<f64>() {
                format!("{f}")
            } else if let Some(s) = val.downcast_ref::<String>() {
                s.clone()
            } else if let Some(b) = val.downcast_ref::<bool>() {
                format!("{b}")
            } else if let Some(i) = val.downcast_ref::<i64>() {
                format!("{i}")
            } else {
                format!("<{}>", val.type_name())
            }
        } else {
            String::new()
        };
        ctx.emit_typed("text", text);
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
        inputs.insert("input", Value::new(3.14f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(
            outputs["text"].downcast_ref::<String>().unwrap(),
            "3.14"
        );
    }

    #[test]
    fn converts_string() {
        let mut node = ToStringNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("input", Value::new(String::from("hello")));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(
            outputs["text"].downcast_ref::<String>().unwrap(),
            "hello"
        );
    }
}
