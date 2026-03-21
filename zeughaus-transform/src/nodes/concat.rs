use zeughaus_core::*;

/// Concatenates two strings with an optional separator.
pub struct ConcatNode {
    pins: Vec<PinDefinition>,
}

impl Default for ConcatNode {
    fn default() -> Self {
        Self::new()
    }
}

impl ConcatNode {
    pub fn new() -> Self {
        Self {
            pins: vec![
                PinDefinition {
                    name: "a",
                    direction: PinDirection::Input,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Trigger,
                    type_name: "String",
                },
                PinDefinition {
                    name: "b",
                    direction: PinDirection::Input,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Trigger,
                    type_name: "String",
                },
                PinDefinition {
                    name: "sep",
                    direction: PinDirection::Input,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "String",
                },
                PinDefinition {
                    name: "result",
                    direction: PinDirection::Output,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "String",
                },
            ],
        }
    }
}

impl ExecutableNode for ConcatNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let a: String = inputs.get("a").unwrap_or_default();
        let b: String = inputs.get("b").unwrap_or_default();
        let sep: String = inputs.get("sep").unwrap_or_default();
        let result = if sep.is_empty() {
            format!("{a}{b}")
        } else {
            format!("{a}{sep}{b}")
        };
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
    fn concat_without_separator() {
        let mut node = ConcatNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new("hello".to_string()));
        inputs.insert("b", Value::new("world".to_string()));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(
            ctx.take_outputs()["result"].downcast_ref::<String>().unwrap(),
            "helloworld"
        );
    }

    #[test]
    fn concat_with_separator() {
        let mut node = ConcatNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new("hello".to_string()));
        inputs.insert("b", Value::new("world".to_string()));
        inputs.insert("sep", Value::new(" ".to_string()));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(
            ctx.take_outputs()["result"].downcast_ref::<String>().unwrap(),
            "hello world"
        );
    }
}
