//! Flow plugin: runtime-flow primitives that bridge the two transmission modes
//! of the dataflow -- Events (one-shot, delivered on Trigger pins) and State
//! (last-value, sampled on Sample pins).
//!
//! `Hold` is the explicit Event -> State adapter: it latches the most recent
//! event value of any type and exposes it as a steady, samplable state. Its
//! pin kinds (Trigger in, Sample out) declare the conversion; once the executor
//! enforces the Trigger/Sample distinction, this node is where a transient
//! event stream becomes a persistent value other nodes can sample.

use zeughaus_core::*;

pub struct FlowPlugin;

impl DomainPlugin for FlowPlugin {
    fn name(&self) -> &str {
        "flow"
    }

    fn node_catalog(&self) -> Vec<NodeDefinition> {
        vec![catalog_entry("flow.hold", "Hold", "Flow", &HoldNode::new())]
    }

    fn create_node(&self, type_id: &str) -> Option<Box<dyn ExecutableNode>> {
        match type_id {
            "flow.hold" => Some(Box::new(HoldNode::new())),
            _ => None,
        }
    }
}

/// Sample & Hold: turns an Event of any type into State by latching the most
/// recent value. Between events it keeps emitting the last one, so downstream
/// nodes sample a steady value rather than a momentary pulse.
pub struct HoldNode {
    last: Option<Value>,
    pins: Vec<PinDefinition>,
}

impl Default for HoldNode {
    fn default() -> Self {
        Self::new()
    }
}

impl HoldNode {
    pub fn new() -> Self {
        Self {
            last: None,
            pins: vec![
                PinDefinition::input("in", Ty::Any, PinKind::Trigger),
                PinDefinition::output("out", Ty::Any),
            ],
        }
    }
}

impl ExecutableNode for HoldNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        // Latch a new event; otherwise keep the previously held value.
        if let Some(v) = inputs.get_value("in") {
            self.last = Some(v.clone());
        }
        // Emit the held state. Before the first event there is nothing to hold.
        if let Some(v) = &self.last {
            ctx.emit("out", v.clone());
            ctx.flush();
        }
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_all_catalog_nodes() {
        let plugin = FlowPlugin;
        for def in plugin.node_catalog() {
            assert!(plugin.create_node(&def.type_id).is_some(), "failed: {}", def.type_id);
        }
    }

    #[test]
    fn latches_then_holds_last_value() {
        let mut node = HoldNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("in", Value::new(5.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(ctx.take_outputs()["out"].downcast_ref::<f64>(), Some(&5.0));

        // No event this run: the held state persists (event consumed, state stays).
        let mut ctx2 = NodeContext::new(NodeId(1), 0);
        node.execute(&InputSet::new(), &mut ctx2).unwrap();
        assert_eq!(ctx2.take_outputs()["out"].downcast_ref::<f64>(), Some(&5.0));
    }

    #[test]
    fn holds_any_type() {
        let mut node = HoldNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("in", Value::new("hi".to_string()));
        let mut ctx = NodeContext::new(NodeId(2), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let out = ctx.take_outputs();
        assert_eq!(out["out"].downcast_ref::<String>().map(String::as_str), Some("hi"));
    }

    #[test]
    fn no_output_before_first_event() {
        let mut node = HoldNode::new();
        let mut ctx = NodeContext::new(NodeId(3), 0);
        node.execute(&InputSet::new(), &mut ctx).unwrap();
        assert!(ctx.take_outputs().is_empty());
    }
}
