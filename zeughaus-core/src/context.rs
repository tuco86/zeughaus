use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::error::Result;
use crate::id::NodeId;
use crate::ty::Typed;
use crate::value::Value;

/// Off-thread work a node defers instead of producing outputs synchronously.
/// A node calls `NodeContext::defer` during `execute()`; the executor hands the
/// work to the host, which runs `run()` on a background thread and feeds the
/// result back via `deliver_async_result`. This keeps blocking work (e.g. an
/// LLM HTTP request) off the UI/executor thread.
pub trait AsyncWork: Send + 'static {
    /// Runs the blocking work and returns the node's output pin values
    /// (pin name -> value), to be applied as if the node had emitted them.
    fn run(self: Box<Self>) -> Result<HashMap<String, Value>>;
}

/// Read-only view of a node's inputs during execution.
pub struct InputSet {
    values: HashMap<Arc<str>, Value>,
    /// Pins a value was delivered on since this node last ran.
    ///
    /// Dirty propagation is uniform -- every downstream node reruns, which is
    /// what an LLM or ML chain depends on -- so the values alone cannot say
    /// what just happened. A node that must act only when its own trigger
    /// fired (an insert, a query) asks this instead of comparing values, which
    /// would fire again on an unchanged one and never on a repeated one.
    changed: HashSet<Arc<str>>,
}

impl InputSet {
    pub fn new() -> Self {
        Self {
            values: HashMap::new(),
            changed: HashSet::new(),
        }
    }

    pub fn insert(&mut self, pin_name: impl Into<Arc<str>>, value: Value) {
        self.values.insert(pin_name.into(), value);
    }

    /// Records that the value on `pin` was delivered since the last execution.
    pub fn mark_changed(&mut self, pin: impl Into<Arc<str>>) {
        self.changed.insert(pin.into());
    }

    /// Whether a value was delivered on `pin` since this node last ran.
    pub fn changed(&self, pin: &str) -> bool {
        self.changed.contains(pin)
    }

    pub fn get<T: Clone + 'static>(&self, pin_name: &str) -> Option<T> {
        self.values.get(pin_name)?.downcast_ref::<T>().cloned()
    }

    pub fn get_value(&self, pin_name: &str) -> Option<&Value> {
        self.values.get(pin_name)
    }

    pub fn has(&self, pin_name: &str) -> bool {
        self.values.contains_key(pin_name)
    }
}

impl Default for InputSet {
    fn default() -> Self {
        Self::new()
    }
}

/// Mutable context passed to nodes during execution.
/// Supports the emit/flush pattern for atomic multi-output delivery.
pub struct NodeContext {
    buffered: HashMap<String, Value>,
    flushed: HashMap<String, Value>,
    deferred: Option<Box<dyn AsyncWork>>,
    /// The node being executed. A value that carries an identity across
    /// nodes (an ML step) takes it from here, so two branches of one graph
    /// never mint the same id.
    pub source_node: NodeId,
}

impl NodeContext {
    pub fn new(source_node: NodeId) -> Self {
        Self {
            buffered: HashMap::new(),
            flushed: HashMap::new(),
            deferred: None,
            source_node,
        }
    }

    /// Defer blocking work to a background thread instead of emitting outputs
    /// now. The node should return Ok without flushing; its outputs arrive
    /// later via the host's async result delivery.
    pub fn defer(&mut self, work: Box<dyn AsyncWork>) {
        self.deferred = Some(work);
    }

    /// Called by the executor after execute() to retrieve any deferred work.
    pub fn take_deferred(&mut self) -> Option<Box<dyn AsyncWork>> {
        self.deferred.take()
    }

    /// Buffer a value for the named output pin. Not visible to downstream until flush().
    pub fn emit(&mut self, pin_name: &str, value: Value) {
        self.buffered.insert(pin_name.to_string(), value);
    }

    /// Convenience: emit a typed value.
    pub fn emit_typed<T: Typed>(&mut self, pin_name: &str, val: T) {
        self.emit(pin_name, Value::new(val));
    }

    /// Release all buffered outputs atomically.
    pub fn flush(&mut self) {
        self.flushed.extend(self.buffered.drain());
    }

    /// Called by the executor after execute() returns Ok to retrieve flushed outputs.
    pub fn take_outputs(&mut self) -> HashMap<String, Value> {
        std::mem::take(&mut self.flushed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_set_round_trip() {
        let mut inputs = InputSet::new();
        inputs.insert("a", Value::new(1.0f64));
        assert_eq!(inputs.get::<f64>("a"), Some(1.0));
        assert!(inputs.has("a"));
        assert!(!inputs.has("b"));
        assert_eq!(inputs.get::<String>("a"), None);
    }

    #[test]
    fn input_set_takes_runtime_pin_names() {
        // Pin names are not always literals: a variadic node or a schema-derived
        // node builds them.
        let mut inputs = InputSet::new();
        let name: Arc<str> = format!("in{}", 3).into();
        inputs.insert(name.clone(), Value::new(2.0f64));
        assert_eq!(inputs.get::<f64>("in3"), Some(2.0));
        assert!(inputs.get_value(&name).is_some());
    }

    #[test]
    fn emit_is_invisible_until_flush() {
        let mut ctx = NodeContext::new(NodeId(1));
        ctx.emit_typed("out", 1.0f64);
        assert!(ctx.take_outputs().is_empty());
        ctx.emit_typed("out", 2.0f64);
        ctx.flush();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs["out"].downcast_ref::<f64>(), Some(&2.0));
    }

    #[test]
    fn flush_releases_all_pins_together() {
        let mut ctx = NodeContext::new(NodeId(1));
        ctx.emit_typed("a", 1.0f64);
        ctx.emit_typed("b", "x".to_string());
        ctx.flush();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs.len(), 2);
    }

    #[test]
    fn deferred_work_is_taken_once() {
        struct Noop;
        impl AsyncWork for Noop {
            fn run(self: Box<Self>) -> Result<HashMap<String, Value>> {
                Ok(HashMap::new())
            }
        }
        let mut ctx = NodeContext::new(NodeId(1));
        ctx.defer(Box::new(Noop));
        assert!(ctx.take_deferred().is_some());
        assert!(ctx.take_deferred().is_none());
    }
}
