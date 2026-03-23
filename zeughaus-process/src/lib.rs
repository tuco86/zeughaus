pub mod nodes;

use zeughaus_core::*;

use nodes::*;

pub struct ProcessPlugin;

impl DomainPlugin for ProcessPlugin {
    fn name(&self) -> &str {
        "process"
    }

    fn node_catalog(&self) -> Vec<NodeDefinition> {
        vec![
            catalog_entry("process.find", "Find Process", "Process", &FindProcessNode::new()),
            catalog_entry("process.find_module", "Find Module", "Process", &FindModuleNode::new()),
            catalog_entry("process.dll_inject", "DLL Inject", "Process", &DllInjectNode::new()),
            catalog_entry("process.read_memory", "Read Memory", "Process", &ReadMemoryNode::new()),
        ]
    }

    fn create_node(&self, type_id: &str) -> Option<Box<dyn ExecutableNode>> {
        match type_id {
            "process.find" => Some(Box::new(FindProcessNode::new())),
            "process.find_module" => Some(Box::new(FindModuleNode::new())),
            "process.dll_inject" => Some(Box::new(DllInjectNode::new())),
            "process.read_memory" => Some(Box::new(ReadMemoryNode::new())),
            _ => None,
        }
    }
}

fn catalog_entry(
    type_id: &'static str,
    display_name: &'static str,
    category: &'static str,
    node: &dyn ExecutableNode,
) -> NodeDefinition {
    NodeDefinition {
        type_id,
        display_name,
        category,
        pins: node.pin_definitions().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_all_nodes() {
        let plugin = ProcessPlugin;
        assert_eq!(plugin.node_catalog().len(), 4);
    }

    #[test]
    fn create_all_catalog_nodes() {
        let plugin = ProcessPlugin;
        for def in plugin.node_catalog() {
            assert!(
                plugin.create_node(def.type_id).is_some(),
                "Failed to create: {}",
                def.type_id
            );
        }
    }
}
