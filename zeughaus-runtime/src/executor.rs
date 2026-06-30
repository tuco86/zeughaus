use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use zeughaus_core::{
    AsyncWork, EdgeId, ExecutableNode, InputSet, NodeContext, NodeId, PinDefinition, Result,
    TypeConverters, Value,
};

/// Work a node deferred during execution, tagged with the node that owns it.
/// The host runs each off-thread and returns the outputs via
/// [`GraphExecutor::deliver_async_result`].
pub type DeferredWork = Vec<(NodeId, Box<dyn AsyncWork>)>;

use crate::cache::EdgeCache;
use crate::graph::Graph;
use crate::topo::topological_sort;

pub struct GraphExecutor {
    pub graph: Graph,
    nodes: HashMap<NodeId, Box<dyn ExecutableNode>>,
    cache: EdgeCache,
    dirty: HashSet<NodeId>,
    /// Nodes awaiting an async result. They are skipped during execution so a
    /// re-run does not spawn a duplicate request, and their downstream nodes
    /// are held back until the result arrives.
    pending: HashSet<NodeId>,
    /// Last produced output values per node (pin name -> value), independent of
    /// edges. Lets a newly connected edge be seeded from an already-computed
    /// source without re-executing it -- critical for nodes with side effects
    /// (e.g. an LLM chat node must not re-fire just because a wire was drawn).
    last_outputs: HashMap<NodeId, HashMap<String, Value>>,
    /// Nodes whose last execution failed (sync error or async failure). Cleared
    /// when the node next runs cleanly. Drives error styling in the editor.
    error_nodes: HashSet<NodeId>,
    trace_counter: u64,
    /// Coerces values that cross an edge whose endpoints declare different
    /// types (e.g. a u8 output into an f64 input). Shared with the editor's
    /// connection validation so "what may connect" and "what is coerced" agree.
    converters: Arc<TypeConverters>,
}

impl GraphExecutor {
    pub fn new(graph: Graph) -> Self {
        Self {
            graph,
            nodes: HashMap::new(),
            cache: EdgeCache::new(),
            dirty: HashSet::new(),
            pending: HashSet::new(),
            last_outputs: HashMap::new(),
            error_nodes: HashSet::new(),
            trace_counter: 0,
            converters: Arc::new(TypeConverters::new()),
        }
    }

    /// Installs the type-converter registry (built from the plugins at startup).
    pub fn set_converters(&mut self, converters: Arc<TypeConverters>) {
        self.converters = converters;
    }

    /// Number of nodes currently awaiting an async result.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Whether a node is awaiting an async result (working).
    pub fn is_pending(&self, id: NodeId) -> bool {
        self.pending.contains(&id)
    }

    /// Whether a node's last execution failed.
    pub fn is_error(&self, id: NodeId) -> bool {
        self.error_nodes.contains(&id)
    }

    /// Flags a node as failed (used by the host when async work errors).
    pub fn mark_error(&mut self, id: NodeId) {
        self.pending.remove(&id);
        self.error_nodes.insert(id);
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

    pub fn execute_all(&mut self) -> Result<DeferredWork> {
        for id in self.graph.node_ids().collect::<Vec<_>>() {
            self.dirty.insert(id);
        }
        self.execute_dirty()
    }

    /// Executes all dirty nodes in topological order. Nodes that defer async
    /// work (or are already awaiting a result) are skipped, and their
    /// downstream nodes are held back until the result is delivered. Returns
    /// the deferred work for the host to run off-thread.
    pub fn execute_dirty(&mut self) -> Result<DeferredWork> {
        let order = topological_sort(&self.graph)?;
        let dirty = std::mem::take(&mut self.dirty);
        let mut deferred: DeferredWork = Vec::new();
        // Nodes whose outputs are not (yet) available this pass: deferred this
        // pass, already pending, or transitively downstream of either.
        let mut blocked: HashSet<NodeId> = HashSet::new();

        for node_id in order {
            if !dirty.contains(&node_id) {
                continue;
            }

            // Hold back nodes that feed off a blocked upstream; keep them dirty
            // so they run once the upstream async result arrives.
            if self.has_blocked_input(node_id, &blocked) {
                blocked.insert(node_id);
                self.dirty.insert(node_id);
                continue;
            }

            // Already awaiting a result from an earlier pass: don't re-run
            // (would spawn a duplicate request), but block its downstream.
            if self.pending.contains(&node_id) {
                blocked.insert(node_id);
                continue;
            }

            let inputs = self.build_input_set(node_id);
            self.trace_counter += 1;
            let mut ctx = NodeContext::new(node_id, self.trace_counter);

            if let Some(node) = self.nodes.get_mut(&node_id)
                && let Err(e) = node.execute(&inputs, &mut ctx)
            {
                // Tag the failing node so the host can flag it visually,
                // then propagate the error.
                self.error_nodes.insert(node_id);
                return Err(e);
            }

            if let Some(work) = ctx.take_deferred() {
                self.pending.insert(node_id);
                self.error_nodes.remove(&node_id); // now retrying
                blocked.insert(node_id);
                deferred.push((node_id, work));
                continue;
            }

            let outputs = ctx.take_outputs();
            self.error_nodes.remove(&node_id); // executed cleanly
            self.last_outputs.insert(node_id, outputs.clone());
            self.apply_outputs(node_id, outputs);
        }

        Ok(deferred)
    }

    fn build_input_set(&self, node_id: NodeId) -> InputSet {
        let mut inputs = InputSet::new();

        for &edge_id in self.graph.incoming_edges(node_id) {
            if let Some(edge) = self.graph.edge(edge_id)
                && let Some(value) = self.cache.get(edge_id)
            {
                let to_type = self.pin_type(node_id, edge.to_pin);
                let from_type = self.pin_type(edge.from_node, edge.from_pin);
                let value = self.coerce(from_type, to_type, value, node_id, edge.to_pin);
                inputs.insert(edge.to_pin, value);
            }
        }
        inputs
    }

    /// Declared type name of a node's pin, if the node and pin are known.
    fn pin_type(&self, node: NodeId, pin: &str) -> Option<&'static str> {
        self.graph
            .node(node)?
            .pin_defs
            .iter()
            .find(|p| p.name == pin)
            .map(|p| p.type_name)
    }

