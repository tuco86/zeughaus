use std::collections::HashMap;

use crate::id::NodeId;
use crate::value::Value;

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
    pub source_node: NodeId,
    pub trace_id: u64,
}

impl NodeContext {
    pub fn new(source_node: NodeId, trace_id: u64) -> Self {
        Self {
            buffered: HashMap::new(),
            flushed: HashMap::new(),
            source_node,
            trace_id,
        }
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
