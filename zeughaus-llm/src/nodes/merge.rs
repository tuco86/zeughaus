use zeughaus_core::*;

use crate::conversation::Conversation;

/// Concatenates two conversations (a then b) into one. Lets separately built
/// branches be joined before feeding a chat node.
pub struct MergeNode {
    pins: Vec<PinDefinition>,
}

impl Default for MergeNode {
    fn default() -> Self {
        Self::new()
    }
}

impl MergeNode {
    pub fn new() -> Self {
        Self {
            pins: vec![
                PinDefinition::input("a", Ty::of::<Conversation>(), PinKind::Sample),
                PinDefinition::input("b", Ty::of::<Conversation>(), PinKind::Sample),
                PinDefinition::output("out", Ty::of::<Conversation>()),
            ],
        }
    }
}

impl ExecutableNode for MergeNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let mut out: Conversation = inputs.get("a").unwrap_or_default();
        let b: Conversation = inputs.get("b").unwrap_or_default();
        out.messages.extend(b.messages);
        ctx.emit_typed("out", out);
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
    fn concatenates_in_order() {
        let a = Conversation::new().with(ChatMessage::new(Role::System, "s"));
        let b = Conversation::new().with(ChatMessage::new(Role::User, "u"));
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(a));
        inputs.insert("b", Value::new(b));
        let mut node = MergeNode::new();
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).unwrap();
        let out = ctx.take_outputs();
        let conv = out["out"].downcast_ref::<Conversation>().unwrap();
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages[0].role, Role::System);
        assert_eq!(conv.messages[1].role, Role::User);
    }
}
