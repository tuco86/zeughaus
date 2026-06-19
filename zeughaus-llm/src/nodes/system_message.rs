use zeughaus_core::*;

use crate::conversation::{ChatMessage, Conversation, Role};

/// Appends a system message to an (optional) incoming conversation.
/// Typically the start of a chain, but accepts an input so system prompts can
/// be inserted mid-chain too.
pub struct SystemMessageNode {
    text: String,
    pins: Vec<PinDefinition>,
}

impl Default for SystemMessageNode {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemMessageNode {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            pins: vec![
                PinDefinition {
                    name: "conv",
                    direction: PinDirection::Input,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "Conversation",
                },
                PinDefinition {
                    name: "out",
                    direction: PinDirection::Output,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "Conversation",
                },
            ],
        }
    }
}

impl ExecutableNode for SystemMessageNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let conv: Conversation = inputs.get("conv").unwrap_or_default();
        let out = conv.with(ChatMessage::new(Role::System, &self.text));
        ctx.emit_typed("out", out);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn settings(&self) -> Vec<SettingDef> {
        vec![SettingDef {
            name: "text",
            default: "You are a helpful assistant.",
            placeholder: "system prompt",
            multiline: true,
        }]
    }

    fn set_parameter(&mut self, name: &str, value: Value) -> Result<()> {
        if name == "text"
            && let Some(s) = value.downcast_ref::<String>()
        {
            self.text = s.clone();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_system_message() {
        let mut node = SystemMessageNode::new();
        node.set_parameter("text", Value::new("be terse".to_string()))
            .unwrap();
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&InputSet::new(), &mut ctx).unwrap();
        let out = ctx.take_outputs();
        let conv = out["out"].downcast_ref::<Conversation>().unwrap();
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.messages[0].role, Role::System);
        assert_eq!(conv.messages[0].content, "be terse");
    }
}
