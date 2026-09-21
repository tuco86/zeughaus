use zeughaus_core::*;

use crate::model::{CompileConfig, KerasModel, keras_model_ty};

/// Attaches a training configuration (optimizer, loss, metrics) to the model.
/// Place it after the layer stack; the Export node renders the resulting
/// `model.compile(...)` call.
pub struct CompileNode {
    optimizer: String,
    loss: String,
    metrics: String,
    pins: Vec<PinDefinition>,
}

impl Default for CompileNode {
    fn default() -> Self {
        Self::new()
    }
}

impl CompileNode {
    pub fn new() -> Self {
        Self {
            optimizer: "adam".to_string(),
            loss: "categorical_crossentropy".to_string(),
            metrics: "accuracy".to_string(),
            pins: vec![
                PinDefinition::input("model", keras_model_ty(), PinKind::Sample),
                PinDefinition::output("out", keras_model_ty()),
            ],
        }
    }
}

impl ExecutableNode for CompileNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let model: KerasModel = inputs.get("model").unwrap_or_default();
        let out = model.with_compile(CompileConfig {
            optimizer: self.optimizer.clone(),
            loss: self.loss.clone(),
            metrics: self.metrics.clone(),
        });
        ctx.emit_typed("out", out);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn settings(&self) -> Vec<SettingDef> {
        vec![
            SettingDef::new("optimizer", "adam").placeholder("adam / sgd / rmsprop"),
            SettingDef::new("loss", "categorical_crossentropy").placeholder("loss function"),
            SettingDef::new("metrics", "accuracy").placeholder("accuracy, mae"),
        ]
    }

    fn set_parameter(&mut self, name: &str, value: Value) -> Result<()> {
        if let Some(s) = value.downcast_ref::<String>() {
            match name {
                "optimizer" => self.optimizer = s.clone(),
                "loss" => self.loss = s.clone(),
                "metrics" => self.metrics = s.clone(),
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
    fn execute_sets_compile_config() {
        let mut node = CompileNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("model", Value::new(KerasModel::new()));
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).unwrap();
        let out = ctx.take_outputs();
        let model = out["out"].downcast_ref::<KerasModel>().unwrap();
        let c = model.compile.as_ref().unwrap();
        assert_eq!(c.optimizer, "adam");
        assert_eq!(c.loss, "categorical_crossentropy");
    }
}