    /// Coerces a value crossing an edge to the target pin's declared type. If
    /// the types match (or either is unknown/`any`), the value passes through.
    /// Otherwise a registered converter is applied; a missing converter for a
    /// genuine mismatch is logged and the value passed through unchanged.
    fn coerce(
        &self,
        from_type: Option<&str>,
        to_type: Option<&str>,
        value: &Value,
        node_id: NodeId,
        to_pin: &str,
    ) -> Value {
        let (Some(from), Some(to)) = (from_type, to_type) else {
            return value.clone();
        };
        if from == to || to == "any" || from == "any" {
            return value.clone();
        }
        if let Some(converted) = self.converters.convert(from, to, value) {
            return converted;
        }
        eprintln!(
            "Type mismatch on {node_id:?} pin '{to_pin}': expected {to}, got {from} (no converter)"
        );
        value.clone()
    }

    /// True if any input edge of `node_id` originates from a blocked node.
    fn has_blocked_input(&self, node_id: NodeId, blocked: &HashSet<NodeId>) -> bool {
        self.graph.incoming_edges(node_id).iter().any(|&edge_id| {
            self.graph
                .edge(edge_id)
                .is_some_and(|e| blocked.contains(&e.from_node))
        })
    }

    /// Delivers the outputs of a previously deferred node, then resumes
    /// execution of its (now unblocked) downstream nodes. Returns any further
    /// deferred work produced downstream (e.g. a chain of chat nodes).
    pub fn deliver_async_result(
        &mut self,
        node_id: NodeId,
        outputs: HashMap<String, Value>,
    ) -> Result<DeferredWork> {
        self.pending.remove(&node_id);
        self.error_nodes.remove(&node_id);
        self.last_outputs.insert(node_id, outputs.clone());
        self.apply_outputs(node_id, outputs);
        // Re-run only the downstream; the node itself is already done.
        for downstream in self.graph.downstream(node_id) {
            self.dirty.insert(downstream);
        }
        self.execute_dirty()
    }

