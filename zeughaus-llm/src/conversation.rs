//! The Conversation value type that flows through LLM nodes.
//!
//! A Conversation is an ordered list of chat messages. LLM nodes consume a
//! Conversation and emit the same Conversation with one more message appended
//! (the assistant reply). This makes prompt chains expressible as node chains.

use serde::{Deserialize, Serialize};
use zeughaus_core::{Ty, Typed};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    /// OpenAI-compatible role string used in the wire protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }

    pub fn from_wire(s: &str) -> Role {
        match s {
            "system" => Role::System,
            "assistant" => Role::Assistant,
            _ => Role::User,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

impl ChatMessage {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
}

/// An ordered chat history. Cloned freely as it flows through edges.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Conversation {
    pub messages: Vec<ChatMessage>,
}

impl Conversation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a new conversation with `msg` appended (immutable-style chaining).
    pub fn with(&self, msg: ChatMessage) -> Self {
        let mut next = self.clone();
        next.messages.push(msg);
        next
    }

    pub fn last(&self) -> Option<&ChatMessage> {
        self.messages.last()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

/// `Conversation` is a nominal plugin type: its meaning is the chat protocol it
/// implements, not its field layout, so it travels as an opaque `Ty`.
impl Typed for Conversation {
    fn ty() -> Ty {
        Ty::opaque("Conversation")
    }
}

impl std::fmt::Display for Conversation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Compact one-line summary for the node value display row.
        match self.last() {
            Some(m) => {
                let preview: String = m.content.chars().take(40).collect();
                write!(f, "[{}] {}: {}", self.messages.len(), m.role.as_str(), preview)
            }
            None => write!(f, "[empty]"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_appends_without_mutating() {
        let base = Conversation::new().with(ChatMessage::new(Role::User, "hi"));
        let extended = base.with(ChatMessage::new(Role::Assistant, "hello"));
        assert_eq!(base.messages.len(), 1);
        assert_eq!(extended.messages.len(), 2);
    }

    #[test]
    fn role_round_trip() {
        assert_eq!(Role::from_wire(Role::Assistant.as_str()), Role::Assistant);
        assert_eq!(Role::from_wire("user"), Role::User);
        assert_eq!(Role::from_wire("unknown"), Role::User);
    }
}
