use std::collections::HashMap;
use std::sync::Arc;

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
        from_pin: impl Into<Arc<str>>,
        to: NodeId,
        to_pin: impl Into<Arc<str>>,
    ) -> Result<EdgeId> {
        let edge_id = EdgeId::next();
        self.graph.add_edge(GraphEdge {
            id: edge_id,
            from_node: from,
            from_pin: from_pin.into(),
            to_node: to,
            to_pin: to_pin.into(),
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

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal plugin for testing
    struct TestPlugin;

    struct TestNode;
    impl ExecutableNode for TestNode {
        fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
            ctx.emit_typed("out", 1.0f64);
            ctx.flush();
            Ok(())
        }
        fn pin_definitions(&self) -> &[PinDefinition] {
            &[]
        }
    }

    impl DomainPlugin for TestPlugin {
        fn name(&self) -> &str {
            "test"
        }
        fn node_catalog(&self) -> Vec<NodeDefinition> {
            vec![NodeDefinition {
                type_id: "test.node".into(),
                display_name: "Test".into(),
                category: "Test".into(),
                pins: vec![],
                settings: vec![],
                container: false,
            }]
        }
        fn create_node(&self, type_id: &str) -> Option<Box<dyn ExecutableNode>> {
            if type_id == "test.node" {
                Some(Box::new(TestNode))
            } else {
                None
            }
        }
    }

    #[test]
    fn add_node_returns_unique_ids() {
        let mut b = GraphBuilder::new();
        b.register_plugin(Box::new(TestPlugin));
        let id1 = b.add_node("test.node", (0.0, 0.0)).unwrap();
        let id2 = b.add_node("test.node", (1.0, 0.0)).unwrap();
        assert_ne!(id1, id2);
    }

    #[test]
    fn add_unknown_type_errors() {
        let mut b = GraphBuilder::new();
        b.register_plugin(Box::new(TestPlugin));
        assert!(b.add_node("nonexistent", (0.0, 0.0)).is_err());
    }

    #[test]
    fn build_empty_graph() {
        let b = GraphBuilder::new();
        let exec = b.build().unwrap();
        assert_eq!(exec.graph.node_count(), 0);
    }

    #[test]
    fn build_with_cycle_errors() {
        let mut b = GraphBuilder::new();
        b.register_plugin(Box::new(TestPlugin));
        let a = b.add_node("test.node", (0.0, 0.0)).unwrap();
        let c = b.add_node("test.node", (1.0, 0.0)).unwrap();
        b.connect(a, "out", c, "in").unwrap();
        b.connect(c, "out", a, "in").unwrap();
        assert!(matches!(b.build(), Err(ZeughausError::CycleDetected)));
    }

    #[test]
    fn build_and_execute() {
        let mut b = GraphBuilder::new();
        b.register_plugin(Box::new(TestPlugin));
        let _a = b.add_node("test.node", (0.0, 0.0)).unwrap();
        let mut exec = b.build().unwrap();
        exec.execute_all().unwrap(); // no panic
    }
}
