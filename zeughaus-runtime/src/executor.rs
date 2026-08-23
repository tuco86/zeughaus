use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use zeughaus_core::{
    AsyncWork, EdgeId, ExecutableNode, InputSet, NodeContext, NodeId, PinBinding, PinDefinition,
    Result, Ty, TypeConverters, Value,
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
    /// Why a node's last execution failed (sync error or async failure), keyed
    /// by node. Cleared when the node next runs cleanly. Drives error styling in
    /// the editor and the message it shows.
    node_errors: HashMap<NodeId, String>,
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
            node_errors: HashMap::new(),
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
        self.node_errors.contains_key(&id)
    }

    /// Why a node's last execution failed, if it did.
    pub fn node_error(&self, id: NodeId) -> Option<&str> {
        self.node_errors.get(&id).map(String::as_str)
    }

    /// Every node that failed in the last pass, in no particular order.
    pub fn errors(&self) -> impl Iterator<Item = (NodeId, &str)> {
        self.node_errors.iter().map(|(id, msg)| (*id, msg.as_str()))
    }

    /// Flags a node as failed (used by the host when async work errors).
    pub fn mark_error(&mut self, id: NodeId, message: String) {
        self.pending.remove(&id);
        self.node_errors.insert(id, message);
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
    ///
    /// A node that fails does not cancel the pass: its error is recorded (see
    /// [`Self::node_error`]), its downstream is held back because its outputs
    /// are unavailable, and every unrelated node still runs. Bailing out
    /// instead used to throw away the deferred work already collected in this
    /// pass while those nodes stayed marked pending -- one broken node left
    /// every async node in the graph hanging forever. `Err` is reserved for a
    /// failure of the pass itself, i.e. a graph that cannot be ordered.
    pub fn execute_dirty(&mut self) -> Result<DeferredWork> {
        let order = topological_sort(&self.graph)?;
        let dirty = std::mem::take(&mut self.dirty);
        let mut deferred: DeferredWork = Vec::new();
        // Nodes whose outputs are not (yet) available this pass: deferred this
        // pass, already pending, failed, or transitively downstream of any.
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
                self.node_errors.insert(node_id, e.to_string());
                blocked.insert(node_id);
                continue;
            }

            if let Some(work) = ctx.take_deferred() {
                self.pending.insert(node_id);
                self.node_errors.remove(&node_id); // now retrying
                blocked.insert(node_id);
                deferred.push((node_id, work));
                continue;
            }

            let outputs = ctx.take_outputs();
            self.node_errors.remove(&node_id); // executed cleanly
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
                let to_type = self.pin_type(node_id, &edge.to_pin);
                let value = self.coerce(to_type, value, node_id, &edge.to_pin);
                inputs.insert(Arc::clone(&edge.to_pin), value);
            }
        }
        inputs
    }

    /// Declared type of a node's pin, if the node and pin are known.
    fn pin_type(&self, node: NodeId, pin: &str) -> Option<&Ty> {
        self.graph
            .node(node)?
            .pin_defs
            .iter()
            .find(|p| &*p.name == pin)
            .map(|p| &p.ty)
    }

    /// Coerces a value crossing an edge to the target pin's declared type.
    ///
    /// The source type is the value's own tag rather than the source pin's
    /// declaration: a value knows what it is, and a pin declared `any` (e.g.
    /// `flow.hold`) carries whatever was latched. If the types match, or the
    /// target accepts anything, the value passes through. Otherwise a registered
    /// converter is applied; a missing converter for a genuine mismatch is
    /// logged and the value passed through unchanged.
    fn coerce(
        &self,
        to_type: Option<&Ty>,
        value: &Value,
        node_id: NodeId,
        to_pin: &str,
    ) -> Value {
        let Some(to) = to_type else {
            return value.clone();
        };
        let from = value.ty();
        if from == to || to.is_any() || from.is_any() {
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
        self.node_errors.remove(&node_id);
        self.last_outputs.insert(node_id, outputs.clone());
        self.apply_outputs(node_id, outputs);
        // Re-run only the downstream; the node itself is already done.
        for downstream in self.graph.downstream(node_id) {
            self.dirty.insert(downstream);
        }
        self.execute_dirty()
    }

    /// Adopts output values produced by another window's runtime.
    ///
    /// Exactly one window owns the graph and executes it; every other window is
    /// a viewer and reaches this method instead of running the node. That is the
    /// point: executing locally is precisely what this replaces, so a node with
    /// side effects (a screen capture, an LLM request) fires once for the whole
    /// session rather than once per open window. Hence nothing is marked dirty,
    /// no pending state is touched and no node runs here -- the outputs are
    /// simply recorded and pushed into the outgoing edge caches, so downstream
    /// nodes and [`Self::output_value`] see them exactly as if they had been
    /// computed here.
    ///
    /// A partial output set is normal, not an error: only scalars are
    /// replicated, so pins carrying frames or plugin-owned types are absent
    /// from `outputs`. Such a pin then has no value and the editor dims it,
    /// which is the honest rendering of "the owner did not publish this".
    pub fn set_remote_outputs(&mut self, node: NodeId, outputs: HashMap<String, Value>) {
        self.last_outputs.insert(node, outputs.clone());
        self.apply_outputs(node, outputs);
    }

    /// Clears a node's pending state without delivering outputs (e.g. after the
    /// async work failed). Downstream nodes stay unexecuted until re-triggered.
    pub fn clear_pending(&mut self, node_id: NodeId) {
        self.pending.remove(&node_id);
    }

    fn apply_outputs(&mut self, node_id: NodeId, outputs: HashMap<String, Value>) {
        for &edge_id in self.graph.outgoing_edges(node_id) {
            if let Some(edge) = self.graph.edge(edge_id)
                && let Some(value) = outputs.get(&*edge.from_pin)
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
        let from_pin = Arc::clone(&edge.from_pin);
        let to_node = edge.to_node;

        if let Some(value) = self
            .last_outputs
            .get(&from_node)
            .and_then(|outs| outs.get(&*from_pin))
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
        self.node_errors.remove(&id);
    }

    pub fn edge_value(&self, edge_id: EdgeId) -> Option<&Value> {
        self.cache.get(edge_id)
    }

    /// The value a node produced on one of its output pins during its last
    /// execution, independent of any edge.
    ///
    /// `last_outputs` is replaced wholesale for a node every time it runs
    /// (`execute_dirty` / `deliver_async_result` both `insert` the full output
    /// map), so `None` means "the last run of this node produced no value on
    /// that pin" -- not "never produced one". That is precisely the state the
    /// editor dims. Contrast [`Self::edge_value`], which is a cache holding the
    /// last value that ever crossed an edge and therefore keeps showing a stale
    /// value after a run that emitted nothing.
    pub fn output_value(&self, node: NodeId, pin: &str) -> Option<&Value> {
        self.last_outputs.get(&node)?.get(pin)
    }

    /// Nodes that asked to be run on a clock, with the interval each wants.
    ///
    /// The host does the scheduling, because a node that slept would block the
    /// pass it runs in. Without this a source node -- a screen capture, a sensor
    /// poll -- produces one value and then never again: nothing upstream ever
    /// marks it dirty.
    pub fn clocked_nodes(&self) -> impl Iterator<Item = (NodeId, std::time::Duration)> + '_ {
        self.nodes
            .iter()
            .filter_map(|(id, node)| node.tick_interval().map(|interval| (*id, interval)))
    }

    /// Recomputes a node's pins from what is currently connected to its inputs.
    /// On a change, updates the graph node's pin_defs and returns the new pin
    /// set so the editor can re-sync its own snapshot. Returns None if unchanged.
    ///
    /// The bindings are derived here rather than passed in: the executor already
    /// holds the edges, the pin declarations and the cached values, so it is the
    /// only place where a pin's incoming type is known without guessing.
    pub fn sync_node_pins(&mut self, id: NodeId) -> Option<Vec<PinDefinition>> {
        let bindings = self.input_bindings(id);
        let borrowed: Vec<PinBinding<'_>> = bindings
            .iter()
            .map(|(name, ty)| PinBinding { name, ty })
            .collect();
        let changed = self.nodes.get_mut(&id)?.sync_pins(&borrowed);
        if !changed {
            return None;
        }
        let pins = self.nodes.get(&id)?.pin_definitions().to_vec();
        if let Some(n) = self.graph.node_mut(id) {
            n.pin_defs = pins.clone();
        }
        Some(pins)
    }

    /// What is connected to each of a node's input pins: the pin name plus the
    /// type actually arriving there (the cached value's own type when there is
    /// one, otherwise the source pin's declaration).
    fn input_bindings(&self, id: NodeId) -> Vec<(Arc<str>, Ty)> {
        self.graph
            .incoming_edges(id)
            .iter()
            .filter_map(|&edge_id| {
                let edge = self.graph.edge(edge_id)?;
                let ty = match self.cache.get(edge_id) {
                    Some(value) => value.ty().clone(),
                    None => self
                        .pin_type(edge.from_node, &edge.from_pin)
                        .cloned()
                        .unwrap_or(Ty::Any),
                };
                Some((Arc::clone(&edge.to_pin), ty))
            })
            .collect()
    }

    pub fn set_parameter(&mut self, id: NodeId, name: &str, value: Value) -> Result<()> {
        if let Some(node) = self.nodes.get_mut(&id) {
            node.set_parameter(name, value)?;
            self.mark_dirty_downstream(id);
        }
        Ok(())
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

    fn make_edge(from: NodeId, from_pin: &str, to: NodeId, to_pin: &str) -> GraphEdge {
        GraphEdge {
            id: EdgeId::next(),
            from_node: from,
            from_pin: from_pin.into(),
            to_node: to,
            to_pin: to_pin.into(),
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
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

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
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

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

        // The pass itself succeeds; the failure is recorded on the node.
        assert!(exec.execute_all().is_ok());
        assert!(exec.is_error(a));
        assert_eq!(exec.node_error(a), Some("node execution failed: boom"));

        // Recover: node now succeeds, error flag clears.
        fail.store(false, Ordering::SeqCst);
        exec.mark_dirty_downstream(a);
        exec.execute_dirty().unwrap();
        assert!(!exec.is_error(a));
    }

    /// A failing node used to abort the whole pass, which threw away deferred
    /// work already collected from unrelated nodes -- those nodes stayed marked
    /// pending with nothing running, so their downstream never resumed.
    #[test]
    fn a_failing_node_does_not_discard_another_nodes_deferred_work() {
        struct Failing;
        impl ExecutableNode for Failing {
            fn execute(&mut self, _inputs: &InputSet, _ctx: &mut NodeContext) -> Result<()> {
                Err(ZeughausError::ExecutionFailed("boom".into()))
            }
            fn pin_definitions(&self) -> &[PinDefinition] {
                &[]
            }
        }

        let mut graph = Graph::new();
        // The failing node is ordered first (topological sort breaks ties by id),
        // so it is the one that used to abort before the deferring node ran.
        let failing = NodeId::next();
        let deferring = NodeId::next();
        graph.add_node(make_node(failing));
        graph.add_node(make_node(deferring));
        let mut exec = GraphExecutor::new(graph);
        exec.register_node(failing, Box::new(Failing));
        exec.register_node(deferring, Box::new(DeferNode(1.0)));

        let deferred = exec.execute_all().unwrap();

        assert_eq!(deferred.len(), 1, "the async work must survive");
        assert_eq!(deferred[0].0, deferring);
        assert!(exec.is_pending(deferring));
        assert!(exec.is_error(failing));
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

    fn typed_node(id: NodeId, pin: &str, dir: PinDirection, ty: Ty) -> GraphNode {
        let pin_def = match dir {
            PinDirection::Input => PinDefinition::input(pin, ty, PinKind::Sample),
            PinDirection::Output => PinDefinition::output(pin, ty),
        };
        GraphNode {
            id,
            type_id: "test".to_string(),
            config: NodeConfig::default(),
            pin_defs: vec![pin_def],
            position: (0.0, 0.0),
        }
    }

    /// A converter coerces a value as it crosses an edge whose endpoints
    /// declare different types (int source -> float input).
    #[test]
    fn converter_coerces_value_across_edge() {
        struct ConstInt(i64);
        impl ExecutableNode for ConstInt {
            fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
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
        let c = NodeId::next();
        graph.add_node(typed_node(a, "value", PinDirection::Output, Ty::Int));
        graph.add_node(typed_node(b, "in", PinDirection::Input, Ty::Float));
        graph.add_node(make_node(c));
        graph.add_edge(make_edge(a, "value", b, "in"));
        let bc = make_edge(b, "out", c, "in");
        let bc_id = bc.id;
        graph.add_edge(bc);

        let mut conv = TypeConverters::new();
        conv.register_typed::<i64, f64, _>(|x| x as f64);

        let mut exec = GraphExecutor::new(graph);
        exec.set_converters(Arc::new(conv));
        exec.register_node(a, Box::new(ConstInt(7)));
        exec.register_node(b, Box::new(DoubleNode)); // reads in:f64, emits out = in*2
        exec.register_node(c, Box::new(DoubleNode));
        exec.execute_all().unwrap();

        // Coercion int(7) -> float(7.0); DoubleNode emits 14.0 onto b->c.
        let val = exec.edge_value(bc_id).unwrap();
        assert_eq!(val.downcast_ref::<f64>(), Some(&14.0));
    }

    /// Coercion keys on the value's own type, not on the source pin's
    /// declaration: a pin declared `any` (e.g. `flow.hold`) still lands
    /// correctly on a typed input.
    #[test]
    fn coercion_uses_the_value_type_not_the_declared_source() {
        struct ConstInt;
        impl ExecutableNode for ConstInt {
            fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
                ctx.emit_typed("value", 21i64);
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
        let c = NodeId::next();
        // Source declares `any` while actually emitting an int.
        graph.add_node(typed_node(a, "value", PinDirection::Output, Ty::Any));
        graph.add_node(typed_node(b, "in", PinDirection::Input, Ty::Float));
        graph.add_node(make_node(c));
        graph.add_edge(make_edge(a, "value", b, "in"));
        let bc = make_edge(b, "out", c, "in");
        let bc_id = bc.id;
        graph.add_edge(bc);

        let mut exec = GraphExecutor::new(graph);
        exec.set_converters(Arc::new(TypeConverters::with_builtins()));
        exec.register_node(a, Box::new(ConstInt));
        exec.register_node(b, Box::new(DoubleNode));
        exec.register_node(c, Box::new(DoubleNode));
        exec.execute_all().unwrap();

        assert_eq!(
            exec.edge_value(bc_id).unwrap().downcast_ref::<f64>(),
            Some(&42.0)
        );
    }

    /// The point of the runtime type system: a node whose interface is derived
    /// from a type nobody wrote in Rust. `SplitRecord` grows one output pin per
    /// field of whatever record arrives on its input.
    #[test]
    fn node_derives_its_pins_from_an_incoming_record_type() {
        struct SplitRecord {
            pins: Vec<PinDefinition>,
        }
        impl ExecutableNode for SplitRecord {
            fn execute(&mut self, _inputs: &InputSet, _ctx: &mut NodeContext) -> Result<()> {
                Ok(())
            }
            fn pin_definitions(&self) -> &[PinDefinition] {
                &self.pins
            }
            fn sync_pins(&mut self, connected: &[PinBinding<'_>]) -> bool {
                let mut pins = vec![PinDefinition::input("row", Ty::Any, PinKind::Trigger)];
                if let Some(Ty::Record(rec)) =
                    connected.iter().find(|b| b.name == "row").map(|b| b.ty)
                {
                    pins.extend(
                        rec.fields
                            .iter()
                            .map(|f| PinDefinition::output(Arc::clone(&f.name), f.ty.clone())),
                    );
                }
                let changed = pins != self.pins;
                self.pins = pins;
                changed
            }
        }

        let customer = Ty::record(
            "Customer",
            vec![Field::new("id", Ty::Int), Field::new("name", Ty::Str)],
        );

        let mut graph = Graph::new();
        let source = NodeId::next();
        let split = NodeId::next();
        graph.add_node(typed_node(source, "row", PinDirection::Output, customer));
        graph.add_node(make_node(split));
        graph.add_edge(make_edge(source, "row", split, "row"));

        let mut exec = GraphExecutor::new(graph);
        exec.register_node(
            split,
            Box::new(SplitRecord {
                pins: vec![PinDefinition::input("row", Ty::Any, PinKind::Trigger)],
            }),
        );

        let pins = exec.sync_node_pins(split).expect("pins changed");
        let names: Vec<&str> = pins.iter().map(|p| &*p.name).collect();
        assert_eq!(names, vec!["row", "id", "name"]);
        assert_eq!(pins[1].ty, Ty::Int);
        assert_eq!(pins[2].ty, Ty::Str);
        // The graph's own snapshot is updated too, so the editor redraws them.
        assert_eq!(exec.graph.node(split).unwrap().pin_defs.len(), 3);
        // Idempotent: nothing changed on a second sync.
        assert!(exec.sync_node_pins(split).is_none());
    }

    /// The editor dims a pin whose node produced nothing on it in the last run,
    /// so `output_value` must reflect only the most recent execution.
    #[test]
    fn output_value_reports_only_the_last_run() {
        /// Declares two outputs but emits just one per run, alternating.
        struct Alternating {
            pins: Vec<PinDefinition>,
            runs: u32,
        }
        impl ExecutableNode for Alternating {
            fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
                if self.runs.is_multiple_of(2) {
                    ctx.emit_typed("a", 1.0_f64);
                } else {
                    ctx.emit_typed("b", 2.0_f64);
                }
                self.runs += 1;
                ctx.flush();
                Ok(())
            }
            fn pin_definitions(&self) -> &[PinDefinition] {
                &self.pins
            }
        }

        let mut graph = Graph::new();
        let node = NodeId::next();
        graph.add_node(make_node(node));

        let mut exec = GraphExecutor::new(graph);
        exec.register_node(
            node,
            Box::new(Alternating {
                pins: vec![
                    PinDefinition::output("a", Ty::Float),
                    PinDefinition::output("b", Ty::Float),
                ],
                runs: 0,
            }),
        );

        exec.execute_all().unwrap();
        assert_eq!(
            exec.output_value(node, "a")
                .and_then(Value::downcast_ref::<f64>),
            Some(&1.0)
        );
        // Declared but not emitted this run.
        assert!(exec.output_value(node, "b").is_none());
        assert!(exec.output_value(NodeId::next(), "a").is_none());

        // Second run emits the other pin: the map is replaced, not merged.
        exec.execute_all().unwrap();
        assert!(exec.output_value(node, "a").is_none());
        assert_eq!(
            exec.output_value(node, "b")
                .and_then(Value::downcast_ref::<f64>),
            Some(&2.0)
        );
    }

    /// A viewer window adopts the owner's outputs instead of running the graph.
    #[test]
    fn remote_outputs_are_adopted_without_executing() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

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
        let owner = NodeId::next();
        let sink = NodeId::next();
        graph.add_node(make_node(owner));
        graph.add_node(make_node(sink));
        let edge = make_edge(owner, "value", sink, "in");
        let edge_id = edge.id;
        graph.add_edge(edge);

        let mut exec = GraphExecutor::new(graph);
        let runs = Arc::new(AtomicU32::new(0));
        exec.register_node(owner, Box::new(Counting(5.0, runs.clone())));
        exec.register_node(sink, Box::new(DoubleNode));
        exec.execute_all().unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        let mut outputs = HashMap::new();
        outputs.insert("value".to_string(), Value::new(9.0_f64));
        exec.set_remote_outputs(owner, outputs);

        // Visible to the editor's pin rendering...
        assert_eq!(
            exec.output_value(owner, "value")
                .and_then(Value::downcast_ref::<f64>),
            Some(&9.0)
        );
        // ... and a pin the owner did not publish reads as "no value" (dimmed).
        assert!(exec.output_value(owner, "frame").is_none());
        // ... and available to downstream nodes on the wire's own edge.
        assert_eq!(
            exec.edge_value(edge_id)
                .and_then(Value::downcast_ref::<f64>),
            Some(&9.0)
        );

        // Nothing ran, and nothing was left dirty for the next pass to run:
        // re-executing locally is exactly what adopting the outputs replaces.
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        exec.execute_dirty().unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(exec.pending_count(), 0);
    }
}
