use zeughaus_core::*;

use crate::model::{KerasModel, keras_model_ty};

/// Terminal node of an ML graph: renders the incoming model to a runnable
/// Keras (TensorFlow) Python program and emits it as a String on the `code`
/// pin. The code is also shown live in the node's value row, so editing any
/// upstream layer updates the export immediately.
pub struct ExportNode {
    pins: Vec<PinDefinition>,
}

impl Default for ExportNode {
    fn default() -> Self {
        Self::new()
    }
}

impl ExportNode {
    pub fn new() -> Self {
        Self {
            pins: vec![
                PinDefinition::input("model", keras_model_ty(), PinKind::Trigger),
                PinDefinition::output("code", Ty::Str),
            ],
        }
    }
}

impl ExecutableNode for ExportNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let model: KerasModel = inputs.get("model").ok_or_else(|| {
            ZeughausError::ExecutionFailed("export node has no model input".to_string())
        })?;

        if model.is_empty() {
            return Err(ZeughausError::ExecutionFailed(
                "model is empty: add at least one layer before exporting".to_string(),
            ));
        }

        // Boundary validation: the functional model is built from
        // keras.Model(inputs=[roots], ...), so every root (a step with no
        // incoming edge) must be an Input layer -- only Input yields a
        // KerasTensor. Any other root would produce a layer object, not a
        // tensor, and keras.Model would reject it at runtime.
        if let Some(bad) = model
            .roots()
            .iter()
            .find(|s| s.layer.keras_class != "Input")
        {
            return Err(ZeughausError::ExecutionFailed(format!(
                "every input branch must start with an Input layer, found {}",
                bad.layer.keras_class
            )));
        }

        ctx.emit_typed("code", model.to_python());
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
    use crate::model::Layer;

    fn input_layer() -> Layer {
        Layer {
            keras_class: "Input".to_string(),
            kwargs: vec![("shape".to_string(), "(28, 28, 1)".to_string())],
        }
    }

    #[test]
    fn empty_model_errors() {
        let mut node = ExportNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("model", Value::new(KerasModel::new()));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        assert!(node.execute(&inputs, &mut ctx).is_err());
    }

    #[test]
    fn missing_input_pin_errors() {
        let mut node = ExportNode::new();
        let mut ctx = NodeContext::new(NodeId(1), 0);
        assert!(node.execute(&InputSet::new(), &mut ctx).is_err());
    }

    #[test]
    fn non_input_first_layer_errors() {
        let mut node = ExportNode::new();
        let model = KerasModel::new().with_layer(
            1,
            Layer {
                keras_class: "Dense".to_string(),
                kwargs: vec![("units".to_string(), "10".to_string())],
            },
        );
        let mut inputs = InputSet::new();
        inputs.insert("model", Value::new(model));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        assert!(node.execute(&inputs, &mut ctx).is_err());
    }

    #[test]
    fn valid_model_emits_code() {
        let mut node = ExportNode::new();
        let model = KerasModel::new().with_layer(1, input_layer()).with_layer(
            2,
            Layer {
                keras_class: "Flatten".to_string(),
                kwargs: vec![],
            },
        );
        let mut inputs = InputSet::new();
        inputs.insert("model", Value::new(model));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let out = ctx.take_outputs();
        let code = out["code"].downcast_ref::<String>().unwrap();
        assert!(code.contains("x0 = layers.Input(shape=(28, 28, 1))"));
        assert!(code.contains("x1 = layers.Flatten()(x0)"));
        assert!(code.contains("model = keras.Model(inputs=x0, outputs=x1)"));
    }
}
