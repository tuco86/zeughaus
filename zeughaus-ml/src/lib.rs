//! ML plugin for Zeughaus. Turns Keras (TensorFlow) layers into nodes so a
//! neural network can be designed entirely in the node graph and exported as a
//! runnable Keras functional-API Python program.
//!
//! Nodes operate on a `KerasModel` value -- a DAG of layer steps. Each layer
//! node consumes a model and emits it extended by one step, so a network
//! architecture maps directly onto a node chain, the same pattern the LLM
//! plugin uses for Conversation. Merge nodes (Concatenate, Add, ...) take two
//! models and join their branches, so non-linear architectures work too.
//!
//! A typical graph: `Input -> Conv2D -> MaxPooling2D -> Flatten -> Dense ->
//! Compile -> Export Code`. Codegen mirrors the edges: each step becomes a
//! variable wired to its inputs (`x1 = layers.Dense(...)(x0)`, merges use
//! `([x1, x2])`), then `keras.Model(inputs, outputs)`, `model.compile(...)` and
//! `model.summary()`. The Export node shows the result live in the node.

pub mod model;
pub mod nodes;

pub use model::{CompileConfig, KerasModel, Layer};

use zeughaus_core::*;

use nodes::layer::{spec, LayerNode, LAYERS};
use nodes::merge::{merge_spec, MergeNode, MERGES};
use nodes::{CompileNode, ExportNode};

pub struct MlPlugin;

impl DomainPlugin for MlPlugin {
    fn name(&self) -> &str {
        "ml"
    }

    fn node_catalog(&self) -> Vec<NodeDefinition> {
        let mut catalog: Vec<NodeDefinition> = LAYERS
            .iter()
            .map(|s| catalog_entry(s.type_id, s.display_name, "ML", &LayerNode::new(s)))
            .collect();
        catalog.extend(
            MERGES
                .iter()
                .map(|s| catalog_entry(s.type_id, s.display_name, "ML Merge", &MergeNode::new(s))),
        );
        catalog.push(catalog_entry("ml.compile", "Compile", "ML", &CompileNode::new()));
        catalog.push(catalog_entry("ml.export", "Export Code", "ML", &ExportNode::new()));
        catalog
    }

