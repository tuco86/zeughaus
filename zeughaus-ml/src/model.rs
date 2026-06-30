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

/// One node of the model DAG: a layer plus the ids of the steps feeding it.
/// `inputs` is empty for a root (an `Input` layer), holds one id for a normal
/// layer, and two or more for a merge layer (Concatenate, Add, ...).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    /// Stable identity: the editor node that produced this layer. The same node
    /// feeding two branches yields one step, so merges dedup by id.
    pub id: u64,
    pub layer: Layer,
    pub inputs: Vec<u64>,
}

/// A model as a directed acyclic graph of layer steps with an optional compile
/// config. Cloned freely as it flows through edges. A linear chain is just a
/// DAG where every step has one input; branches and merges add steps with zero
/// or multiple inputs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KerasModel {
    pub steps: Vec<Step>,
    /// The current branch tip: the step a downstream layer connects to. `None`
    /// for an empty model.
    pub output: Option<u64>,
    pub compile: Option<CompileConfig>,
}

impl KerasModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a new model with `layer` appended after the current tip. `id` is
    /// the producing node's identity. A root layer (empty incoming model) gets
    /// no inputs; otherwise it consumes the current `output` tip.
    pub fn with_layer(&self, id: u64, layer: Layer) -> Self {
        let mut next = self.clone();
        let inputs: Vec<u64> = next.output.into_iter().collect();
        next.steps.push(Step { id, layer, inputs });
        next.output = Some(id);
        next
    }

    /// Builds a merge step (`layer`, e.g. Concatenate) fed by the tips of every
    /// branch. Branch step sets are unioned and deduped by id so layers shared
    /// across branches (e.g. a common Input) appear once. `id` is the merge
    /// node's identity.
    pub fn join(id: u64, layer: Layer, branches: &[&KerasModel]) -> Self {
        let mut steps: Vec<Step> = Vec::new();
        let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut compile = None;
        for b in branches {
            for s in &b.steps {
                if seen.insert(s.id) {
                    steps.push(s.clone());
                }
            }
            if compile.is_none() {
                compile = b.compile.clone();
            }
        }
        let inputs: Vec<u64> = branches.iter().filter_map(|b| b.output).collect();
        steps.push(Step { id, layer, inputs });
        Self { steps, output: Some(id), compile }
    }

    /// Returns a new model carrying the given compile configuration.
    pub fn with_compile(&self, compile: CompileConfig) -> Self {
        let mut next = self.clone();
        next.compile = Some(compile);
        next
    }

    pub fn is_empty(&self) -> bool {
        self.output.is_none()
    }

    /// Steps in a valid dependency order (every step after all of its inputs),
    /// via Kahn's algorithm. Deterministic: ties follow insertion order.
    fn ordered(&self) -> Vec<&Step> {
        use std::collections::HashMap;
        let mut indeg: HashMap<u64, usize> = self.steps.iter().map(|s| (s.id, 0)).collect();
        let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
        for s in &self.steps {
            for &inp in &s.inputs {
                // Count only edges to steps that actually exist, so a dangling
                // reference never deadlocks the sort.
                if indeg.contains_key(&inp) {
                    *indeg.get_mut(&s.id).unwrap() += 1;
                    children.entry(inp).or_default().push(s.id);
                }
            }
        }
        let by_id: HashMap<u64, &Step> = self.steps.iter().map(|s| (s.id, s)).collect();
        let mut queue: Vec<u64> = self.steps.iter().filter(|s| indeg[&s.id] == 0).map(|s| s.id).collect();
        let mut order: Vec<&Step> = Vec::new();
        let mut head = 0;
        while head < queue.len() {
            let id = queue[head];
            head += 1;
            order.push(by_id[&id]);
            if let Some(cs) = children.get(&id) {
                for &c in cs {
                    let d = indeg.get_mut(&c).unwrap();
                    *d -= 1;
                    if *d == 0 {
                        queue.push(c);
                    }
                }
            }
        }
        order
    }

    /// Renders the full Keras Python program using the functional API. Codegen
    /// mirrors the DAG: each step becomes a variable, and the call syntax
    /// reflects its inputs -- none for a root (`x0 = layers.Input(...)`), one
    /// for a normal layer (`x1 = layers.Dense(...)(x0)`), and a list for a
    /// merge (`x3 = layers.Concatenate()([x1, x2])`). The program closes with
    /// `keras.Model(inputs, outputs)`, an optional compile call, and summary.
    pub fn to_python(&self) -> String {
        use std::collections::HashMap;
        let mut out = String::from("import keras\nfrom keras import layers\n\n");
        if self.steps.is_empty() {
            return out;
        }

        let order = self.ordered();
        let var: HashMap<u64, String> =
            order.iter().enumerate().map(|(i, s)| (s.id, format!("x{i}"))).collect();

        for step in &order {
            let call = match step.inputs.as_slice() {
                [] => String::new(),
                [single] => format!("({})", var[single]),
                many => {
                    let refs = many.iter().map(|i| var[i].clone()).collect::<Vec<_>>().join(", ");
                    format!("([{refs}])")
                }
            };
            out.push_str(&format!("{} = {}{}\n", var[&step.id], step.layer.render(), call));
        }

        // Roots (no inputs) are the model inputs; a single root is passed bare,
        // multiple as a list.
        let roots: Vec<String> =
            order.iter().filter(|s| s.inputs.is_empty()).map(|s| var[&s.id].clone()).collect();
        let inputs = match roots.as_slice() {
            [single] => single.clone(),
            _ => format!("[{}]", roots.join(", ")),
        };
        let outputs = self.output.map(|o| var[&o].clone()).unwrap_or_default();
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

    /// Root step ids (no inputs). All roots must be `Input` layers for the
    /// functional model to build; the Export node validates this.
    pub fn roots(&self) -> Vec<&Step> {
        self.steps.iter().filter(|s| s.inputs.is_empty()).collect()
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
        let tip = self.output.and_then(|o| self.steps.iter().find(|s| s.id == o));
        match tip {
            Some(s) => write!(f, "[{} layers] -> {}", self.steps.len(), s.layer.keras_class),
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
        let base = KerasModel::new().with_layer(1, dense("32"));
        let extended = base.with_layer(2, dense("10"));
        assert_eq!(base.steps.len(), 1);
        assert_eq!(extended.steps.len(), 2);
        // The new step is wired to the previous tip.
        assert_eq!(extended.steps[1].inputs, vec![1]);
        assert_eq!(extended.output, Some(2));
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

    fn concat() -> Layer {
        Layer { keras_class: "Concatenate".to_string(), kwargs: vec![] }
    }

    #[test]
    fn codegen_includes_imports_and_summary() {
        let py = KerasModel::new().with_layer(1, input("(4,)")).to_python();
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
            .with_layer(1, input("(4,)"))
            .with_layer(2, dense("10"))
            .to_python();
        assert!(py.contains("x0 = layers.Input(shape=(4,))\n"));
        assert!(py.contains("x1 = layers.Dense(units=10)(x0)\n"));
        assert!(py.contains("model = keras.Model(inputs=x0, outputs=x1)"));
        assert!(!py.contains("Sequential"));
    }

    #[test]
    fn codegen_merges_two_branches_with_list_call() {
        // One Input fans out to two Dense layers, joined by Concatenate. The
        // shared Input must appear once, and the merge uses list-call syntax.
        let inp = KerasModel::new().with_layer(1, input("(4,)"));
        let a = inp.with_layer(2, dense("8"));
        let b = inp.with_layer(3, dense("16"));
        let merged = KerasModel::join(4, concat(), &[&a, &b]);
        let py = merged.with_layer(5, dense("10")).to_python();

        assert_eq!(py.matches("layers.Input").count(), 1, "shared Input emitted once");
        assert!(py.contains("layers.Concatenate()(["), "merge uses list call");
        // The concat references both branch tips.
        assert!(py.contains("(x0)\n"), "branch dense wired to shared input");
        assert!(py.contains("model = keras.Model(inputs=x0, outputs=x4)"));
        assert!(!py.contains("Sequential"));
    }

    #[test]
    fn codegen_multi_input_model_lists_all_roots() {
        let a = KerasModel::new().with_layer(1, input("(4,)"));
        let b = KerasModel::new().with_layer(2, input("(8,)"));
        let merged = KerasModel::join(3, concat(), &[&a, &b]);
        let py = merged.to_python();
        assert!(py.contains("model = keras.Model(inputs=[x0, x1], outputs=x2)"));
    }

    #[test]
    fn codegen_emits_compile_when_set() {
        let model = KerasModel::new().with_layer(1, dense("10")).with_compile(CompileConfig {
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
