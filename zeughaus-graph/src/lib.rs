//! Graph plugin: a subgraph is a node.
//!
//! Three node types and one idea. `graph.sub` is a container: it holds other
//! nodes and has no pins of its own. Its pins in the parent view are
//! synthesized from the boundary nodes inside it -- one input pin per
//! `graph.input`, one output pin per `graph.output`, named by that child's
//! `name` setting.
//!
//! The executor never learns about any of this. An edge in the store always
//! connects real nodes: a wire drawn onto a container's pin `x` is stored as an
//! edge to the `graph.input` child named `x`, and a boundary node is an
//! ordinary passthrough that emits what it was given. So the graph stays flat
//! where it is executed and is nested only where it is drawn, which is the one
//! place nesting is worth anything.

use zeughaus_core::*;

pub struct GraphPlugin;

impl DomainPlugin for GraphPlugin {
    fn name(&self) -> &str {
        "graph"
    }

    fn node_catalog(&self) -> Vec<NodeDefinition> {
        vec![
            catalog_entry("graph.sub", "Subgraph", "Graph", &SubgraphNode).container(),
            catalog_entry(
                "graph.input",
                "Input",
                "Graph",
                &BoundaryNode::new(Boundary::Input),
            ),
            catalog_entry(
                "graph.output",
                "Output",
                "Graph",
                &BoundaryNode::new(Boundary::Output),
            ),
        ]
    }

    fn create_node(&self, type_id: &str) -> Option<Box<dyn ExecutableNode>> {
        match type_id {
            "graph.sub" => Some(Box::new(SubgraphNode)),
            "graph.input" => Some(Box::new(BoundaryNode::new(Boundary::Input))),
            "graph.output" => Some(Box::new(BoundaryNode::new(Boundary::Output))),
            _ => None,
        }
    }
}

/// The container itself: nothing to run.
///
/// It is not a no-op by omission but by definition -- everything it stands for
/// is the nodes inside it, and those are executed on their own.
pub struct SubgraphNode;

impl ExecutableNode for SubgraphNode {
    fn execute(&mut self, _inputs: &InputSet, _ctx: &mut NodeContext) -> Result<()> {
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &[]
    }
}

/// Which side of a subgraph a boundary node stands on.
///
/// Only the default name differs; the behaviour is identical, because a
/// boundary is a passthrough in both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    Input,
    Output,
}

impl Boundary {
    /// The `name` this side defaults to, and therefore the pin name a fresh
    /// boundary node contributes to its container.
    pub fn default_name(self) -> &'static str {
        match self {
            Boundary::Input => "in",
            Boundary::Output => "out",
        }
    }
}

/// One pin of a subgraph, seen from the inside.
///
/// The pin it contributes to its container is named by the `name` setting, so
/// renaming it renames the container's pin. Inside, it is a plain passthrough:
/// whatever arrives on `in` leaves on `out`.
pub struct BoundaryNode {
    name: String,
    pins: Vec<PinDefinition>,
}

impl BoundaryNode {
    pub fn new(side: Boundary) -> Self {
        Self {
            name: side.default_name().to_string(),
            pins: vec![
                PinDefinition::input("in", Ty::Any, PinKind::Trigger),
                PinDefinition::output("out", Ty::Any),
            ],
        }
    }

    /// The name this boundary contributes to its container's pins.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl ExecutableNode for BoundaryNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        // Nothing on the input is not an error: a boundary whose outside is not
        // wired yet simply carries nothing, and emitting a substitute would
        // hand the subgraph a value nobody sent.
        if let Some(value) = inputs.get_value("in") {
            ctx.emit("out", value.clone());
            ctx.flush();
        }
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn settings(&self) -> Vec<SettingDef> {
        vec![SettingDef::new("name", self.name.clone())]
    }

    /// An empty name keeps the previous one: the container's pin has to be
    /// called something, and a half-cleared text field must not unname it.
    fn set_parameter(&mut self, name: &str, value: Value) -> Result<()> {
        if name != "name" {
            return Ok(());
        }
        if let Repr::Str(text) = value.repr() {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                self.name = trimmed.to_string();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_all_catalog_nodes() {
        let plugin = GraphPlugin;
        for def in plugin.node_catalog() {
            assert!(
                plugin.create_node(&def.type_id).is_some(),
                "failed: {}",
                def.type_id
            );
        }
    }

    /// The container has no pins of its own: the editor synthesizes them from
    /// the boundary nodes inside it, and a pin declared here would collide.
    #[test]
    fn a_subgraph_declares_no_pins_and_is_marked_a_container() {
        let plugin = GraphPlugin;
        let def = plugin
            .node_catalog()
            .into_iter()
            .find(|d| &*d.type_id == "graph.sub")
            .expect("graph.sub in the catalog");
        assert!(def.container);
        assert!(def.pins.is_empty());
        // The two boundary types are ordinary nodes.
        assert!(
            plugin
                .node_catalog()
                .iter()
                .filter(|d| &*d.type_id != "graph.sub")
                .all(|d| !d.container)
        );
    }

    /// A boundary is a passthrough: the value that crosses it is the value the
    /// outside sent, not a copy of anything the node decided.
    #[test]
    fn a_boundary_forwards_the_value_it_was_given() {
        let mut node = BoundaryNode::new(Boundary::Input);
        let mut inputs = InputSet::new();
        inputs.insert("in", Value::new(7.0_f64));
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&inputs, &mut ctx).expect("execute");
        let outputs = ctx.take_outputs();
        assert_eq!(
            outputs.get("out").and_then(Value::downcast_ref::<f64>),
            Some(&7.0)
        );
    }

    /// An unwired boundary emits nothing rather than a stand-in value.
    #[test]
    fn an_unwired_boundary_emits_nothing() {
        let mut node = BoundaryNode::new(Boundary::Output);
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&InputSet::new(), &mut ctx).expect("execute");
        assert!(ctx.take_outputs().is_empty());
    }

    /// The name is what the container's pin is called, so it is trimmed, and an
    /// empty one is refused rather than leaving a nameless pin.
    #[test]
    fn setting_the_name_trims_it_and_refuses_an_empty_one() {
        let mut node = BoundaryNode::new(Boundary::Output);
        assert_eq!(node.name(), "out");
        node.set_parameter("name", Value::new("  result ".to_string()))
            .expect("set");
        assert_eq!(node.name(), "result");
        node.set_parameter("name", Value::new("   ".to_string()))
            .expect("set");
        assert_eq!(node.name(), "result");
        // The name is also what the editor reads back as the setting default.
        assert_eq!(&*node.settings()[0].default, "result");
        // An unknown key is ignored, not an error.
        node.set_parameter("nope", Value::new("x".to_string()))
            .expect("set");
    }
}