    fn create_node(&self, type_id: &str) -> Option<Box<dyn ExecutableNode>> {
        match type_id {
            "ml.compile" => Some(Box::new(CompileNode::new())),
            "ml.export" => Some(Box::new(ExportNode::new())),
            other => {
                if let Some(s) = spec(other) {
                    Some(Box::new(LayerNode::new(s)))
                } else {
                    merge_spec(other).map(|s| Box::new(MergeNode::new(s)) as Box<dyn ExecutableNode>)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_all_catalog_nodes() {
        let plugin = MlPlugin;
        for def in plugin.node_catalog() {
            assert!(
                plugin.create_node(&def.type_id).is_some(),
                "failed to create: {}",
                def.type_id
            );
        }
    }

    #[test]
    fn unknown_type_returns_none() {
        assert!(MlPlugin.create_node("ml.nope").is_none());
    }

    /// End-to-end: build a realistic CNN by stacking layer nodes and render the
    /// full program through the Export node, asserting the exact expected code.
    #[test]
    fn cnn_round_trip_renders_expected_code() {
        let plugin = MlPlugin;

        // Build the stack: each node appends one layer to the model value.
        let chain = [
            ("ml.input", vec![("shape", "(28, 28, 1)")]),
            ("ml.conv2d", vec![("filters", "32"), ("kernel_size", "(3, 3)"), ("activation", "relu")]),
            ("ml.maxpool2d", vec![("pool_size", "(2, 2)")]),
            ("ml.flatten", vec![]),
            ("ml.dropout", vec![("rate", "0.5")]),
            ("ml.dense", vec![("units", "10"), ("activation", "softmax")]),
        ];

        // Each node gets a distinct id (as the executor assigns): the id is the
        // step's identity, so reusing one would collapse the chain.
        let mut model = KerasModel::new();
        for (i, (type_id, params)) in chain.into_iter().enumerate() {
            let mut node = plugin.create_node(type_id).unwrap();
            for (k, v) in params {
                node.set_parameter(k, Value::new(v.to_string())).unwrap();
            }
            let mut inputs = InputSet::new();
            inputs.insert("model", Value::new(model.clone()));
            let mut ctx = NodeContext::new(NodeId(i as u64 + 1), 0);
            node.execute(&inputs, &mut ctx).unwrap();
            model = ctx.take_outputs()["out"].downcast_ref::<KerasModel>().unwrap().clone();
        }

        // Compile, then export.
        let mut compile = plugin.create_node("ml.compile").unwrap();
        compile.set_parameter("optimizer", Value::new("adam".to_string())).unwrap();
        compile.set_parameter("loss", Value::new("categorical_crossentropy".to_string())).unwrap();
        compile.set_parameter("metrics", Value::new("accuracy".to_string())).unwrap();
        let mut inputs = InputSet::new();
        inputs.insert("model", Value::new(model));
        let mut ctx = NodeContext::new(NodeId(100), 0);
        compile.execute(&inputs, &mut ctx).unwrap();
        let model = ctx.take_outputs()["out"].downcast_ref::<KerasModel>().unwrap().clone();

        let mut export = plugin.create_node("ml.export").unwrap();
        let mut inputs = InputSet::new();
        inputs.insert("model", Value::new(model));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        export.execute(&inputs, &mut ctx).unwrap();
        let code = ctx.take_outputs()["code"].downcast_ref::<String>().unwrap().clone();

        let expected = concat!(
            "import keras\n",
            "from keras import layers\n",
            "\n",
            "x0 = layers.Input(shape=(28, 28, 1))\n",
            "x1 = layers.Conv2D(filters=32, kernel_size=(3, 3), activation='relu')(x0)\n",
            "x2 = layers.MaxPooling2D(pool_size=(2, 2))(x1)\n",
            "x3 = layers.Flatten()(x2)\n",
            "x4 = layers.Dropout(rate=0.5)(x3)\n",
            "x5 = layers.Dense(units=10, activation='softmax')(x4)\n",
            "\n",
            "model = keras.Model(inputs=x0, outputs=x5)\n",
            "model.compile(optimizer='adam', loss='categorical_crossentropy', metrics=['accuracy'])\n",
            "model.summary()\n",
        );

        assert_eq!(code, expected);
    }

    /// Runs a layer node with the given id and params on an incoming model.
    fn run(plugin: &MlPlugin, type_id: &str, id: u64, params: &[(&str, &str)], model: &KerasModel) -> KerasModel {
        let mut node = plugin.create_node(type_id).unwrap();
        for (k, v) in params {
            node.set_parameter(k, Value::new(v.to_string())).unwrap();
        }
        let mut inputs = InputSet::new();
        inputs.insert("model", Value::new(model.clone()));
        let mut ctx = NodeContext::new(NodeId(id), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        ctx.take_outputs()["out"].downcast_ref::<KerasModel>().unwrap().clone()
    }

    /// End-to-end with a branch: one Input fans out to two Conv2D branches that
    /// are joined by a Concatenate merge node, then a Dense head and Export.
    /// Verifies the shared Input is emitted once and the merge wires both tips.
    #[test]
    fn branching_model_renders_functional_dag() {
        let plugin = MlPlugin;

        let inp = run(&plugin, "ml.input", 1, &[("shape", "(32, 32, 3)")], &KerasModel::new());
        // The Input value is delivered to two separate branch nodes.
        let a = run(&plugin, "ml.conv2d", 2, &[("filters", "16"), ("kernel_size", "(3, 3)")], &inp);
        let b = run(&plugin, "ml.conv2d", 3, &[("filters", "16"), ("kernel_size", "(5, 5)")], &inp);

        // Merge node joins both branches.
        let mut merge = plugin.create_node("ml.concatenate").unwrap();
        let mut minputs = InputSet::new();
        minputs.insert("a", Value::new(a));
        minputs.insert("b", Value::new(b));
        let mut ctx = NodeContext::new(NodeId(4), 0);
        merge.execute(&minputs, &mut ctx).unwrap();
        let merged = ctx.take_outputs()["out"].downcast_ref::<KerasModel>().unwrap().clone();

        let head = run(&plugin, "ml.dense", 5, &[("units", "10"), ("activation", "softmax")], &merged);

        let mut export = plugin.create_node("ml.export").unwrap();
        let mut einputs = InputSet::new();
        einputs.insert("model", Value::new(head));
        let mut ctx = NodeContext::new(NodeId(6), 0);
        export.execute(&einputs, &mut ctx).unwrap();
        let code = ctx.take_outputs()["code"].downcast_ref::<String>().unwrap().clone();

        let expected = concat!(
            "import keras\n",
            "from keras import layers\n",
            "\n",
            "x0 = layers.Input(shape=(32, 32, 3))\n",
            "x1 = layers.Conv2D(filters=16, kernel_size=(3, 3), activation='relu')(x0)\n",
            "x2 = layers.Conv2D(filters=16, kernel_size=(5, 5), activation='relu')(x0)\n",
            "x3 = layers.Concatenate()([x1, x2])\n",
            "x4 = layers.Dense(units=10, activation='softmax')(x3)\n",
            "\n",
            "model = keras.Model(inputs=x0, outputs=x4)\n",
            "model.summary()\n",
        );
        assert_eq!(code, expected);
    }
}
