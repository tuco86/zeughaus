use std::collections::{HashMap, HashSet};

use zeughaus_core::{EdgeId, ExecutableNode, InputSet, NodeContext, NodeId, Result, Value};

use crate::cache::EdgeCache;
use crate::graph::Graph;
use crate::topo::topological_sort;

pub struct GraphExecutor {
    pub graph: Graph,
    nodes: HashMap<NodeId, Box<dyn ExecutableNode>>,
    cache: EdgeCache,
    dirty: HashSet<NodeId>,
    trace_counter: u64,
}

impl GraphExecutor {
    pub fn new(graph: Graph) -> Self {
        Self {
            graph,
            nodes: HashMap::new(),
            cache: EdgeCache::new(),
            dirty: HashSet::new(),
            trace_counter: 0,
        }
    }

    pub fn register_node(&mut self, id: NodeId, exec: Box<dyn ExecutableNode>) {
        self.dirty.insert(id);
        self.nodes.insert(id, exec);
    }

    pub fn mark_dirty(&mut self, id: NodeId) {
        self.dirty.insert(id);
    }

    pub fn mark_dirty_downstream(&mut self, id: NodeId) {
        self.dirty.insert(id);
        for downstream in self.graph.downstream(id) {
            self.dirty.insert(downstream);
        }
    }

    pub fn execute_all(&mut self) -> Result<()> {
        for id in self.graph.node_ids().collect::<Vec<_>>() {
            self.dirty.insert(id);
        }
        self.execute_dirty()
    }

    pub fn execute_dirty(&mut self) -> Result<()> {
        let order = topological_sort(&self.graph)?;
        let dirty = std::mem::take(&mut self.dirty);

        for node_id in order {
            if !dirty.contains(&node_id) {
                continue;
            }

            let inputs = self.build_input_set(node_id);
            self.trace_counter += 1;
            let mut ctx = NodeContext::new(node_id, self.trace_counter);

            if let Some(node) = self.nodes.get_mut(&node_id) {
                node.execute(&inputs, &mut ctx)?;
            }

            let outputs = ctx.take_outputs();
            self.apply_outputs(node_id, outputs);
        }

        Ok(())
    }

    fn build_input_set(&self, node_id: NodeId) -> InputSet {
        let mut inputs = InputSet::new();
        for &edge_id in self.graph.incoming_edges(node_id) {
            if let Some(edge) = self.graph.edge(edge_id)
                && let Some(value) = self.cache.get(edge_id)
            {
                inputs.insert(edge.to_pin, value.clone());
            }
        }
        inputs
    }

    fn apply_outputs(&mut self, node_id: NodeId, outputs: HashMap<String, Value>) {
        for &edge_id in self.graph.outgoing_edges(node_id) {
            if let Some(edge) = self.graph.edge(edge_id)
                && let Some(value) = outputs.get(edge.from_pin)
            {
                self.cache.set(edge_id, value.clone());
            }
        }
    }

    pub fn edge_value(&self, edge_id: EdgeId) -> Option<&Value> {
        self.cache.get(edge_id)
    }

    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut Box<dyn ExecutableNode>> {
        self.nodes.get_mut(&id)
    }

    pub fn set_parameter(&mut self, id: NodeId, name: &str, value: Value) -> Result<()> {
        if let Some(node) = self.nodes.get_mut(&id) {
            node.set_parameter(name, value)?;
            self.mark_dirty_downstream(id);
        }
        Ok(())
    }

    /// Read cached output values for a node (from outgoing edge caches).
    pub fn node_output_values(&self, node_id: NodeId) -> HashMap<&'static str, &Value> {
        let mut result = HashMap::new();
        for &edge_id in self.graph.outgoing_edges(node_id) {
            if let Some(edge) = self.graph.edge(edge_id)
                && let Some(value) = self.cache.get(edge_id)
            {
                result.insert(edge.from_pin, value);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{GraphEdge, GraphNode};
    use zeughaus_core::*;

    struct ConstNode(f64);
    impl ExecutableNode for ConstNode {
        fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
            ctx.emit_typed("value", self.0);
            ctx.flush();
            Ok(())
        }
        fn pin_definitions(&self) -> &[PinDefinition] {
            &[]
        }
    }

    struct DoubleNode;
    impl ExecutableNode for DoubleNode {
        fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
            let v: f64 = inputs.get("in").unwrap_or(0.0);
            ctx.emit_typed("out", v * 2.0);
            ctx.flush();
            Ok(())
        }
        fn pin_definitions(&self) -> &[PinDefinition] {
            &[]
        }
    }

    fn make_node(id: NodeId) -> GraphNode {
        GraphNode {
            id,
            type_id: "test".to_string(),
            config: NodeConfig::default(),
            pin_defs: vec![],
            position: (0.0, 0.0),
        }
    }

    fn make_edge(
        from: NodeId,
        from_pin: &'static str,
        to: NodeId,
        to_pin: &'static str,
    ) -> GraphEdge {
        GraphEdge {
            id: EdgeId::next(),
            from_node: from,
            from_pin,
            to_node: to,
            to_pin,
            semantic: EdgeSemantic::default(),
        }
    }

    #[test]
    fn single_node_execution() {
        let mut graph = Graph::new();
        let a = NodeId::next();
        graph.add_node(make_node(a));

        let mut exec = GraphExecutor::new(graph);
        exec.register_node(a, Box::new(ConstNode(42.0)));
        exec.execute_all().unwrap();
        // No outgoing edges, so no cached values -- but execution should not panic
    }

    #[test]
    fn linear_chain_propagation() {
        let mut graph = Graph::new();
        let a = NodeId::next();
        let b = NodeId::next();
        graph.add_node(make_node(a));
        graph.add_node(make_node(b));
        let edge = make_edge(a, "value", b, "in");
        let edge_id = edge.id;
        graph.add_edge(edge);

        let mut exec = GraphExecutor::new(graph);
        exec.register_node(a, Box::new(ConstNode(5.0)));
        exec.register_node(b, Box::new(DoubleNode));
        exec.execute_all().unwrap();

        let val = exec.edge_value(edge_id).unwrap();
        assert_eq!(val.downcast_ref::<f64>(), Some(&5.0));
    }

    #[test]
    fn dirty_only_execution() {
        let mut graph = Graph::new();
        let a = NodeId::next();
        let b = NodeId::next();
        graph.add_node(make_node(a));
        graph.add_node(make_node(b));
        let edge = make_edge(a, "value", b, "in");
        graph.add_edge(edge);

        let mut exec = GraphExecutor::new(graph);
        exec.register_node(a, Box::new(ConstNode(5.0)));
        exec.register_node(b, Box::new(DoubleNode));

        // First full execution
        exec.execute_all().unwrap();

        // Only mark a as dirty (and its downstream)
        exec.mark_dirty_downstream(a);
        exec.execute_dirty().unwrap();
        // Should not panic -- b is also re-executed because it's downstream
    }
}
