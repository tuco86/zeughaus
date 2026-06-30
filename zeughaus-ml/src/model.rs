//! The KerasModel value type that flows through ML nodes, plus the codegen
//! that renders it to runnable Keras (TensorFlow) Python.
//!
//! A KerasModel is an ordered stack of layer specifications with an optional
//! compile configuration. Layer nodes consume a model and emit the same model
//! with one more layer appended, so a network architecture maps directly onto
//! a node chain -- exactly like the LLM Conversation pattern.

use serde::{Deserialize, Serialize};

/// One rendered Keras layer: the `keras.layers` class name plus its already
/// formatted keyword arguments (e.g. `("units", "64")`, `("activation", "'relu'")`).
/// Quoting is resolved at build time from the parameter type so codegen stays
/// a trivial join.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layer {
    pub keras_class: String,
    pub kwargs: Vec<(String, String)>,
}

impl Layer {
    /// Renders `layers.Dense(units=64, activation='relu')`.
    pub fn render(&self) -> String {
        let args = self
            .kwargs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("layers.{}({})", self.keras_class, args)
    }
}

/// How the model is trained. Empty fields fall back to Keras defaults (omitted
/// from the generated `compile` call).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompileConfig {
    pub optimizer: String,
    pub loss: String,
    pub metrics: String,
}

impl CompileConfig {
    pub fn is_set(&self) -> bool {
        !self.optimizer.trim().is_empty()
            || !self.loss.trim().is_empty()
            || !self.metrics.trim().is_empty()
    }
}

/// An ordered layer stack with optional compile config. Cloned freely as it
/// flows through edges.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KerasModel {
    pub layers: Vec<Layer>,
    pub compile: Option<CompileConfig>,
}

impl KerasModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a new model with `layer` appended (immutable-style chaining).
    pub fn with_layer(&self, layer: Layer) -> Self {
        let mut next = self.clone();
        next.layers.push(layer);
        next
    }

    /// Returns a new model carrying the given compile configuration.
    pub fn with_compile(&self, compile: CompileConfig) -> Self {
        let mut next = self.clone();
        next.compile = Some(compile);
        next
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Renders the full Keras Python program using the functional API: imports,
    /// one variable per layer wired to its predecessor via the call syntax
    /// `xN = layers.Foo(...)(x{N-1})`, the `keras.Model(inputs, outputs)`
    /// construction, an optional compile call, and `model.summary()`.
    ///
    /// The first layer (the graph root, an `Input`) has no incoming edge, so it
    /// is emitted without a call suffix; every following layer is called on the
    /// variable of the layer feeding it, mirroring the graph edges.
    pub fn to_python(&self) -> String {
        let mut out = String::from("import keras\nfrom keras import layers\n\n");
        if self.layers.is_empty() {
            return out;
        }

        let var = |i: usize| format!("x{i}");
        for (i, layer) in self.layers.iter().enumerate() {
            if i == 0 {
                // Root layer: no incoming edge, no functional call.
                out.push_str(&format!("{} = {}\n", var(i), layer.render()));
            } else {
                // Edge from the previous layer becomes the (input) call.
                out.push_str(&format!("{} = {}({})\n", var(i), layer.render(), var(i - 1)));
            }
        }

        let inputs = var(0);
        let outputs = var(self.layers.len() - 1);
        out.push_str(&format!("\nmodel = keras.Model(inputs={inputs}, outputs={outputs})\n"));

        if let Some(c) = &self.compile {
            let mut args: Vec<String> = Vec::new();
            if !c.optimizer.trim().is_empty() {
                args.push(format!("optimizer='{}'", c.optimizer.trim()));
            }
            if !c.loss.trim().is_empty() {
                args.push(format!("loss='{}'", c.loss.trim()));
            }
            if !c.metrics.trim().is_empty() {
                args.push(format!("metrics=[{}]", quote_metrics(&c.metrics)));
            }
            out.push_str(&format!("model.compile({})\n", args.join(", ")));
        }

        out.push_str("model.summary()\n");
        out
    }
}

/// Turns a comma-separated metrics setting (`accuracy, mae`) into a quoted
/// Python list body (`'accuracy', 'mae'`).
fn quote_metrics(raw: &str) -> String {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl std::fmt::Display for KerasModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Compact one-line summary for the node value display row.
        match self.layers.last() {
            Some(l) => write!(f, "[{} layers] -> {}", self.layers.len(), l.keras_class),
            None => write!(f, "[empty model]"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dense(units: &str) -> Layer {
        Layer {
            keras_class: "Dense".to_string(),
            kwargs: vec![("units".to_string(), units.to_string())],
        }
    }

    #[test]
    fn with_layer_appends_without_mutating() {
        let base = KerasModel::new().with_layer(dense("32"));
        let extended = base.with_layer(dense("10"));
        assert_eq!(base.layers.len(), 1);
        assert_eq!(extended.layers.len(), 2);
    }

    #[test]
    fn render_layer_joins_kwargs() {
        let layer = Layer {
            keras_class: "Dense".to_string(),
            kwargs: vec![
                ("units".to_string(), "64".to_string()),
                ("activation".to_string(), "'relu'".to_string()),
            ],
        };
        assert_eq!(layer.render(), "layers.Dense(units=64, activation='relu')");
    }

    fn input(shape: &str) -> Layer {
        Layer {
            keras_class: "Input".to_string(),
            kwargs: vec![("shape".to_string(), shape.to_string())],
        }
    }

    #[test]
    fn codegen_includes_imports_and_summary() {
        let py = KerasModel::new().with_layer(input("(4,)")).to_python();
        assert!(py.contains("import keras"));
        assert!(py.contains("model = keras.Model(inputs=x0, outputs=x0)"));
        assert!(py.contains("x0 = layers.Input(shape=(4,))"));
        assert!(py.contains("model.summary()"));
        assert!(!py.contains("model.compile"));
    }

    #[test]
    fn codegen_wires_layers_with_functional_calls() {
        // Root layer has no call suffix; each edge becomes a (prev) call.
        let py = KerasModel::new()
            .with_layer(input("(4,)"))
            .with_layer(dense("10"))
            .to_python();
        assert!(py.contains("x0 = layers.Input(shape=(4,))\n"));
        assert!(py.contains("x1 = layers.Dense(units=10)(x0)\n"));
        assert!(py.contains("model = keras.Model(inputs=x0, outputs=x1)"));
        assert!(!py.contains("Sequential"));
    }

    #[test]
    fn codegen_emits_compile_when_set() {
        let model = KerasModel::new().with_layer(dense("10")).with_compile(CompileConfig {
            optimizer: "adam".to_string(),
            loss: "mse".to_string(),
            metrics: "accuracy, mae".to_string(),
        });
        let py = model.to_python();
        assert!(py.contains("model.compile(optimizer='adam', loss='mse', metrics=['accuracy', 'mae'])"));
    }

    #[test]
    fn compile_config_is_set_detects_empty() {
        assert!(!CompileConfig::default().is_set());
        assert!(CompileConfig { optimizer: "adam".into(), ..Default::default() }.is_set());
    }
}
