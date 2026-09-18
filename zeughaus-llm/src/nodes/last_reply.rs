use zeughaus_core::*;

use crate::conversation::Conversation;

/// Extracts the content of the last message in a conversation as a plain
/// string. Useful for feeding an LLM reply into non-LLM nodes (display, etc.).
pub struct LastReplyNode {
    pins: Vec<PinDefinition>,
}

impl Default for LastReplyNode {
    fn default() -> Self {
        Self::new()
    }
}

impl LastReplyNode {
    pub fn new() -> Self {
        Self {
            pins: vec![
                PinDefinition::input("conv", Ty::of::<Conversation>(), PinKind::Sample),
                PinDefinition::output("text", Ty::Str),
            ],
        }
    }
}

impl ExecutableNode for LastReplyNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let conv: Conversation = inputs.get("conv").unwrap_or_default();
        let text = conv.last().map(|m| m.content.clone()).unwrap_or_default();
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
    use crate::conversation::{ChatMessage, Role};

    #[test]
    fn extracts_last_message() {
        let conv = Conversation::new()
            .with(ChatMessage::new(Role::User, "q"))
            .with(ChatMessage::new(Role::Assistant, "a"));
        let mut inputs = InputSet::new();
        inputs.insert("conv", Value::new(conv));
        let mut node = LastReplyNode::new();
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(
            ctx.take_outputs()["text"].downcast_ref::<String>().unwrap(),
            "a"
        );
    }
}
