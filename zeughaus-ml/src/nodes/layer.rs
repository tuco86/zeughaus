//! Data-driven layer node. Every Keras layer node is structurally identical --
//! it takes a model in, appends one layer, and emits the model out -- so rather
//! than one near-identical file per layer, layers are declared as rows in the
//! `LAYERS` table and a single `LayerNode` is instantiated from a row.

use zeughaus_core::*;

use crate::model::{KerasModel, Layer};

/// Whether a parameter is a Python string (quoted in codegen) or a raw literal
/// (number, tuple, bool, identifier -- emitted verbatim).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    Str,
    Raw,
}

/// One configurable keyword argument of a layer.
#[derive(Debug, Clone, Copy)]
pub struct ParamDef {
    /// The Keras keyword argument name, e.g. "units" or "activation".
    pub name: &'static str,
    pub default: &'static str,
    pub placeholder: &'static str,
    pub ty: ParamType,
}

/// Static description of a layer node type.
#[derive(Debug, Clone, Copy)]
pub struct LayerSpec {
    pub type_id: &'static str,
    pub display_name: &'static str,
    /// The `keras.layers` class name, e.g. "Conv2D".
    pub keras_class: &'static str,
    pub params: &'static [ParamDef],
}

const fn p(name: &'static str, default: &'static str, placeholder: &'static str, ty: ParamType) -> ParamDef {
    ParamDef { name, default, placeholder, ty }
}

use ParamType::{Raw, Str};

/// The catalog of supported Keras layers. Add a layer by adding a row.
pub static LAYERS: &[LayerSpec] = &[
    LayerSpec {
        type_id: "ml.input",
        display_name: "Input",
        keras_class: "Input",
        params: &[p("shape", "(28, 28, 1)", "(28, 28, 1)", Raw)],
    },
    LayerSpec {
        type_id: "ml.dense",
        display_name: "Dense",
        keras_class: "Dense",
        params: &[
            p("units", "64", "units", Raw),
            p("activation", "relu", "relu / softmax / none", Str),
        ],
    },
    LayerSpec {
        type_id: "ml.conv2d",
        display_name: "Conv2D",
        keras_class: "Conv2D",
        params: &[
            p("filters", "32", "filters", Raw),
            p("kernel_size", "(3, 3)", "(3, 3)", Raw),
            p("strides", "", "(1, 1)", Raw),
            p("padding", "", "valid / same", Str),
            p("activation", "relu", "relu", Str),
        ],
    },
    LayerSpec {
        type_id: "ml.conv1d",
        display_name: "Conv1D",
        keras_class: "Conv1D",
        params: &[
            p("filters", "32", "filters", Raw),
            p("kernel_size", "3", "kernel size", Raw),
            p("activation", "relu", "relu", Str),
        ],
    },
    LayerSpec {
        type_id: "ml.maxpool2d",
        display_name: "MaxPooling2D",
        keras_class: "MaxPooling2D",
        params: &[p("pool_size", "(2, 2)", "(2, 2)", Raw)],
    },
    LayerSpec {
        type_id: "ml.avgpool2d",
        display_name: "AveragePooling2D",
        keras_class: "AveragePooling2D",
        params: &[p("pool_size", "(2, 2)", "(2, 2)", Raw)],
    },
    LayerSpec {
        type_id: "ml.global_avgpool2d",
        display_name: "GlobalAveragePooling2D",
        keras_class: "GlobalAveragePooling2D",
        params: &[],
    },
    LayerSpec {
        type_id: "ml.flatten",
        display_name: "Flatten",
        keras_class: "Flatten",
        params: &[],
    },
    LayerSpec {
        type_id: "ml.dropout",
        display_name: "Dropout",
        keras_class: "Dropout",
        params: &[p("rate", "0.5", "0.0 - 1.0", Raw)],
    },
    LayerSpec {
        type_id: "ml.batchnorm",
        display_name: "BatchNormalization",
        keras_class: "BatchNormalization",
        params: &[],
    },
    LayerSpec {
        type_id: "ml.layernorm",
        display_name: "LayerNormalization",
        keras_class: "LayerNormalization",
        params: &[],
    },
    LayerSpec {
        type_id: "ml.activation",
        display_name: "Activation",
        keras_class: "Activation",
        params: &[p("activation", "relu", "relu / sigmoid / tanh", Str)],
    },
    LayerSpec {
        type_id: "ml.lstm",
        display_name: "LSTM",
        keras_class: "LSTM",
        params: &[
            p("units", "64", "units", Raw),
            p("return_sequences", "", "True / False", Raw),
        ],
    },
    LayerSpec {
        type_id: "ml.gru",
        display_name: "GRU",
        keras_class: "GRU",
        params: &[
            p("units", "64", "units", Raw),
            p("return_sequences", "", "True / False", Raw),
        ],
    },
    LayerSpec {
        type_id: "ml.embedding",
        display_name: "Embedding",
        keras_class: "Embedding",
        params: &[
            p("input_dim", "10000", "vocab size", Raw),
            p("output_dim", "128", "embed dim", Raw),
        ],
    },
    LayerSpec {
        type_id: "ml.reshape",
        display_name: "Reshape",
        keras_class: "Reshape",
        params: &[p("target_shape", "(28, 28, 1)", "(28, 28, 1)", Raw)],
    },
];

