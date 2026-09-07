use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::pin::PinDefinition;

/// Per-node configuration. `capture` is reserved for opt-in result
/// persistence to database (see DESIGN.md). Not yet implemented.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeConfig {
    pub capture: bool,
}

/// How the editor renders a setting.
///
/// A hint, not a type: the value is always the same string that reaches the
/// store and the node's `set_parameter`. Nothing behind the editor learns
/// which widget the user typed into.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SettingKind {
    /// One line of text.
    Text,
    /// A taller, multi-line text field.
    Multiline,
    /// The node's own name, drawn as an editable title above its body instead
    /// of as one labeled field among others. For a node that *is* the thing it
    /// names -- a table, a boundary pin -- the name is the heading, not a
    /// setting to scroll past.
    Title,
    /// A list of `name:type` rows -- one per line of the value -- each with a
    /// name field, a type choice out of `types`, and a way to remove it.
    ///
    /// Deliberately generic. The value stays the newline-separated text a
    /// multiline field would hold, so a node parses it exactly as before, and
    /// the type vocabulary travels in the setting rather than being read from
    /// the plugin that declared it -- which is what lets the browser editor
    /// render the rows for a plugin it cannot even link.
    Fields { types: Vec<String> },
}

/// An editable setting rendered directly inside the node widget.
/// Distinct from input pins: settings are node-local configuration the user
/// types (e.g. an LLM base URL or model name), persisted as node parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingDef {
    pub name: Arc<str>,
    pub default: Arc<str>,
    pub placeholder: Arc<str>,
    /// Which widget the editor draws for this setting.
    pub kind: SettingKind,
}

impl SettingDef {
    pub fn new(name: impl Into<Arc<str>>, default: impl Into<Arc<str>>) -> Self {
        let default = default.into();
        Self {
            name: name.into(),
            placeholder: default.clone(),
            default,
            kind: SettingKind::Text,
        }
    }

    pub fn placeholder(mut self, placeholder: impl Into<Arc<str>>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    pub fn multiline(mut self) -> Self {
        self.kind = SettingKind::Multiline;
        self
    }

    /// Renders as the node's editable title.
    pub fn title(mut self) -> Self {
        self.kind = SettingKind::Title;
        self
    }

    /// Renders as a row editor over the value's `name:type` lines, offering
    /// `types` per row.
    pub fn fields<S: Into<String>>(mut self, types: impl IntoIterator<Item = S>) -> Self {
        self.kind = SettingKind::Fields {
            types: types.into_iter().map(Into::into).collect(),
        };
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
    /// Whether this node type holds a subgraph.
    ///
    /// A container has no pins of its own: the editor synthesizes them from the
    /// boundary nodes inside it, and offers a way in. Nothing else in the
    /// system distinguishes it -- the executor stays flat, because a boundary
    /// node is an ordinary passthrough.
    pub container: bool,
}

impl NodeDefinition {
    /// Marks this type as holding a subgraph.
    pub fn container(mut self) -> Self {
        self.container = true;
        self
    }
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
        container: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_defaults_to_its_value_as_placeholder() {
        let def = SettingDef::new("model", "qwen3");
        assert_eq!(&*def.placeholder, "qwen3");
        assert_eq!(def.kind, SettingKind::Text);

        let prompt = SettingDef::new("prompt", "").placeholder("ask...").multiline();
        assert_eq!(&*prompt.placeholder, "ask...");
        assert_eq!(prompt.kind, SettingKind::Multiline);

        let columns = SettingDef::new("columns", "id:int").fields(["int", "str"]);
        assert_eq!(
            columns.kind,
            SettingKind::Fields {
                types: vec!["int".to_string(), "str".to_string()]
            }
        );
    }
}
