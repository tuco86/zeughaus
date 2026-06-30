//! Merge layer nodes (Concatenate, Add, ...). Unlike the single-input layer
//! nodes, a merge node has two model inputs `a` and `b` and joins the two
//! branches into one DAG before appending the merge layer. For more than two
//! inputs, chain merge nodes.

use zeughaus_core::*;

use super::layer::{build_kwargs, ParamDef, ParamType};
use crate::model::{KerasModel, Layer};

/// Static description of a merge node type. Mirrors `LayerSpec` but for the
/// two-input merge layers.
#[derive(Debug, Clone, Copy)]
pub struct MergeSpec {
    pub type_id: &'static str,
    pub display_name: &'static str,
    pub keras_class: &'static str,
    pub params: &'static [ParamDef],
}

const fn p(name: &'static str, default: &'static str, placeholder: &'static str, ty: ParamType) -> ParamDef {
    ParamDef { name, default, placeholder, ty }
}

/// The catalog of supported Keras merge layers. Most take no parameters; the
/// merge semantics come from the class itself.
pub static MERGES: &[MergeSpec] = &[
    MergeSpec {
        type_id: "ml.concatenate",
        display_name: "Concatenate",
        keras_class: "Concatenate",
        params: &[p("axis", "", "-1", ParamType::Raw)],
    },
    MergeSpec { type_id: "ml.add", display_name: "Add", keras_class: "Add", params: &[] },
    MergeSpec { type_id: "ml.subtract", display_name: "Subtract", keras_class: "Subtract", params: &[] },
    MergeSpec { type_id: "ml.multiply", display_name: "Multiply", keras_class: "Multiply", params: &[] },
    MergeSpec { type_id: "ml.average", display_name: "Average", keras_class: "Average", params: &[] },
    MergeSpec { type_id: "ml.maximum", display_name: "Maximum", keras_class: "Maximum", params: &[] },
    MergeSpec { type_id: "ml.minimum", display_name: "Minimum", keras_class: "Minimum", params: &[] },
    MergeSpec {
        type_id: "ml.dot",
        display_name: "Dot",
        keras_class: "Dot",
        params: &[p("axes", "-1", "-1 or (1, 2)", ParamType::Raw)],
    },
];

pub fn merge_spec(type_id: &str) -> Option<&'static MergeSpec> {
    MERGES.iter().find(|s| s.type_id == type_id)
}

/// Stable static names for the variadic inputs, in order. A merge can join up
/// to 26 branches; inputs grow as you fill them.
const LETTERS: [&str; 26] = [
    "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s",
    "t", "u", "v", "w", "x", "y", "z",
];

fn input_pin(name: &'static str) -> PinDefinition {
    PinDefinition {
        name,
        direction: PinDirection::Input,
        data_mode: DataMode::Value,
        pin_kind: PinKind::Sample,
        type_name: "KerasModel",
    }
}

/// Builds `input_count` ordered input pins (a, b, c, ...) plus the `out` pin.
fn merge_pins(input_count: usize) -> Vec<PinDefinition> {
    let mut pins: Vec<PinDefinition> = LETTERS[..input_count].iter().map(|n| input_pin(n)).collect();
    pins.push(PinDefinition {
        name: "out",
        direction: PinDirection::Output,
        data_mode: DataMode::Value,
        pin_kind: PinKind::Sample,
        type_name: "KerasModel",
    });
    pins
}

/// A merge layer joining two branches. Both inputs must carry a non-empty model
/// (a dangling branch is a user error and is reported, not panicked on).
pub struct MergeNode {
    spec: &'static MergeSpec,
    values: Vec<(&'static str, String)>,
    pins: Vec<PinDefinition>,
}

impl MergeNode {
    pub fn new(spec: &'static MergeSpec) -> Self {
        let values = spec.params.iter().map(|d| (d.name, d.default.to_string())).collect();
        // Start with two inputs (a, b); more appear as they are filled.
        Self { spec, values, pins: merge_pins(2) }
    }

