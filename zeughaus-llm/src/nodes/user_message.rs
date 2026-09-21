use zeughaus_core::*;

use crate::conversation::{ChatMessage, Conversation, Role};

/// Appends a user message to an (optional) incoming conversation. The message
/// content comes from the `text` input pin when connected, otherwise from the
/// in-node `text` setting. This lets prompts be either static or wired from
/// upstream string nodes.
pub struct UserMessageNode {
    text: String,
    pins: Vec<PinDefinition>,
}

impl Default for UserMessageNode {
    fn default() -> Self {
        Self::new()
    }
}

impl UserMessageNode {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            pins: vec![
                PinDefinition::input("conv", Ty::of::<Conversation>(), PinKind::Sample),
                PinDefinition::input("text", Ty::Str, PinKind::Sample),
                PinDefinition::output("out", Ty::of::<Conversation>()),
            ],
        }
    }
}

impl ExecutableNode for UserMessageNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let conv: Conversation = inputs.get("conv").unwrap_or_default();
        let content: String = inputs.get("text").unwrap_or_else(|| self.text.clone());
        let out = conv.with(ChatMessage::new(Role::User, content));
        ctx.emit_typed("out", out);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn settings(&self) -> Vec<SettingDef> {
        vec![
            SettingDef::new("text", "")
                .placeholder("user message")
                .multiline(),
        ]
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
    fn pin_overrides_setting() {
        let mut node = UserMessageNode::new();
        node.set_parameter("text", Value::new("from setting".to_string()))
            .unwrap();
        let mut inputs = InputSet::new();
        inputs.insert("text", Value::new("from pin".to_string()));
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).unwrap();
        let out = ctx.take_outputs();
        let conv = out["out"].downcast_ref::<Conversation>().unwrap();
        assert_eq!(conv.messages[0].content, "from pin");
    }

    #[test]
    fn appends_to_existing_conversation() {
        let mut node = UserMessageNode::new();
        node.set_parameter("text", Value::new("second".to_string()))
            .unwrap();
        let prior = Conversation::new().with(ChatMessage::new(Role::System, "first"));
        let mut inputs = InputSet::new();
        inputs.insert("conv", Value::new(prior));
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).unwrap();
        let out = ctx.take_outputs();
        let conv = out["out"].downcast_ref::<Conversation>().unwrap();
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[1].role, Role::User);
    }
}