pub fn spec(type_id: &str) -> Option<&'static LayerSpec> {
    LAYERS.iter().find(|s| s.type_id == type_id)
}

/// Renders a layer's keyword arguments from its param defs and current values:
/// string params are quoted, raw params emitted verbatim, blank params omitted
/// (so Keras applies its own default). Shared by layer and merge nodes.
pub(crate) fn build_kwargs(
    params: &[ParamDef],
    values: &[(&'static str, String)],
) -> Vec<(String, String)> {
    let mut kwargs = Vec::new();
    for def in params {
        let raw = values
            .iter()
            .find(|(n, _)| *n == def.name)
            .map(|(_, v)| v.trim())
            .unwrap_or("");
        if raw.is_empty() {
            continue;
        }
        let rendered = match def.ty {
            ParamType::Str => format!("'{raw}'"),
            ParamType::Raw => raw.to_string(),
        };
        kwargs.push((def.name.to_string(), rendered));
    }
    kwargs
}

/// Two pins shared by every layer node: model in (Sample), model out.
fn layer_pins() -> Vec<PinDefinition> {
    vec![
        PinDefinition {
            name: "model",
            direction: PinDirection::Input,
            data_mode: DataMode::Value,
            pin_kind: PinKind::Sample,
            type_name: "KerasModel",
        },
        PinDefinition {
            name: "out",
            direction: PinDirection::Output,
            data_mode: DataMode::Value,
            pin_kind: PinKind::Sample,
            type_name: "KerasModel",
        },
    ]
}

/// A single Keras layer in the graph. Built from a `LayerSpec` row; current
/// parameter values are stored per param name and overridden by the matching
/// input pin when wired (none are wired by default, but the mechanism mirrors
/// the rest of the codebase).
pub struct LayerNode {
    spec: &'static LayerSpec,
    values: Vec<(&'static str, String)>,
    pins: Vec<PinDefinition>,
}

impl LayerNode {
    pub fn new(spec: &'static LayerSpec) -> Self {
        let values = spec.params.iter().map(|d| (d.name, d.default.to_string())).collect();
        Self { spec, values, pins: layer_pins() }
    }

    /// Builds the rendered `Layer` from this node's params and current values.
    fn build_layer(&self) -> Layer {
        Layer {
            keras_class: self.spec.keras_class.to_string(),
            kwargs: build_kwargs(self.spec.params, &self.values),
        }
    }
}

impl ExecutableNode for LayerNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let model: KerasModel = inputs.get("model").unwrap_or_default();
        // The producing node's id is the step's stable identity, so a layer
        // feeding two branches stays a single step when they later merge.
        let out = model.with_layer(ctx.source_node.0, self.build_layer());
        ctx.emit_typed("out", out);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
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

    #[test]
    fn dense_builds_with_defaults() {
        let node = LayerNode::new(spec("ml.dense").unwrap());
        let layer = node.build_layer();
        assert_eq!(layer.render(), "layers.Dense(units=64, activation='relu')");
    }

    #[test]
    fn blank_param_is_omitted() {
        let node = LayerNode::new(spec("ml.conv2d").unwrap());
        // strides + padding default to blank -> not emitted
        let rendered = node.build_layer().render();
        assert!(!rendered.contains("strides"));
        assert!(!rendered.contains("padding"));
        assert!(rendered.contains("filters=32"));
        assert!(rendered.contains("kernel_size=(3, 3)"));
    }

    #[test]
    fn setting_overrides_param() {
        let mut node = LayerNode::new(spec("ml.dense").unwrap());
        node.set_parameter("units", Value::new("10".to_string())).unwrap();
        node.set_parameter("activation", Value::new("softmax".to_string())).unwrap();
        assert_eq!(node.build_layer().render(), "layers.Dense(units=10, activation='softmax')");
    }

    #[test]
    fn execute_appends_layer_to_model() {
        let mut node = LayerNode::new(spec("ml.flatten").unwrap());
        let prior = KerasModel::new().with_layer(
            1,
            Layer {
                keras_class: "Input".to_string(),
                kwargs: vec![("shape".to_string(), "(28, 28)".to_string())],
            },
        );
        let mut inputs = InputSet::new();
        inputs.insert("model", Value::new(prior));
        let mut ctx = NodeContext::new(NodeId(7), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let out = ctx.take_outputs();
        let model = out["out"].downcast_ref::<KerasModel>().unwrap();
        assert_eq!(model.steps.len(), 2);
        assert_eq!(model.steps[1].layer.keras_class, "Flatten");
        // Step id comes from the executing node; wired to the prior tip.
        assert_eq!(model.steps[1].id, 7);
        assert_eq!(model.steps[1].inputs, vec![1]);
        assert_eq!(model.output, Some(7));
    }

    #[test]
    fn empty_params_layer_has_no_settings() {
        let node = LayerNode::new(spec("ml.flatten").unwrap());
        assert!(node.settings().is_empty());
    }
}