    /// Number of input pins currently exposed (all pins minus the `out` pin).
    fn input_count(&self) -> usize {
        self.pins.len() - 1
    }
}

impl ExecutableNode for MergeNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        // Collect every connected, non-empty branch in pin order (a, b, c, ...).
        let branches: Vec<KerasModel> = LETTERS
            .iter()
            .copied()
            .filter_map(|n| inputs.get::<KerasModel>(n))
            .filter(|m| !m.is_empty())
            .collect();
        if branches.len() < 2 {
            return Err(ZeughausError::ExecutionFailed(format!(
                "{} needs at least two connected model inputs",
                self.spec.display_name
            )));
        }
        let layer = Layer {
            keras_class: self.spec.keras_class.to_string(),
            kwargs: build_kwargs(self.spec.params, &self.values),
        };
        let refs: Vec<&KerasModel> = branches.iter().collect();
        let out = KerasModel::join(ctx.source_node.0, layer, &refs);
        ctx.emit_typed("out", out);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn sync_arity(&mut self, connected: &[&str]) -> bool {
        // Highest connected input index, then keep exactly one spare empty input
        // after it (at least 2 inputs, at most 26 -- the a..z cap).
        let last = LETTERS
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, n)| connected.contains(n))
            .map(|(i, _)| i)
            .max();
        let desired = match last {
            Some(i) => (i + 2).clamp(2, 26),
            None => 2,
        };
        if desired != self.input_count() {
            self.pins = merge_pins(desired);
            true
        } else {
            false
        }
    }

    fn settings(&self) -> Vec<SettingDef> {
        self.spec
            .params
            .iter()
            .map(|d| SettingDef {
                name: d.name,
                default: d.default,
                placeholder: d.placeholder,
                multiline: false,
            })
            .collect()
    }

    fn set_parameter(&mut self, name: &str, value: Value) -> Result<()> {
        if let Some(s) = value.downcast_ref::<String>()
            && let Some(entry) = self.values.iter_mut().find(|(n, _)| *n == name)
        {
            entry.1 = s.clone();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input_model(id: u64) -> KerasModel {
        KerasModel::new().with_layer(
            id,
            Layer { keras_class: "Input".to_string(), kwargs: vec![("shape".to_string(), "(4,)".to_string())] },
        )
    }

    #[test]
    fn join_merges_two_branches() {
        let mut node = MergeNode::new(merge_spec("ml.concatenate").unwrap());
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(input_model(1)));
        inputs.insert("b", Value::new(input_model(2)));
        let mut ctx = NodeContext::new(NodeId(3), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let out = ctx.take_outputs();
        let model = out["out"].downcast_ref::<KerasModel>().unwrap();
        assert_eq!(model.output, Some(3));
        let merge_step = model.steps.iter().find(|s| s.id == 3).unwrap();
        assert_eq!(merge_step.inputs, vec![1, 2]);
        assert_eq!(merge_step.layer.keras_class, "Concatenate");
    }

    #[test]
    fn empty_branch_errors() {
        let mut node = MergeNode::new(merge_spec("ml.add").unwrap());
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(input_model(1)));
        // b missing -> empty
        let mut ctx = NodeContext::new(NodeId(3), 0);
        assert!(node.execute(&inputs, &mut ctx).is_err());
    }

    #[test]
    fn dot_carries_axes_param() {
        let mut node = MergeNode::new(merge_spec("ml.dot").unwrap());
        node.set_parameter("axes", Value::new("(1, 2)".to_string())).unwrap();
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(input_model(1)));
        inputs.insert("b", Value::new(input_model(2)));
        let mut ctx = NodeContext::new(NodeId(3), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let model = ctx.take_outputs()["out"].downcast_ref::<KerasModel>().unwrap().clone();
        let step = model.steps.iter().find(|s| s.id == 3).unwrap();
        assert_eq!(step.layer.render(), "layers.Dot(axes=(1, 2))");
    }

    #[test]
    fn arity_grows_when_last_input_filled() {
        let mut node = MergeNode::new(merge_spec("ml.concatenate").unwrap());
        let names = |n: &MergeNode| n.pin_definitions().iter().map(|p| p.name).collect::<Vec<_>>();
        assert_eq!(names(&node), vec!["a", "b", "out"]);
        // "a" filled but "b" is still the spare -> no growth.
        assert!(!node.sync_arity(&["a"]));
        assert_eq!(names(&node), vec!["a", "b", "out"]);
        // Filling the last input "b" reveals "c".
        assert!(node.sync_arity(&["a", "b"]));
        assert_eq!(names(&node), vec!["a", "b", "c", "out"]);
    }

    #[test]
    fn arity_shrinks_on_disconnect() {
        let mut node = MergeNode::new(merge_spec("ml.add").unwrap());
        node.sync_arity(&["a", "b", "c"]); // grows to a, b, c, d
        assert_eq!(node.pin_definitions().len(), 5);
        // Disconnecting "c": last connected is "b" -> back to a, b, c (one spare).
        assert!(node.sync_arity(&["a", "b"]));
        let names: Vec<_> = node.pin_definitions().iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["a", "b", "c", "out"]);
    }

    #[test]
    fn execute_joins_three_branches_in_order() {
        let mut node = MergeNode::new(merge_spec("ml.concatenate").unwrap());
        node.sync_arity(&["a", "b", "c"]);
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(input_model(1)));
        inputs.insert("b", Value::new(input_model(2)));
        inputs.insert("c", Value::new(input_model(3)));
        let mut ctx = NodeContext::new(NodeId(9), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let model = ctx.take_outputs()["out"].downcast_ref::<KerasModel>().unwrap().clone();
        let step = model.steps.iter().find(|s| s.id == 9).unwrap();
        assert_eq!(step.inputs, vec![1, 2, 3]);
    }
}
