//! LLM plugin for Zeughaus. Talks to LM Studio's OpenAI-compatible local
//! endpoint. Nodes operate on a Conversation value type: each node consumes a
//! conversation and emits it extended by one message, so prompt chains map
//! directly onto node chains.

pub mod client;
pub mod conversation;
pub mod nodes;

pub use conversation::{ChatMessage, Conversation, Role};

use zeughaus_core::*;

use nodes::*;

pub struct LlmPlugin;

impl DomainPlugin for LlmPlugin {
    fn name(&self) -> &str {
        "llm"
    }

    fn node_catalog(&self) -> Vec<NodeDefinition> {
        vec![
            catalog_entry(
                "llm.system",
                "System Message",
                "LLM",
                &SystemMessageNode::new(),
            ),
            catalog_entry("llm.user", "User Message", "LLM", &UserMessageNode::new()),
            catalog_entry("llm.chat", "Chat (LM Studio)", "LLM", &ChatNode::new()),
            catalog_entry("llm.last_reply", "Last Reply", "LLM", &LastReplyNode::new()),
            catalog_entry("llm.merge", "Merge", "LLM", &MergeNode::new()),
        ]
    }

    fn create_node(&self, type_id: &str) -> Option<Box<dyn ExecutableNode>> {
        match type_id {
            "llm.system" => Some(Box::new(SystemMessageNode::new())),
            "llm.user" => Some(Box::new(UserMessageNode::new())),
            "llm.chat" => Some(Box::new(ChatNode::new())),
            "llm.last_reply" => Some(Box::new(LastReplyNode::new())),
            "llm.merge" => Some(Box::new(MergeNode::new())),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_all_catalog_nodes() {
        let plugin = LlmPlugin;
        for def in plugin.node_catalog() {
            assert!(
                plugin.create_node(&def.type_id).is_some(),
                "failed to create: {}",
                def.type_id
            );
        }
    }

    #[test]
    fn chat_node_exposes_settings_in_catalog() {
        let plugin = LlmPlugin;
        let chat = plugin
            .node_catalog()
            .into_iter()
            .find(|d| &*d.type_id == "llm.chat")
            .unwrap();
        assert_eq!(chat.settings.len(), 2);
    }
}
