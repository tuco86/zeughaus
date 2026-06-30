//! ML plugin for Zeughaus. Turns Keras (TensorFlow) layers into nodes so a
//! neural network can be designed entirely in the node graph and exported as a
//! runnable Keras functional-API Python program.
//!
//! Nodes operate on a `KerasModel` value: each layer node consumes a model and
//! emits it extended by one layer, so a network architecture maps directly onto
//! a node chain -- the same pattern the LLM plugin uses for Conversation.
//!
//! A typical graph: `Input -> Conv2D -> MaxPooling2D -> Flatten -> Dense ->
//! Compile -> Export Code`. Codegen mirrors the edges: each layer becomes a
//! variable wired to its predecessor (`x1 = layers.Dense(...)(x0)`), then
//! `keras.Model(inputs, outputs)`, `model.compile(...)` and `model.summary()`.
//! The Export node shows the result live in the node.

pub mod model;
pub mod nodes;

pub use model::{CompileConfig, KerasModel, Layer};

use zeughaus_core::*;

use nodes::layer::{spec, LayerNode, LAYERS};
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
        catalog.push(catalog_entry("ml.compile", "Compile", "ML", &CompileNode::new()));
        catalog.push(catalog_entry("ml.export", "Export Code", "ML", &ExportNode::new()));
        catalog
    }

    fn create_node(&self, type_id: &str) -> Option<Box<dyn ExecutableNode>> {
        match type_id {
            "ml.compile" => Some(Box::new(CompileNode::new())),
            "ml.export" => Some(Box::new(ExportNode::new())),
            other => spec(other).map(|s| Box::new(LayerNode::new(s)) as Box<dyn ExecutableNode>),
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
                plugin.create_node(def.type_id).is_some(),
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

        let mut model = KerasModel::new();
        for (type_id, params) in chain {
            let mut node = plugin.create_node(type_id).unwrap();
            for (k, v) in params {
                node.set_parameter(k, Value::new(v.to_string())).unwrap();
            }
            let mut inputs = InputSet::new();
            inputs.insert("model", Value::new(model.clone()));
            let mut ctx = NodeContext::new(NodeId(1), 0);
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
        let mut ctx = NodeContext::new(NodeId(1), 0);
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
}