    /// Clears a node's pending state without delivering outputs (e.g. after the
    /// async work failed). Downstream nodes stay unexecuted until re-triggered.
    pub fn clear_pending(&mut self, node_id: NodeId) {
        self.pending.remove(&node_id);
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

    /// Notifies the executor that an edge was just added. Seeds the edge from
    /// the source node's last known output (so the target sees the value
    /// immediately) and marks only the target's subtree dirty. The source is
    /// NOT re-executed -- avoids re-firing nodes with side effects. Only when
    /// the source has no cached output do we fall back to running it.
    pub fn on_edge_added(&mut self, edge_id: EdgeId) {
        let Some(edge) = self.graph.edge(edge_id) else {
            return;
        };
        let from_node = edge.from_node;
        let from_pin = edge.from_pin;
        let to_node = edge.to_node;

        if let Some(value) = self
            .last_outputs
            .get(&from_node)
            .and_then(|outs| outs.get(from_pin))
            .cloned()
        {
            self.cache.set(edge_id, value);
            self.mark_dirty_downstream(to_node);
        } else {
            // Source never produced this output yet; run it to populate the edge.
            self.mark_dirty_downstream(from_node);
        }
    }

    /// Remove an edge and clear its cached value.
    /// Marks the downstream node dirty so it re-executes without the stale input.
    pub fn disconnect_edge(&mut self, edge_id: EdgeId) {
        if let Some(edge) = self.graph.edge(edge_id) {
            let to_node = edge.to_node;
            self.cache.remove(edge_id);
            self.graph.remove_edge(edge_id);
            self.mark_dirty_downstream(to_node);
        }
    }

    /// Remove a node, all its edges, and clean up all associated cache entries.
    pub fn remove_node(&mut self, id: NodeId) {
        // Collect edge IDs to remove (incoming + outgoing)
        let edge_ids: Vec<EdgeId> = self
            .graph
            .incoming_edges(id)
            .iter()
            .chain(self.graph.outgoing_edges(id).iter())
            .copied()
            .collect();

        for eid in edge_ids {
            self.cache.remove(eid);
        }

        self.graph.remove_node(id);
        self.nodes.remove(&id);
        self.dirty.remove(&id);
        self.pending.remove(&id);
        self.last_outputs.remove(&id);
        self.error_nodes.remove(&id);
    }

    pub fn edge_value(&self, edge_id: EdgeId) -> Option<&Value> {
        self.cache.get(edge_id)
    }

    /// Recomputes a variadic node's pins from its connected input pin names.
    /// On a change, updates the graph node's pin_defs and returns the new pin
    /// set so the editor can re-sync its own snapshot. Returns None if unchanged.
    pub fn sync_node_arity(&mut self, id: NodeId, connected: &[&str]) -> Option<Vec<PinDefinition>> {
        let changed = self.nodes.get_mut(&id)?.sync_arity(connected);
        if !changed {
            return None;
        }
        let pins = self.nodes.get(&id)?.pin_definitions().to_vec();
        if let Some(n) = self.graph.node_mut(id) {
            n.pin_defs = pins.clone();
        }
        Some(pins)
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

    /// A node that defers its output to async work instead of emitting now.
    struct DeferNode(f64);
    impl ExecutableNode for DeferNode {
        fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
            ctx.defer(Box::new(DeferWork(self.0)));
            Ok(())
        }
        fn pin_definitions(&self) -> &[PinDefinition] {
            &[]
        }
    }
    struct DeferWork(f64);
    impl AsyncWork for DeferWork {
        fn run(self: Box<Self>) -> Result<HashMap<String, Value>> {
            let mut out = HashMap::new();
            out.insert("value".to_string(), Value::new(self.0));
            Ok(out)
        }
    }

    #[test]
    fn deferred_node_blocks_downstream_until_delivered() {
        let mut graph = Graph::new();
        let a = NodeId::next();
        let b = NodeId::next();
        graph.add_node(make_node(a));
        graph.add_node(make_node(b));
        let edge = make_edge(a, "value", b, "in");
        let edge_id = edge.id;
        graph.add_edge(edge);

        let mut exec = GraphExecutor::new(graph);
        exec.register_node(a, Box::new(DeferNode(7.0)));
        exec.register_node(b, Box::new(DoubleNode));

        // First pass: node a defers, b is held back, no output cached yet.
        let deferred = exec.execute_all().unwrap();
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].0, a);
        assert_eq!(exec.pending_count(), 1);
        assert!(exec.edge_value(edge_id).is_none());

        // Run the deferred work off-thread (here inline) and deliver it.
        let outputs = deferred.into_iter().next().unwrap().1.run().unwrap();
        let more = exec.deliver_async_result(a, outputs).unwrap();

