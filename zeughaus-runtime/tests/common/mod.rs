//! Assembling a graph from a plugin catalog, the way a test needs it.
//!
//! The host does this from store rows: it creates the node instance from its
//! plugins and hands it to [`GraphExecutor::add_node`]. A test has no store,
//! so it names node types and pins directly and this helper does the rest.

use std::sync::Arc;

use zeughaus_core::{DomainPlugin, EdgeId, NodeId, Result, TypeConverters, ZeughausError};
use zeughaus_runtime::GraphExecutor;

pub struct GraphBuilder {
    executor: GraphExecutor,
    plugins: Vec<Box<dyn DomainPlugin>>,
}

impl GraphBuilder {
    pub fn new() -> Self {
        Self {
            executor: GraphExecutor::new(Arc::new(TypeConverters::with_builtins())),
            plugins: Vec::new(),
        }
    }

    pub fn register_plugin(&mut self, plugin: Box<dyn DomainPlugin>) {
        self.plugins.push(plugin);
    }

    /// Creates a node of `type_id` from the registered plugins.
    pub fn add_node(&mut self, type_id: &str) -> Result<NodeId> {
        let node = self
            .plugins
            .iter()
            .find_map(|p| p.create_node(type_id))
            .ok_or_else(|| ZeughausError::UnknownNodeType(type_id.to_string()))?;
        let id = NodeId::next();
        self.executor.add_node(id, type_id, node);
        Ok(id)
    }

    pub fn connect(&mut self, from: NodeId, from_pin: &str, to: NodeId, to_pin: &str) -> EdgeId {
        let id = EdgeId::next();
        self.executor
            .add_edge(id, from, from_pin.into(), to, to_pin.into());
        id
    }

    pub fn build(self) -> GraphExecutor {
        self.executor
    }
}

impl Default for GraphBuilder {
    fn default() -> Self {
        Self::new()
    }
}
