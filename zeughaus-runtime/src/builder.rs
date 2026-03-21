use std::collections::HashMap;

use zeughaus_core::*;

use crate::executor::GraphExecutor;
use crate::graph::{Graph, GraphEdge, GraphNode};

pub struct GraphBuilder {
    graph: Graph,
    plugins: Vec<Box<dyn DomainPlugin>>,
    executables: HashMap<NodeId, Box<dyn ExecutableNode>>,
}

impl GraphBuilder {
    pub fn new() -> Self {
        Self {
            graph: Graph::new(),
            plugins: Vec::new(),
            executables: HashMap::new(),
        }
    }

    pub fn register_plugin(&mut self, plugin: Box<dyn DomainPlugin>) {
        self.plugins.push(plugin);
    }

    pub fn add_node(
        &mut self,
        type_id: &str,
        position: (f32, f32),
    ) -> Result<NodeId> {
        let exec = self
            .plugins
            .iter()
            .find_map(|p| p.create_node(type_id))
            .ok_or_else(|| ZeughausError::UnknownNodeType(type_id.to_string()))?;

        let id = NodeId::next();
        let pin_defs = exec.pin_definitions().to_vec();

        self.graph.add_node(GraphNode {
            id,
            type_id: type_id.to_string(),
            config: NodeConfig::default(),
            pin_defs,
            position,
        });

        self.executables.insert(id, exec);
        Ok(id)
    }

    pub fn connect(
        &mut self,
        from: NodeId,
        from_pin: &'static str,
        to: NodeId,
        to_pin: &'static str,
    ) -> Result<EdgeId> {
        let edge_id = EdgeId::next();
        self.graph.add_edge(GraphEdge {
            id: edge_id,
            from_node: from,
            from_pin,
            to_node: to,
            to_pin,
            semantic: EdgeSemantic::default(),
        });
        Ok(edge_id)
    }

    pub fn build(self) -> Result<GraphExecutor> {
        // Validate no cycles before building
        crate::topo::topological_sort(&self.graph)?;

        let mut executor = GraphExecutor::new(self.graph);
        for (id, exec) in self.executables {
            executor.register_node(id, exec);
        }
        Ok(executor)
    }
}

impl Default for GraphBuilder {
    fn default() -> Self {
        Self::new()
    }
}
