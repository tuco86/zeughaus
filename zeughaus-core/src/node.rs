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

/// The `name:type` rows of a [`SettingKind::Fields`] value, as slices of it.
///
/// Borrowed rather than owned so a row can be handed straight to a text
/// input, and lenient rather than validating: a line whose type is half-typed
/// is still a row the user is editing and must not vanish under the cursor.
/// A line with no `:` at all is a name with no type yet.
pub fn field_rows(text: &str) -> Vec<(&str, &str)> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| match line.split_once(':') {
            Some((name, ty)) => (name.trim(), ty.trim()),
            None => (line.trim(), ""),
        })
        .collect()
}

/// The single field a field-list edit renamed, as `(old, new)`.
///
/// Recognised deliberately narrowly: the same number of rows, exactly one
/// position whose name changed, that row's type unchanged, and neither name
/// empty. A row added, removed or reordered, two names changed at once, or a
/// name changed together with its type, is not a rename.
///
/// The asymmetry is on purpose, and both readers depend on it. Guessing
/// "rename" where the user removed one field and added another would move a
/// foreign key onto a field nobody pointed it at -- and, for a node that
/// keeps its schema in a file, rename a column that holds someone else's
/// data. Guessing "not a rename" costs a wire that is redrawn in a second.
///
/// Lives here rather than in either caller because this is the format
/// [`SettingKind::Fields`] declares: the editor reads it to keep a renamed
/// field's relations, and a node reads it to rename the column in its file.
pub fn renamed_field(before: &str, after: &str) -> Option<(String, String)> {
    let (before, after) = (field_rows(before), field_rows(after));
    if before.len() != after.len() {
        return None;
    }
    let mut changed = before
        .iter()
        .zip(&after)
        .filter(|((old, _), (new, _))| old != new);
    let ((old, old_ty), (new, new_ty)) = changed.next()?;
    if changed.next().is_some() || old.is_empty() || new.is_empty() || old_ty != new_ty {
        return None;
    }
    Some((old.to_string(), new.to_string()))
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

    /// A rename has to be recognised so the field's relations survive it, and
    /// only recognised when it really is one: mistaking a remove-plus-add for
    /// a rename moves a foreign key onto a field nobody pointed it at.
    #[test]
    fn one_name_changed_in_place_is_a_rename_and_nothing_else_is() {
        let before = "id:int\ncustomer_id:int\ntotal:float";
        assert_eq!(
            renamed_field(before, "id:int\ncust_id:int\ntotal:float"),
            Some(("customer_id".to_string(), "cust_id".to_string()))
        );

        // A type change on its own leaves every name where it was.
        assert_eq!(
            renamed_field(before, "id:int\ncustomer_id:str\ntotal:float"),
            None
        );
        // Nothing changed.
        assert_eq!(renamed_field(before, before), None);
        // A row added or removed: the rows no longer line up.
        assert_eq!(renamed_field(before, "id:int\ncustomer_id:int"), None);
        assert_eq!(
            renamed_field(before, "id:int\ncustomer_id:int\ntotal:float\nnote:str"),
            None
        );
        // Two names at once, and a reorder, are not one rename.
        assert_eq!(renamed_field(before, "id:int\ncust:int\nsum:float"), None);
        assert_eq!(
            renamed_field(before, "customer_id:int\nid:int\ntotal:float"),
            None
        );
        // A name cleared to nothing is a row being retyped, not a rename to
        // the empty pin.
        assert_eq!(renamed_field(before, "id:int\n:int\ntotal:float"), None);
        // A name AND its type changed at once: the row is not the same field
        // under another name, and renaming a column of another type in a file
        // would move data nobody asked to move.
        assert_eq!(
            renamed_field(before, "id:int\ncust_id:str\ntotal:float"),
            None
        );
    }

    /// A half-typed row is still a row: the field whose type the user has not
    /// chosen yet must not disappear from under the cursor.
    #[test]
    fn a_field_row_survives_without_a_type() {
        assert_eq!(
            field_rows("id:int\nname\n\n  \ntag:str"),
            vec![("id", "int"), ("name", ""), ("tag", "str")]
        );
    }
}