        // a is no longer pending; b ran with a's value (7 * 2 = 14).
        assert!(more.is_empty());
        assert_eq!(exec.pending_count(), 0);
        let val = exec.edge_value(edge_id).unwrap();
        assert_eq!(val.downcast_ref::<f64>(), Some(&7.0));
    }

    #[test]
    fn on_edge_added_seeds_without_rerunning_source() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;

        struct Counting(f64, Arc<AtomicU32>);
        impl ExecutableNode for Counting {
            fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
                self.1.fetch_add(1, Ordering::SeqCst);
                ctx.emit_typed("value", self.0);
                ctx.flush();
                Ok(())
            }
            fn pin_definitions(&self) -> &[PinDefinition] {
                &[]
            }
        }

        let mut graph = Graph::new();
        let a = NodeId::next();
        let b = NodeId::next();
        graph.add_node(make_node(a));
        graph.add_node(make_node(b));

        let mut exec = GraphExecutor::new(graph);
        let runs = Arc::new(AtomicU32::new(0));
        exec.register_node(a, Box::new(Counting(9.0, runs.clone())));
        exec.register_node(b, Box::new(DoubleNode));
        exec.execute_all().unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        // Connect a -> b AFTER a already produced its output.
        let edge = make_edge(a, "value", b, "in");
        let edge_id = edge.id;
        exec.graph.add_edge(edge);
        exec.on_edge_added(edge_id);
        exec.execute_dirty().unwrap();

        // Source not re-executed; the new edge is seeded from its cached output.
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        let val = exec.edge_value(edge_id).unwrap();
        assert_eq!(val.downcast_ref::<f64>(), Some(&9.0));
    }

    #[test]
    fn error_state_set_then_cleared() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        // Fails while `fail` is true, succeeds otherwise.
        struct Flaky(Arc<AtomicBool>);
        impl ExecutableNode for Flaky {
            fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
                if self.0.load(Ordering::SeqCst) {
                    return Err(ZeughausError::ExecutionFailed("boom".into()));
                }
                ctx.emit_typed("value", 1.0f64);
                ctx.flush();
                Ok(())
            }
            fn pin_definitions(&self) -> &[PinDefinition] {
                &[]
            }
        }

        let mut graph = Graph::new();
        let a = NodeId::next();
        graph.add_node(make_node(a));
        let mut exec = GraphExecutor::new(graph);
        let fail = Arc::new(AtomicBool::new(true));
        exec.register_node(a, Box::new(Flaky(fail.clone())));

        assert!(exec.execute_all().is_err());
        assert!(exec.is_error(a));

        // Recover: node now succeeds, error flag clears.
        fail.store(false, Ordering::SeqCst);
        exec.mark_dirty_downstream(a);
        exec.execute_dirty().unwrap();
        assert!(!exec.is_error(a));
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

    #[test]
    fn disconnect_edge_clears_cache() {
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

        // Edge cache should have the value
        assert!(exec.edge_value(edge_id).is_some());

        // Disconnect the edge properly
        exec.disconnect_edge(edge_id);

        // Cache entry must be gone
        assert!(exec.edge_value(edge_id).is_none());

        // Re-execute: b should get default input (0.0) -> output 0.0
        exec.execute_dirty().unwrap();
    }

    #[test]
    fn remove_node_cleans_up_cache() {
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

        assert!(exec.edge_value(edge_id).is_some());

        exec.remove_node(a);

        // Cache for the removed edge must be gone
        assert!(exec.edge_value(edge_id).is_none());
        // Graph should only have node b
        assert_eq!(exec.graph.node_count(), 1);
    }

    /// A converter coerces a value as it crosses an edge whose endpoints
    /// declare different types (u8 source -> f64 input).
    #[test]
    fn converter_coerces_value_across_edge() {
        struct ConstU8(u8);
        impl ExecutableNode for ConstU8 {
            fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
                ctx.emit_typed("value", self.0);
                ctx.flush();
                Ok(())
            }
            fn pin_definitions(&self) -> &[PinDefinition] {
                &[]
            }
        }

        fn typed_node(id: NodeId, pin: &'static str, dir: PinDirection, ty: &'static str) -> GraphNode {
            GraphNode {
                id,
                type_id: "test".to_string(),
                config: NodeConfig::default(),
                pin_defs: vec![PinDefinition {
                    name: pin,
                    direction: dir,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: ty,
                }],
                position: (0.0, 0.0),
            }
        }

        let mut graph = Graph::new();
        let a = NodeId::next();
        let b = NodeId::next();
        let c = NodeId::next();
        graph.add_node(typed_node(a, "value", PinDirection::Output, "u8"));
        graph.add_node(typed_node(b, "in", PinDirection::Input, "f64"));
        graph.add_node(make_node(c));
        graph.add_edge(make_edge(a, "value", b, "in"));
        let bc = make_edge(b, "out", c, "in");
        let bc_id = bc.id;
        graph.add_edge(bc);

        let mut conv = TypeConverters::new();
        conv.register_typed::<u8, f64, _>("u8", "f64", |x| x as f64);

        let mut exec = GraphExecutor::new(graph);
        exec.set_converters(Arc::new(conv));
        exec.register_node(a, Box::new(ConstU8(7)));
        exec.register_node(b, Box::new(DoubleNode)); // reads in:f64, emits out = in*2
        exec.register_node(c, Box::new(DoubleNode));
        exec.execute_all().unwrap();

        // Coercion u8(7) -> f64(7.0); DoubleNode emits 14.0 onto b->c.
        let val = exec.edge_value(bc_id).unwrap();
        assert_eq!(val.downcast_ref::<f64>(), Some(&14.0));
    }
}
