use std::collections::HashMap;

use zeughaus_core::*;

use crate::client::{self, DEFAULT_BASE_URL};
use crate::conversation::Conversation;

/// The blocking part of a chat node: model resolution + the HTTP call. Runs on
/// a background thread via the executor's deferred-work mechanism so the UI
/// stays responsive during generation.
struct ChatRequest {
    base_url: String,
    model: String,
    conv: Conversation,
}

impl AsyncWork for ChatRequest {
    fn run(self: Box<Self>) -> Result<HashMap<String, Value>> {
        // Resolve an empty model to the first one LM Studio reports loaded.
        let model = if self.model.trim().is_empty() {
            client::list_models(&self.base_url)
                .ok()
                .and_then(|m| m.into_iter().next())
                .unwrap_or_default()
        } else {
            self.model.clone()
        };

        let (out, result) = client::chat_and_append(&self.base_url, &model, &self.conv)
            .map_err(ZeughausError::ExecutionFailed)?;

        let mut outputs: HashMap<String, Value> = HashMap::new();
        outputs.insert("reply".to_string(), Value::new(result.content.clone()));
        outputs.insert("tok_per_s".to_string(), Value::new(result.tokens_per_sec()));
        outputs.insert("out".to_string(), Value::new(out));
        Ok(outputs)
    }
}

/// Sends the incoming conversation to LM Studio and appends the assistant
/// reply. Outputs the extended conversation (for further chaining), the reply
/// text on its own, and generation throughput in tokens/sec.
pub struct ChatNode {
    base_url: String,
    model: String,
    pins: Vec<PinDefinition>,
}

impl Default for ChatNode {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatNode {
    pub fn new() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            model: String::new(),
            pins: vec![
                PinDefinition {
                    name: "conv",
                    direction: PinDirection::Input,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Trigger,
                    type_name: "Conversation",
                },
                PinDefinition {
                    name: "out",
                    direction: PinDirection::Output,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "Conversation",
                },
                PinDefinition {
                    name: "reply",
                    direction: PinDirection::Output,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "String",
                },
                PinDefinition {
                    name: "tok_per_s",
                    direction: PinDirection::Output,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "f64",
                },
            ],
        }
    }
}

impl ExecutableNode for ChatNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let conv: Conversation = inputs.get("conv").unwrap_or_default();
        if conv.is_empty() {
            return Err(ZeughausError::ExecutionFailed(
                "chat node has no conversation input".to_string(),
            ));
        }

        let base_url = if self.base_url.trim().is_empty() {
            DEFAULT_BASE_URL.to_string()
        } else {
            self.base_url.clone()
        };

        // Defer the blocking HTTP request to a background thread. Outputs are
        // applied later via the executor's async result delivery.
        ctx.defer(Box::new(ChatRequest {
            base_url,
            model: self.model.clone(),
            conv,
        }));
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn settings(&self) -> Vec<SettingDef> {
        vec![
            SettingDef {
                name: "base_url",
                default: DEFAULT_BASE_URL,
                placeholder: DEFAULT_BASE_URL,
                multiline: false,
            },
            SettingDef {
                name: "model",
                default: "",
                placeholder: "(first loaded model)",
                multiline: false,
            },
        ]
    }

    fn set_parameter(&mut self, name: &str, value: Value) -> Result<()> {
        if let Some(s) = value.downcast_ref::<String>() {
            match name {
                "base_url" => self.base_url = s.clone(),
                "model" => self.model = s.clone(),
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_conversation_errors() {
        let mut node = ChatNode::new();
        let mut ctx = NodeContext::new(NodeId(1), 0);
        let err = node.execute(&InputSet::new(), &mut ctx);
        assert!(err.is_err());
    }

    #[test]
    fn settings_have_base_url_and_model() {
        let node = ChatNode::new();
        let names: Vec<_> = node.settings().iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["base_url", "model"]);
    }
}
