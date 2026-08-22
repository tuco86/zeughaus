use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::pin::PinDefinition;

/// Per-node configuration. `capture` is reserved for opt-in result
/// persistence to database (see DESIGN.md). Not yet implemented.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeConfig {
    pub capture: bool,
}

/// An editable text setting rendered directly inside the node widget.
/// Distinct from input pins: settings are node-local configuration the user
/// types (e.g. an LLM base URL or model name), persisted as node parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingDef {
    pub name: Arc<str>,
    pub default: Arc<str>,
    pub placeholder: Arc<str>,
    /// Hint for the editor to render a taller, multi-line field.
    pub multiline: bool,
}

impl SettingDef {
    pub fn new(name: impl Into<Arc<str>>, default: impl Into<Arc<str>>) -> Self {
        let default = default.into();
        Self {
            name: name.into(),
            placeholder: default.clone(),
            default,
            multiline: false,
        }
    }

    pub fn placeholder(mut self, placeholder: impl Into<Arc<str>>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    pub fn multiline(mut self) -> Self {
        self.multiline = true;
        self
    }
}

/// A node type in the catalog.
///
/// The identifying strings are owned rather than `&'static str`: node types are
/// not necessarily authored in Rust (a subgraph saved by the user is a node type
/// too), so the catalog has to be extensible at runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeDefinition {
    pub type_id: Arc<str>,
    pub display_name: Arc<str>,
    pub category: Arc<str>,
    pub pins: Vec<PinDefinition>,
    pub settings: Vec<SettingDef>,
}

/// Helper to build a NodeDefinition from a node instance.
pub fn catalog_entry(
    type_id: impl Into<Arc<str>>,
    display_name: impl Into<Arc<str>>,
    category: impl Into<Arc<str>>,
    node: &dyn crate::plugin::ExecutableNode,
) -> NodeDefinition {
    NodeDefinition {
        type_id: type_id.into(),
        display_name: display_name.into(),
        category: category.into(),
        pins: node.pin_definitions().to_vec(),
        settings: node.settings(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_defaults_to_its_value_as_placeholder() {
        let def = SettingDef::new("model", "qwen3");
        assert_eq!(&*def.placeholder, "qwen3");
        assert!(!def.multiline);

        let prompt = SettingDef::new("prompt", "").placeholder("ask...").multiline();
        assert_eq!(&*prompt.placeholder, "ask...");
        assert!(prompt.multiline);
    }
}
