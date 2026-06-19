use std::collections::HashMap;

use crate::error::Result;
use crate::id::NodeId;
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
    values: HashMap<&'static str, Value>,
}

impl InputSet {
    pub fn new() -> Self {
        Self {
            values: HashMap::new(),
        }
    }

    pub fn insert(&mut self, pin_name: &'static str, value: Value) {
        self.values.insert(pin_name, value);
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
    pub source_node: NodeId,
    pub trace_id: u64,
}

impl NodeContext {
    pub fn new(source_node: NodeId, trace_id: u64) -> Self {
        Self {
            buffered: HashMap::new(),
            flushed: HashMap::new(),
            deferred: None,
            source_node,
            trace_id,
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
    pub fn emit_typed<T: Clone + Send + Sync + 'static>(&mut self, pin_name: &str, val: T) {
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
    fn input_set_get_typed() {
        let mut set = InputSet::new();
        set.insert("a", Value::new(3.0f64));
        assert_eq!(set.get::<f64>("a"), Some(3.0));
        assert_eq!(set.get::<String>("a"), None);
    }

    #[test]
    fn input_set_has() {
        let mut set = InputSet::new();
        set.insert("x", Value::new(1i32));
        assert!(set.has("x"));
        assert!(!set.has("y"));
    }

    #[test]
    fn context_emit_flush() {
        let mut ctx = NodeContext::new(NodeId(1), 0);
        ctx.emit_typed("out_a", 10.0f64);
        ctx.emit_typed("out_b", 20.0f64);

        // Before flush, take_outputs returns nothing
        let pre = ctx.take_outputs();
        assert!(pre.is_empty());

        // Re-emit since take cleared flushed
        ctx.emit_typed("out_a", 10.0f64);
        ctx.emit_typed("out_b", 20.0f64);
        ctx.flush();

        let outputs = ctx.take_outputs();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs["out_a"].downcast_ref::<f64>(), Some(&10.0));
        assert_eq!(outputs["out_b"].downcast_ref::<f64>(), Some(&20.0));
    }

    #[test]
    fn context_emit_without_flush_discards() {
        let mut ctx = NodeContext::new(NodeId(1), 0);
        ctx.emit_typed("out", 42.0f64);
        // No flush -- outputs stay buffered, not flushed
        let outputs = ctx.take_outputs();
        assert!(outputs.is_empty());
    }
}
